//! RPC bridge between the Python facade and a `probe-rs serve` process.
//!
//! All connections share one dedicated tokio runtime ([`Bridge`]). Sync facade
//! methods run futures on that runtime with the GIL released, so the
//! interpreter stays responsive during flash, resets, and block transfers.
//! Every call is wrapped in a deadline: the postcard-rpc host client waits
//! forever otherwise.

use parking_lot::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use std::path::PathBuf;

use probe_rs_rpc::probe::{AttachRequest, AttachResult};
use probe_rs_rpc_client::{ClientError, RpcClient, SessionInterface};

use crate::error::{BridgeError, Phase, is_fatal_recoverable};
use crate::open_state;

/// Default deadline for bridge calls that pass no explicit timeout.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const USER_AGENT: &str = concat!("pyprobe-rs/", env!("CARGO_PKG_VERSION"));

/// Minimum time a reattach waits for the probe lease. The failed
/// operation's server-side future may still be winding down when the old
/// transport drops; the lease is released once it does.
const REATTACH_LEASE_WAIT: Duration = Duration::from_secs(10);

/// How to rebuild the transport after a dead probe/target link: the same
/// server this connection originally used.
#[derive(Clone)]
pub(crate) enum ConnSource {
    /// Reconnect through the module's server manager (spawned-local mode).
    Local,
    /// Reconnect to the same remote host.
    Remote { host: String, token: Option<String> },
}

/// Shared async runtime for all RPC connections.
pub(crate) struct Bridge {
    rt: tokio::runtime::Runtime,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

impl Bridge {
    /// Process-wide bridge runtime, created on first use and reused across
    /// connections and server restarts.
    pub(crate) fn global() -> Result<&'static Self, BridgeError> {
        if let Some(bridge) = BRIDGE.get() {
            return Ok(bridge);
        }
        // A lost race drops the surplus runtime before it is ever used.
        let _ = BRIDGE.set(Self::new()?);
        Ok(BRIDGE.get().expect("just initialized"))
    }

    /// Run `fut` as a background task on the runtime (event pollers).
    pub(crate) fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        self.rt.spawn(fut);
    }

    #[cfg(test)]
    pub(crate) fn test_enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.rt.enter()
    }

    fn new() -> Result<Self, BridgeError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| {
                BridgeError::Connection(format!("failed to start the async runtime: {e}"))
            })?;
        Ok(Self { rt })
    }

    /// Run `fut` to completion under `timeout`. Must be called with the GIL
    /// released (facade methods use `py.allow_threads`).
    pub(crate) fn wait<F>(
        &self,
        timeout: Duration,
        op: &str,
        fut: F,
    ) -> Result<F::Output, BridgeError>
    where
        F: Future,
    {
        self.rt.block_on(async move {
            tokio::time::timeout(timeout, fut).await.map_err(|_| {
                BridgeError::Timeout(format!("{op} did not complete within {timeout:?}"))
            })
        })
    }
}

/// Await `fut` on the shared runtime (GIL must already be released) and map
/// both transport failures and server-side RPC errors onto [`BridgeError`].
pub(crate) fn rpc_call<F, T>(timeout: Duration, op: &str, fut: F) -> Result<T, BridgeError>
where
    F: Future<Output = Result<T, ClientError>>,
{
    Bridge::global()?
        .wait(timeout, op, fut)
        .and_then(|r| r.map_err(|e| BridgeError::from_client(e, Phase::Call)))
}

