//! RPC bridge between the Python facade and a `probe-rs serve` process.
//!
//! All connections share one dedicated tokio runtime ([`Bridge`]). Sync facade
//! methods run futures on that runtime with the GIL released, so the
//! interpreter stays responsive during flash, resets, and block transfers.
//! Every call is wrapped in a deadline: the postcard-rpc host client waits
//! forever otherwise.

use std::collections::HashMap;
use parking_lot::Mutex;
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

/// One RPC connection with an attached session. Dropping it closes the
/// transport, which makes the server release the probe lease.
pub(crate) struct Connection {
    client: RpcClient,
    session: SessionInterface,
    /// Hardware breakpoints set through the facade, per core index. The flash
    /// path re-applies them after a reflash because a reset can clear the
    /// debug unit while our session stays alive.
    pub(crate) breakpoints: Mutex<HashMap<u32, Vec<u64>>>,
    /// Set by `Session.close()`/`close_all()`; makes later calls fail fast
    /// even while other Arc clones keep the transport alive.
    pub(crate) closed: AtomicBool,
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
        let result = client
            .attach_probe(request)
            .await
            .map_err(|e| BridgeError::from_client(e, Phase::Call))?;
        match result {
            AttachResult::Success(sessid) => {
                let session = SessionInterface::new(client.clone(), sessid);
                Ok(Self {
                    client,
                    session,
                    breakpoints: Mutex::new(HashMap::new()),
                    closed: AtomicBool::new(false),
                })
            }
            AttachResult::ProbeInUse => Err(BridgeError::ProbeBusy(
                "the probe is claimed by another session on this server; close \
                 that session (pyprobe_rs.close_all() closes ours, \
                 pyprobe_rs.shutdown_server() restarts a server pyprobe-rs \
                 spawned), then attach again"
                    .into(),
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

    pub(crate) fn session(&self) -> &SessionInterface {
        &self.session
    }

    pub(crate) fn client(&self) -> &RpcClient {
        &self.client
    }

    /// Mark this connection unusable; later calls fail with a clear message.
    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}
