//! RPC bridge between the Python facade and a `probe-rs serve` process.
//!
//! All connections share one dedicated tokio runtime ([`Bridge`]). Sync facade
//! methods run futures on that runtime with the GIL released, so the
//! interpreter stays responsive during flash, resets, and block transfers.
//! Every call is wrapped in a deadline: the postcard-rpc host client waits
//! forever otherwise.

use std::collections::HashMap;
use parking_lot::{Mutex, MutexGuard};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use probe_rs_rpc::probe::{AttachRequest, AttachResult};
use probe_rs_rpc_client::{ClientError, RpcClient, SessionInterface};

use crate::error::{BridgeError, Phase};

/// Default deadline for bridge calls that pass no explicit timeout.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

const USER_AGENT: &str = concat!("pyprobe-rs/", env!("CARGO_PKG_VERSION"));

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
            tokio::time::timeout(timeout, fut)
                .await
                .map_err(|_| {
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
    /// droppable independently of this struct's lifetime.
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
}

/// Transport and session, dropped as a unit by `sever`. The client lives
/// only inside the `SessionInterface`; nothing reads it separately.
struct ConnectionInner {
    session: SessionInterface,
}
impl Connection {
    pub(crate) async fn connect(
        url: &str,
        token: Option<&str>,
    ) -> Result<RpcClient, BridgeError> {
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
    ) -> Result<Self, BridgeError> {
        let dry_run = request.dry_run;
        let result = client
            .attach_probe(request)
            .await
            .map_err(|e| BridgeError::from_client(e, Phase::Call))?;
        match result {
            AttachResult::Success(sessid) => {
                let session = SessionInterface::new(client.clone(), sessid);
                Ok(Self {
                    inner: Mutex::new(Some(ConnectionInner { session })),
                    breakpoints: Mutex::new(HashMap::new()),
                    closed: AtomicBool::new(false),
                    dry_run,
                })
            }
            AttachResult::ProbeInUse => Err(BridgeError::ProbeBusy(
                "the probe is claimed by another session on this server; close \
                 that session first (pyprobe_rs.close_all() closes every session \
                 this process holds; a stale session on a server pyprobe-rs \
                 spawned is restarted automatically on the next attach; if the \
                 server was adopted, pr.shutdown_server(force=True) restarts it)"
                    .to_string(),
            )),
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
    pub(crate) fn try_session_clone(
        &self,
    ) -> Result<SessionInterface, SessionTryError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SessionTryError::Closed);
        }
        match self.inner.try_lock() {
            Some(inner) => Ok(inner.as_ref().expect("checked at construction").session.clone()),
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
}

/// Borrow the inner transport or classify a severed connection.
fn live_inner<'a>(
    conn: &'a Connection,
) -> Result<MutexGuard<'a, Option<ConnectionInner>>, BridgeError> {
    let guard = conn.inner.lock();
    if guard.is_none() {
        return Err(BridgeError::Connection(
            "this session is closed (Session.close() or pyprobe_rs.close_all() \
             ran); attach a new session"
                .into(),
        ));
    }
    Ok(guard)
}

/// Why [`Connection::try_session_clone`] returned without a session.
pub(crate) enum SessionTryError {
    /// A call is in flight; retry on the next tick.
    Busy,
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
        &self.guard.as_ref().expect("checked at construction").session
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
        })
    }
    /// A live connection whose wire ends are exposed: nothing ever replies,
    /// and `out_frames` (`Receiver::try_recv`) observes every frame the
    /// client emits. Built inside the shared bridge runtime so the client's
    /// dispatch task lands there, as in production.
    pub(crate) fn silent_wire() -> (
        Arc<Connection>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
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