/// One RPC connection with an attached session. Severing the inner
/// transport ([`Connection::sever`]) or dropping the last `Arc` closes the
/// wire, which makes the server release the probe lease.
pub(crate) struct Connection {
    /// Transport + attached session; `None` once severed. Handles keep
    /// `Arc<Connection>` alive after `close()`, so the transport must be
    /// droppable independently of this struct's lifetime. Also `None`
    /// transiently while a reattach rebuilds it.
    inner: Mutex<Option<ConnectionInner>>,
    /// Hardware breakpoints set through the facade, per core index. The flash
    /// path re-applies them after a reflash because a reset can clear the
    /// debug unit while our session stays alive.
    pub(crate) breakpoints: Mutex<HashMap<u32, Vec<u64>>>,
    /// Set by [`Connection::sever`]; makes later calls fail fast even while
    /// other Arc clones keep this object alive.
    pub(crate) closed: AtomicBool,
    /// Attach-time dry_run: the session runs on a mocked core, so the flash
    /// path skips the boot step (a boot sequence just times out on it).
    pub(crate) dry_run: bool,
    /// Where to rebuild the transport from after a dead-link failure.
    source: ConnSource,
    /// The attach request this connection was built with; reused by
    /// [`Connection::reattach`] so the rebuilt session keeps the original
    /// attach policy (chip, speed, permissions, dry-run).
    request: AttachRequest,
    /// DWARF file pushed via `load_debug_info`; re-pushed after a reattach
    /// because the debug state lives in the (new) server session.
    pub(crate) debug_info: Mutex<Option<PathBuf>>,
    /// One-shot guard: automatic dead-link recovery already ran.
    recovered: AtomicBool,
}
/// Transport and session, dropped as a unit by `sever`. The client lives
/// only inside the `SessionInterface`; nothing reads it separately.
struct ConnectionInner {
    session: SessionInterface,
}
impl Connection {
    pub(crate) async fn connect(url: &str, token: Option<&str>) -> Result<RpcClient, BridgeError> {
        let client = probe_rs_rpc_client::connect(url, token, USER_AGENT)
            .await
            .map_err(|e| BridgeError::from_client(e, Phase::Connect))?;
        client
            .ensure_compatible()
            .await
            .map_err(|e| BridgeError::from_client(e, Phase::Connect))
    }

    /// Attach a session to the probe described by `request`.
    pub(crate) async fn attach(
        client: RpcClient,
        request: AttachRequest,
        source: ConnSource,
    ) -> Result<Self, BridgeError> {
        let dry_run = request.dry_run;
        let session = Self::attach_session(client.clone(), request.clone()).await?;
        Ok(Self {
            inner: Mutex::new(Some(ConnectionInner { session })),
            breakpoints: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            dry_run,
            source,
            request,
            debug_info: Mutex::new(None),
            recovered: AtomicBool::new(false),
        })
    }

