//! Background event pumping (RTT polling) on the shared bridge runtime.
//!
//! The RTT background poller lives here so `rtt_` stays a thin channel API.
//! Polling is a plain task on the bridge runtime: it dies with the
//! connection, so a crashed client can never leave a polling loop stranded
//! on the server.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use probe_rs_rpc_client::ClientError;
use probe_rs_rpc::{Key, RttClient};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use tokio::time::{MissedTickBehavior, timeout};

use crate::error::to_bridge_err;
use crate::rpc::{Bridge, Connection};

/// Host-side ring capacity per streamed channel (drop-oldest).
const RING_CAPACITY: usize = 1024 * 1024;

/// How often a repeating poller failure is reported to stderr.
const WARN_INTERVAL: Duration = Duration::from_secs(5);

/// Wait granularity for [`ChannelRing::pop_wait`].
const POP_WAIT_GRANULARITY: Duration = Duration::from_millis(1);

/// Drop-oldest ring buffer for one streamed up channel.
///
/// The poller pushes raw target bytes; `pop` drains from the front. Once the
/// ring is full the oldest bytes are dropped: streaming is a lossy tap by
/// design, and the poller must never block on a slow consumer.
pub(crate) struct ChannelRing {
    buf: Mutex<VecDeque<u8>>,
}

impl ChannelRing {
    pub(crate) fn new() -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
        }
    }

    /// Append `data`, dropping oldest bytes beyond the ring capacity.
    pub(crate) fn push(&self, data: &[u8]) {
        let mut buf = self.buf.lock();
        if data.len() >= RING_CAPACITY {
            // One chunk larger than the whole ring: keep only its tail.
            buf.clear();
            buf.extend(&data[data.len() - RING_CAPACITY..]);
            return;
        }
        if buf.len() + data.len() > RING_CAPACITY {
            let excess = buf.len() + data.len() - RING_CAPACITY;
            buf.drain(..excess);
        }
        buf.extend(data);
    }

    /// Take up to `max` bytes from the front of the ring.
    pub(crate) fn pop(&self, max: usize) -> Vec<u8> {
        let mut buf = self.buf.lock();
        let take = max.min(buf.len());
        buf.drain(..take).collect()
    }

    /// [`ChannelRing::pop`], waiting up to `timeout` for data to arrive; the
    /// caller keeps the GIL released for the wait. A zero timeout makes a
    /// single attempt.
    pub(crate) fn pop_wait(&self, max: usize, timeout: Duration) -> Vec<u8> {
        if timeout.is_zero() {
            return self.pop(max);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let data = self.pop(max);
            if !data.is_empty() || Instant::now() >= deadline {
                return data;
            }
            std::thread::sleep(POP_WAIT_GRANULARITY);
        }
    }
}

/// One up channel subscribed to the background poller.
pub(crate) struct SubscribedChannel {
    pub(crate) number: u32,
    pub(crate) ring: Arc<ChannelRing>,
}

/// Handle for the background RTT poller of one attached RTT client.
///
/// The task polls all subscribed up channels in one request per tick, pushes
/// new bytes into their rings, and optionally forwards each chunk to a
/// Python callback. It exits when `stop()` is called, when the connection
/// dies, or when the server stops answering — whichever comes first.
pub(crate) struct RttPoller {
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}

impl RttPoller {
    /// Spawn the poller task on the bridge runtime.
    pub(crate) fn start(
        conn: Arc<Connection>,
        rtt: Key<RttClient>,
        channels: Vec<SubscribedChannel>,
        interval: Duration,
        callback: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        Bridge::global()
            .map_err(to_bridge_err)?
            .spawn(poll_loop(
                conn,
                rtt,
                channels,
                interval,
                callback,
                stop.clone(),
                running.clone(),
            ));
        Ok(Self { stop, running })
    }

    /// Ask the task to exit; it stops at its next tick. Data already in the
    /// rings stays available.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the poller task is still alive.
    pub(crate) fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }
}

/// Poll every subscribed channel once per `interval` until asked to stop.
async fn poll_loop(
    conn: Arc<Connection>,
    rtt: Key<RttClient>,
    channels: Vec<SubscribedChannel>,
    interval: Duration,
    callback: Option<Py<PyAny>>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
) {
    // Bridge deadline: scaled with the poll interval (bounded below by 1 s),
    // so a slow server stalls the loop instead of erroring it out.
    let deadline = (interval * 2).max(Duration::from_secs(1));
    let mut tick = tokio::time::interval(interval);
    // A stalled poll must not burst stale ticks; the next poll catches up.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_warn: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        if conn.is_closed() {
            break;
        }
        let numbers: Vec<u32> = channels.iter().map(|c| c.number).collect();
        // `rpc_call` block_on's the bridge runtime, which this task is
        // running on — await the same deadline directly instead.
        let poll = conn.session().poll_rtt_up(rtt, numbers);
        match timeout(deadline, poll).await {
            Ok(Ok(results)) => {
                last_warn = None;
                for result in results {
                    let Ok(data) = result.result else {
                        // Per-channel failure: the server already logged it.
                        continue;
                    };
                    if data.is_empty() {
                        continue;
                    }
                    let Some(channel) = channels.iter().find(|c| c.number == result.channel)
                    else {
                        continue;
                    };
                    channel.ring.push(&data);
                    if let Some(callback) = &callback {
                        deliver(callback, result.channel, &data);
                    }
                }
            }
            // Server-side trouble (e.g. RTT not attached yet): keep polling —
            // the server retries the control-block attach on every request.
            Ok(Err(ClientError::Remote(e))) => {
                warn_later(&mut last_warn, format!("RTT poll failed: {e}"));
            }
            // Transport gone: the loop dies with the connection.
            Ok(Err(e)) => {
                warn_later(&mut last_warn, format!("RTT poller stopping: {e}"));
                break;
            }
            Err(_) => warn_later(
                &mut last_warn,
                format!("RTT poll did not complete within {deadline:?}"),
            ),
        }
    }
    running.store(false, Ordering::Relaxed);
}

/// Forward one chunk to the Python callback. Exceptions are printed and
/// never propagated: a misbehaving callback must not kill the poller.
fn deliver(callback: &Py<PyAny>, channel: u32, data: &[u8]) {
    Python::attach(|py| {
        let chunk = PyBytes::new(py, data);
        if let Err(err) = callback.call1(py, (channel, chunk)) {
            err.print(py);
        }
    });
}

/// Print a poller warning at most once per [`WARN_INTERVAL`].
fn warn_later(last_warn: &mut Option<Instant>, msg: String) {
    let now = Instant::now();
    if last_warn.is_none_or(|t| now.duration_since(t) > WARN_INTERVAL) {
        eprintln!("pyprobe-rs: {msg}");
        *last_warn = Some(now);
    }
}
