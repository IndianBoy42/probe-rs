//! RTT (Real-Time Transfer) channel access over the probe-rs RPC bridge.
//!
//! Attaching creates a server-side RTT client on the `probe-rs serve` process
//! (`create_rtt_client`) and snapshots its channel list (`get_rtt_channels`);
//! every read/write afterwards is one RPC request. Raw target bytes come back
//! as-is — the wire has no decoding stage, so `read_str` applies the lossy
//! UTF-8 the `String` data format implies. The background poller that keeps
//! channels drained while Python runs other code lives in [`crate::events`]
//! and is started via `Rtt.start_streaming()`.

use std::sync::Arc;
use std::time::Duration;

use probe_rs_rpc::rtt_client::{RttChannelMeta, ScanRegion};
use probe_rs_rpc::rtt_config::RttChannelConfig;
use probe_rs_rpc::{Key, RttClient};
use pyo3::prelude::*;

use crate::error::{BridgeError, py_err_with_kind, to_bridge_err};
use crate::events::{ChannelRing, RttPoller, SubscribedChannel};
use crate::rpc::{Connection, DEFAULT_TIMEOUT, rpc_call};
use crate::types::ChannelModeProxy;

/// Bridge deadline for one down-channel write. The server retries the write
/// itself while the target's buffer is full; this only bounds the round trip.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bridge deadline for one direct up-channel poll. Target RAM reads are
/// sub-millisecond; a second covers a busy server.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Down writes keep the old single-attempt contract: non-blocking, returning
/// the bytes the target accepted (`0` disables the server's retry window).
const DOWN_WRITE_ATTEMPT_MS: u32 = 0;

/// Map a bridge failure on an RTT endpoint. These endpoints are RTT-only, so
/// a server-side rejection is an RTT failure: surface it with the historical
/// `Rtt` kind (the wire carries no structured error kind) instead of `Rpc`.
fn rtt_err(e: BridgeError) -> PyErr {
    match e {
        BridgeError::Rpc(msg) => py_err_with_kind(msg, "Rtt"),
        other => to_bridge_err(other),
    }
}

/// The wire cannot read or change channel modes after attach; a mode is
/// applied once by the server-side channel configuration at attach time.
fn live_mode_unsupported() -> PyErr {
    py_err_with_kind(
        "the RPC wire cannot read or change the RTT channel mode after \
         attach; the mode is fixed by the attach-time channel configuration \
         (pyprobe-rs keeps the target's own configuration)"
            .into(),
        "NotImplemented",
    )
}

/// The wire does not report target-side channel buffer sizes.
fn buffer_size_unsupported() -> PyErr {
    py_err_with_kind(
        "the RPC wire does not report the RTT buffer size".into(),
        "NotImplemented",
    )
}

/// Attach to RTT on the session's core, optionally at a known control-block
/// pointer.
///
/// Creates the server-side RTT client — scanning exactly `ptr` when given,
/// all RAM otherwise — and snapshots the channel list. Channels keep the
/// target's own buffering mode: the wire applies channel configuration only
/// at attach time and has no live mode endpoint.
pub(crate) fn attach(conn: Arc<Connection>, core: u32, ptr: Option<u64>) -> PyResult<Rtt> {
    // The wire derives the polled core from the scan region (an exact pointer
    // pins its core, the default RAM scan covers core 0); a `core` index
    // without a pointer has no wire field to land in, so it stays advisory.
    let _ = core;
    let scan = match ptr {
        Some(addr) => ScanRegion::Exact(addr),
        None => ScanRegion::default(),
    };
    // No channel overrides: the default config keeps `mode: None`, i.e. the
    // target's own buffering mode for every channel.
    let default_config = RttChannelConfig::default();

    let (handle, channels) = Python::attach(|py| {
        py.detach(|| -> PyResult<(Key<RttClient>, probe_rs_rpc::rtt_client::RttChannels)> {
            // Two round-trips, both bounded by the default deadline: create
            // the server-side client, then read its channel snapshot. The
            // control-block attach happens server-side inside
            // `get_rtt_channels` (and is retried by every later poll).
            let data = rpc_call(
                DEFAULT_TIMEOUT,
                "rtt attach",
                conn.session()
                    .create_rtt_client(scan, Vec::new(), default_config),
            )
            .map_err(rtt_err)?;
            let channels = rpc_call(
                DEFAULT_TIMEOUT,
                "rtt channels",
                conn.session().get_rtt_channels(data.handle),
            )
            .map_err(rtt_err)?;
            Ok((data.handle, channels))
        })
    })?;

    let state = |metas: &[RttChannelMeta]| -> Vec<ChannelState> {
        metas
            .iter()
            .map(|m| ChannelState {
                number: m.number,
                name: Some(m.name.clone()),
                ring: Arc::new(ChannelRing::new()),
            })
            .collect()
    };

    Ok(Rtt {
        conn,
        rtt: handle,
        up: state(&channels.up),
        down: state(&channels.down),
        poller: None,
    })
}