    /// Attach and map the wire result onto a session or a bridge error.
    /// Shared by [`Connection::attach`] and [`Connection::reattach`].
    async fn attach_session(
        client: RpcClient,
        request: AttachRequest,
    ) -> Result<SessionInterface, BridgeError> {
        let result = client
            .attach_probe(request)
            .await
            .map_err(|e| BridgeError::from_client(e, Phase::Call))?;
        match result {
            AttachResult::Success(sessid) => Ok(SessionInterface::new(client, sessid)),
            AttachResult::ProbeInUse { holder } => Err(BridgeError::ProbeBusy(format!(
                "the probe is claimed by another session on this server{}; close \
                 that session first (pyprobe_rs.close_all() closes every session \
                 this process holds; a stale session on a server pyprobe-rs \
                 spawned is restarted automatically on the next attach; if the \
                 server was adopted, pr.shutdown_server(force=True) restarts it)",
                match &holder {
                    Some(holder) => format!(" — holder: {holder}"),
                    None => String::new(),
                }
            ))),
            AttachResult::ProbeNotFound => Err(BridgeError::Attach(
                "no debug probe found; connect one and check pyprobe_rs.list_probes()".into(),
                "Probe",
            )),
            AttachResult::FailedToOpenProbe(msg) => Err(BridgeError::Attach(
                format!("failed to open the probe: {msg}"),
                "Probe",
            )),
            AttachResult::TargetAttachFailed {
                message,
                connect_under_reset,
            } => {
                let hint = if connect_under_reset {
                    " (attach with connect_under_reset may succeed)"
                } else {
                    ""
                };
                Err(BridgeError::Attach(
                    format!("failed to attach to the target: {message}{hint}"),
                    "TargetAttachFailed",
                ))
            }
        }
    }
    /// Borrow the live transport's session. Errors once the connection is
    /// severed; the borrow keeps the transport alive for the call's duration.
    /// Blocking by design: the connection is one wire, and the exclusive
    /// guard held for the call's duration is what serializes requests onto
    /// it — the connection never has two requests in flight.
    pub(crate) fn session(&self) -> Result<SessionRef<'_>, BridgeError> {
        Ok(SessionRef {
            conn: self,
            guard: live_inner(self)?,
        })
    }

    /// Clone the session interface without holding the guard across the
    /// await. For event pollers: blocking on this lock would freeze a
    /// runtime worker for the whole in-flight call (e.g. a flash) — and a
    /// frozen worker can strand the client's dispatch task behind it — so
    /// the caller skips its tick instead. Skipping is also semantically
    /// right: while a flash owns the wire, the target is not available for
    /// side reads.
    pub(crate) fn try_session_clone(&self) -> Result<SessionInterface, SessionTryError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SessionTryError::Closed);
        }
        match self.inner.try_lock() {
            Some(inner) => match inner.as_ref() {
                Some(state) => Ok(state.session.clone()),
                // Transient: a reattach is rebuilding the transport; the
                // next tick observes the new session.
                None => Err(SessionTryError::Unavailable),
            },
            None => Err(SessionTryError::Busy),
        }
    }
    /// Sever the transport: later calls fail fast and the server releases
    /// the probe lease (the RPC session drops with the connection) even
    /// while handles keep this `Arc` alive. Idempotent.
    ///
    /// Never blocks. When a call is in flight it holds the connection's
    /// only guard; the take is then deferred to that call's `SessionRef`
    /// drop, so `close()` is safe from RTT/flash callbacks and the lease
    /// release is bounded by the in-flight call's own deadline.
    pub(crate) fn sever(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut inner) = self.inner.try_lock() {
            inner.take();
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Rebuild the transport and the attached session after a dead
    /// probe/target link (target power cycle, USB replug).
    ///
    /// The old transport is dropped first: the server releases the probe
    /// lease when the client disconnects. The attach then waits for the
    /// lease (`REATTACH_LEASE_WAIT`) because the failed operation's
    /// server-side future may still be winding down. Session-resident state
    /// (DWARF debug info, hardware breakpoints) is restored afterwards; a
    /// failure there is logged, not raised — the retried operation surfaces
    /// what actually matters. `closed` is never touched: event pollers
    /// observe the swapped transport on their next tick. Blocking; run with
    /// the GIL released on a plain thread.
    pub(crate) fn reattach(&self, deadline: Duration) -> Result<(), BridgeError> {
        let mut request = self.request.clone();
        if request
            .wait_for_probe
            .is_none_or(|w| w < REATTACH_LEASE_WAIT)
        {
            request.wait_for_probe = Some(REATTACH_LEASE_WAIT);
        }
        self.inner.lock().take();
        let client = match &self.source {
            ConnSource::Local => open_state::ensure_server_client(),
            ConnSource::Remote { host, token } => {
                open_state::connect_remote(host, token.as_deref())
            }
        }?;
        let session = Bridge::global()?.wait(
            deadline,
            "reattach",
            Self::attach_session(client, request),
        )??;
        *self.inner.lock() = Some(ConnectionInner {
            session: session.clone(),
        });
        if let Some(path) = self.debug_info.lock().clone() {
            let _ = Bridge::global()?.wait(
                deadline.min(DEFAULT_TIMEOUT),
                "reattach: reload debug info",
                session.load_debug_info(path),
            );
        }
        let breakpoints: Vec<(u32, Vec<u64>)> = self
            .breakpoints
            .lock()
            .iter()
            .filter(|(_, addresses)| !addresses.is_empty())
            .map(|(core, addresses)| (*core, addresses.clone()))
            .collect();
        for (core, addresses) in breakpoints {
            let _ = Bridge::global()?.wait(
                DEFAULT_TIMEOUT,
                "reattach: reapply breakpoints",
                session.core(core as usize).set_hw_breakpoints(addresses),
            );
        }
        Ok(())
    }
}

/// Run one facade RPC with one-shot dead-link recovery.
///
/// `call` performs one full blocking attempt (an `rpc_call` whose
/// `conn.session()` temporary holds the wire guard for its duration). When
/// the failure classifies as a recoverable dead link — target power cycle,
/// USB replug ([`is_fatal_recoverable`]) — the session is reattached once
/// and the call retried. Every later dead-link failure surfaces unchanged:
/// one automatic recovery absorbs a transient rig glitch without hiding a
/// persistently broken one. Must run with the GIL released on a plain
/// thread (reattach blocks on the bridge runtime).
pub(crate) fn call_with_recovery<T, M>(
    conn: &Arc<Connection>,
    timeout: Duration,
    op: &str,
    call: M,
) -> Result<T, BridgeError>
where
    M: Fn() -> Result<T, BridgeError>,
{
    match call() {
        Err(BridgeError::Rpc(msg, kind))
            if is_fatal_recoverable(kind, &msg)
                && !conn.closed.load(Ordering::SeqCst)
                && !conn.recovered.swap(true, Ordering::SeqCst) =>
        {
            if let Err(re) = conn.reattach(timeout.max(DEFAULT_TIMEOUT)) {
                return Err(BridgeError::Rpc(
                    format!("{msg}; recovery reattach failed: {re}"),
                    kind,
                ));
            }
            eprintln!("pyprobe-rs: probe link lost during {op}; reattached and retried");
            call()
        }
        other => other,
    }
}

/// Borrow the inner transport or classify a severed connection.
fn live_inner<'a>(
    conn: &'a Connection,
) -> Result<MutexGuard<'a, Option<ConnectionInner>>, BridgeError> {
    let guard = conn.inner.lock();
    if guard.is_none() {
        return Err(if conn.closed.load(Ordering::SeqCst) {
            BridgeError::Connection(
                "this session is closed (Session.close() or pyprobe_rs.close_all() \
                 ran); attach a new session"
                    .into(),
            )
        } else {
            BridgeError::Connection(
                "this session is reattaching after a probe failure; retry \
                 the call in a moment"
                    .into(),
            )
        });
    }
    Ok(guard)
}