/// Identity and streaming state of one channel from the attach snapshot.
struct ChannelState {
    number: u32,
    name: Option<String>,
    /// Host-side drop-oldest ring fed by the background poller; always
    /// present so `pop()` works before, during, and after streaming.
    ring: Arc<ChannelRing>,
}

fn channel_info(c: &ChannelState) -> ChannelInfo {
    ChannelInfo {
        number: c.number as usize,
        name: c.name.clone(),
    }
}

/// Snapshot of one RTT channel from attach time.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct ChannelInfo {
    #[pyo3(get)]
    pub number: usize,
    #[pyo3(get)]
    pub name: Option<String>,
}

#[pymethods]
impl ChannelInfo {
    /// Target-side buffer size in bytes.
    ///
    /// The RPC wire does not report channel buffer sizes, so this cannot be
    /// answered from here.
    #[getter]
    fn buffer_size(&self) -> PyResult<usize> {
        Err(buffer_size_unsupported())
    }
}

/// Handle to a server-side RTT client attached to the target's control block.
///
/// Channel names are a snapshot from attach time; if the target
/// re-initializes RTT, attach again via `Session.attach_rtt()` to rescan.
#[pyclass]
pub struct Rtt {
    conn: Arc<Connection>,
    rtt: Key<RttClient>,
    up: Vec<ChannelState>,
    down: Vec<ChannelState>,
    /// Background poller started by `start_streaming`; `None` until then.
    poller: Option<RttPoller>,
}

impl Drop for Rtt {
    fn drop(&mut self) {
        // A dropped Rtt must not leave its poller draining a client nobody
        // can read from anymore.
        if let Some(poller) = &self.poller {
            poller.stop();
        }
    }
}

#[pymethods]
impl Rtt {
    fn up_channels(&self) -> Vec<ChannelInfo> {
        self.up.iter().map(channel_info).collect()
    }

    fn down_channels(&self) -> Vec<ChannelInfo> {
        self.down.iter().map(channel_info).collect()
    }

    fn up_channel(slf: &Bound<'_, Self>, n: usize) -> PyResult<UpChannel> {
        let rtt = slf.borrow();
        if n >= rtt.up.len() {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "up channel {n} out of range (have {})",
                rtt.up.len()
            )));
        }
        drop(rtt);
        Ok(UpChannel {
            rtt: slf.clone().unbind(),
            channel_number: n,
        })
    }

    fn down_channel(slf: &Bound<'_, Self>, n: usize) -> PyResult<DownChannel> {
        let rtt = slf.borrow();
        if n >= rtt.down.len() {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "down channel {n} out of range (have {})",
                rtt.down.len()
            )));
        }
        drop(rtt);
        Ok(DownChannel {
            rtt: slf.clone().unbind(),
            channel_number: n,
        })
    }

    /// Control-block address. The RPC wire does not expose it.
    fn ptr(&self) -> PyResult<u64> {
        Err(py_err_with_kind(
            "the RPC wire does not expose the RTT control block address; \
             pass ptr= to Session.attach_rtt() when you know it"
                .into(),
            "NotImplemented",
        ))
    }

    /// Keep every up channel drained in the background while Python runs
    /// other code: a task on the bridge runtime polls all channels every
    /// `interval_secs` and pushes bytes into per-channel 1 MiB drop-oldest
    /// ring buffers, which `UpChannel.pop()` drains. `callback`, when given,
    /// is called as `callback(channel_number, bytes)` per chunk; exceptions
    /// are printed and never stop the stream. Idempotent while running.
    #[pyo3(signature = (interval_secs=0.02, callback=None))]
    fn start_streaming(
        slf: &Bound<'_, Self>,
        interval_secs: f64,
        callback: Option<Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        if interval_secs <= 0.0 {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "interval_secs must be a positive number of seconds",
            ));
        }
        let mut this = slf.borrow_mut();
        if this.poller.as_ref().is_some_and(RttPoller::is_running) {
            return Ok(());
        }
        let subscribed = this
            .up
            .iter()
            .map(|c| SubscribedChannel {
                number: c.number,
                ring: c.ring.clone(),
            })
            .collect();
        this.poller = Some(RttPoller::start(
            this.conn.clone(),
            this.rtt,
            subscribed,
            Duration::from_secs_f64(interval_secs),
            callback.map(|cb| cb.unbind()),
        )?);
        Ok(())
    }

    /// Stop the background poller (at the next tick). Data already in the
    /// rings stays available via `UpChannel.pop()`; `start_streaming()`
    /// restarts polling.
    fn stop_streaming(&self) {
        if let Some(poller) = &self.poller {
            poller.stop();
        }
    }
}

/// Handle to one up (target → host) RTT channel.
#[pyclass]
pub struct UpChannel {
    rtt: Py<Rtt>,
    channel_number: usize,
}

impl UpChannel {
    /// Connection, server-side RTT client, and channel number for one call.
    fn parts(&self, py: Python<'_>) -> (Arc<Connection>, Key<RttClient>, u32) {
        let rtt = self.rtt.bind(py).borrow();
        (rtt.conn.clone(), rtt.rtt, self.channel_number as u32)
    }
}