/// Why [`Connection::try_session_clone`] returned without a session.
pub(crate) enum SessionTryError {
    /// A call is in flight; retry on the next tick.
    Busy,
    /// The transport is being rebuilt (reattach); retry on the next tick.
    Unavailable,
    /// The connection is severed.
    Closed,
}
/// Borrow of the transport valid for one call's duration. At most one of
/// these exists at a time (the mutex guard is exclusive).
pub(crate) struct SessionRef<'a> {
    conn: &'a Connection,
    guard: MutexGuard<'a, Option<ConnectionInner>>,
}
impl std::ops::Deref for SessionRef<'_> {
    type Target = SessionInterface;
    fn deref(&self) -> &SessionInterface {
        &self
            .guard
            .as_ref()
            .expect("checked at construction")
            .session
    }
}
impl Drop for SessionRef<'_> {
    fn drop(&mut self) {
        // sever() during this call deferred the take to us: dropping the
        // transport ends the wire and releases the probe lease as soon as
        // the in-flight call no longer borrows it.
        if self.conn.closed.load(Ordering::SeqCst) {
            self.guard.take();
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Arc;

    /// A minimal attach request for test connections; never sent anywhere.
    fn test_attach_request() -> AttachRequest {
        AttachRequest {
            chip: Some("test".into()),
            protocol: None,
            probe: probe_rs_rpc::probe::DebugProbeEntry {
                identifier: "test".into(),
                vendor_id: 0,
                product_id: 0,
                interface: None,
                serial_number: "test".into(),
                probe_type: "test".into(),
                inaccessible: false,
            },
            speed: None,
            connect_under_reset: false,
            dry_run: true,
            allow_erase_all: false,
            resume_target: false,
            wait_for_probe: None,
            client: None,
        }
    }
    pub(crate) fn for_tests() -> Arc<Connection> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let _enter = rt.enter();
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let client = RpcClient::new_local_from_wire(tx, rx);
        let session = SessionInterface::new(client, probe_rs_rpc::Key::new());
        Arc::new(Connection {
            inner: Mutex::new(Some(ConnectionInner { session })),
            breakpoints: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            dry_run: true,
            source: ConnSource::Local,
            request: test_attach_request(),
            debug_info: Mutex::new(None),
            recovered: AtomicBool::new(false),
        })
    }
    /// A live connection whose wire ends are exposed: nothing ever replies,
    /// and `out_frames` (`Receiver::try_recv`) observes every frame the
    /// client emits. Built inside the shared bridge runtime so the client's
    /// dispatch task lands there, as in production.
    pub(crate) fn silent_wire() -> (Arc<Connection>, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let bridge = Bridge::global().expect("bridge");
        let _enter = bridge.test_enter();
        // Client → server carries the frames under test; server → client is
        // never fed. Its sender must stay ALIVE (forgotten, not dropped):
        // dropping it EOFs the client's reader task, which tears down the
        // whole client and fails every call instantly.
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let client = RpcClient::new_local_from_wire(out_tx, in_rx);
        std::mem::forget(in_tx);
        let session = SessionInterface::new(client, probe_rs_rpc::Key::new());
        (
            Arc::new(Connection {
                inner: Mutex::new(Some(ConnectionInner { session })),
                breakpoints: Mutex::new(HashMap::new()),
                closed: AtomicBool::new(false),
                dry_run: true,
                source: ConnSource::Local,
                request: test_attach_request(),
                debug_info: Mutex::new(None),
                recovered: AtomicBool::new(false),
            }),
            out_rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support;
    use super::test_support::for_tests;
    use super::{Bridge, SessionTryError};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn severed_connection_fails_fast_with_connection_kind() {
        let conn = for_tests();
        assert!(conn.session().is_ok());
        conn.sever();
        assert!(conn.is_closed());
        // Severing twice must stay idempotent.
        conn.sever();
        let Err(err) = conn.session() else {
            panic!("severed connection must fail session()");
        };
        assert_eq!(err.kind(), "Connection");
    }
    #[test]
    fn bridge_call_times_out_on_silent_wire() {
        let start = std::time::Instant::now();
        let (conn, _tx) = test_support::silent_wire();
        let result = Bridge::global().expect("bridge").wait(
            Duration::from_millis(150),
            "silent poll",
            conn.session()
                .expect("live")
                .poll_rtt_up(probe_rs_rpc::Key::new(), vec![0]),
        );
        let Err(err) = result else {
            panic!("a call on a silent wire must hit its deadline");
        };
        assert_eq!(err.kind(), "Timeout");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn sever_during_in_flight_call_is_bounded_and_defers_release() {
        let (conn, _tx) = test_support::silent_wire();
        let worker = std::thread::spawn({
            let conn = Arc::clone(&conn);
            move || {
                let _guard = conn.session().expect("live");
                std::thread::sleep(Duration::from_millis(400));
            }
        });
        std::thread::sleep(Duration::from_millis(100));
        let t0 = std::time::Instant::now();
        conn.sever();
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "sever must not wait for the in-flight call"
        );
        worker.join().expect("worker");
        // The deferred take runs when the in-flight call's SessionRef drops.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while conn.inner.try_lock().is_some_and(|g| g.is_some()) {
            assert!(
                std::time::Instant::now() < deadline,
                "transport was never released after the in-flight call ended"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn try_session_clone_reflects_state() {
        let (conn, _tx) = test_support::silent_wire();
        assert!(conn.try_session_clone().is_ok());
        {
            let _guard = conn.session().expect("live");
            assert!(matches!(
                conn.try_session_clone(),
                Err(SessionTryError::Busy)
            ));
        }
        conn.sever();
        assert!(matches!(
            conn.try_session_clone(),
            Err(SessionTryError::Closed)
        ));
    }
}