#[pymethods]
impl UpChannel {
    /// Read the bytes that arrived since the previous poll.
    ///
    /// One direct RPC poll: the server consumes the target's ring buffer, so
    /// everything pending is returned even beyond `count` — `count` is kept
    /// for signature compatibility, but truncating would lose the remainder
    /// (already consumed server-side). Cap chunk sizes without losing data
    /// via `Rtt.start_streaming()` + `pop()`. Polling here races with a
    /// running background poller: use one consumer per channel.
    #[pyo3(signature = (count=4096))]
    fn read(&self, py: Python<'_>, count: usize) -> PyResult<Vec<u8>> {
        let _ = count;
        let (conn, rtt, channel) = self.parts(py);
        let results = py
            .detach(|| {
                rpc_call(
                    READ_TIMEOUT,
                    "rtt poll",
                    conn.session().poll_rtt_up(rtt, vec![channel]),
                )
            })
            .map_err(rtt_err)?;
        let result = results.into_iter().next().ok_or_else(|| {
            py_err_with_kind("RTT poll returned no result for the channel".into(), "Rpc")
        })?;
        match result.result {
            Ok(data) => Ok(data),
            Err(e) => Err(py_err_with_kind(e.to_string(), "Rtt")),
        }
    }

    /// Read pending data as text (lossy UTF-8 of `read`).
    #[pyo3(signature = (count=4096))]
    fn read_str(&self, py: Python<'_>, count: usize) -> PyResult<String> {
        let data = self.read(py, count)?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    /// Drain the streaming ring buffer filled by `Rtt.start_streaming()`.
    ///
    /// Returns up to `max_bytes` of the oldest buffered bytes; with
    /// `timeout_secs > 0`, waits up to that long for data to arrive (the GIL
    /// is released throughout). An empty result means the ring had nothing
    /// within the deadline.
    #[pyo3(signature = (max_bytes=4096, timeout_secs=0.0))]
    fn pop(&self, py: Python<'_>, max_bytes: usize, timeout_secs: f64) -> PyResult<Vec<u8>> {
        let ring = self
            .rtt
            .bind(py)
            .borrow()
            .up
            .get(self.channel_number)
            .map(|c| c.ring.clone())
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("channel not found"))?;
        let timeout = if timeout_secs > 0.0 {
            Duration::from_secs_f64(timeout_secs)
        } else {
            Duration::ZERO
        };
        Ok(py.detach(|| ring.pop_wait(max_bytes, timeout)))
    }

    fn number(&self) -> usize {
        self.channel_number
    }

    fn name(&self, py: Python<'_>) -> Option<String> {
        self.rtt
            .bind(py)
            .borrow()
            .up
            .get(self.channel_number)
            .and_then(|c| c.name.clone())
    }

    /// Target-side buffer size. The RPC wire does not report it.
    fn buffer_size(&self) -> PyResult<usize> {
        Err(buffer_size_unsupported())
    }

    /// The channel's buffering mode. The RPC wire cannot read it live.
    fn mode(&self) -> PyResult<ChannelModeProxy> {
        Err(live_mode_unsupported())
    }

    /// Set the buffering mode for this channel. The RPC wire has no live
    /// mode-change endpoint: modes are fixed by the attach-time channel
    /// configuration.
    fn set_mode(&self, mode: ChannelModeProxy) -> PyResult<()> {
        let _ = mode;
        Err(live_mode_unsupported())
    }
}

/// Handle to one down (host → target) RTT channel.
#[pyclass]
pub struct DownChannel {
    rtt: Py<Rtt>,
    channel_number: usize,
}

#[pymethods]
impl DownChannel {
    /// Write to the channel; returns how many bytes the target accepted.
    ///
    /// A single non-blocking attempt: depending on the channel's mode, a full
    /// target buffer yields 0 or a partial write — check the returned count.
    fn write(&self, py: Python<'_>, data: &[u8]) -> PyResult<usize> {
        let rtt = self.rtt.bind(py).borrow();
        let (conn, client, channel) = (rtt.conn.clone(), rtt.rtt, self.channel_number as u32);
        drop(rtt);
        let written = py
            .detach(|| {
                rpc_call(
                    WRITE_TIMEOUT,
                    "rtt write",
                    conn.session().send_to_rtt(
                        client,
                        channel,
                        data.to_vec(),
                        DOWN_WRITE_ATTEMPT_MS,
                    ),
                )
            })
            .map_err(rtt_err)?;
        Ok(written as usize)
    }

    /// Write a string to the channel. Returns the number of bytes accepted —
    /// depending on the channel's mode, writes can be partial.
    fn write_str(&self, py: Python<'_>, s: &str) -> PyResult<usize> {
        self.write(py, s.as_bytes())
    }

    fn number(&self) -> usize {
        self.channel_number
    }

    fn name(&self, py: Python<'_>) -> Option<String> {
        self.rtt
            .bind(py)
            .borrow()
            .down
            .get(self.channel_number)
            .and_then(|c| c.name.clone())
    }

    /// Target-side buffer size. The RPC wire does not report it.
    fn buffer_size(&self) -> PyResult<usize> {
        Err(buffer_size_unsupported())
    }
}
