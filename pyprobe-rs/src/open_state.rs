//! Module-level server lifecycle and connection registry.
//!
//! `open()` converges the module to "server running, schema compatible": it
//! starts a private `probe-rs serve` when no host is configured, or verifies a
//! remote host. Sessions attach through [`ensure_server_client`]; the weak
//! registry lets `close_all()` and `sessions()` reach live connections.

use parking_lot::Mutex;
use std::sync::{Arc, OnceLock, Weak};

use probe_rs_rpc_client::RpcClient;

use crate::error::BridgeError;
use crate::rpc::{Bridge, Connection};
use crate::server::{ManagerError, ServerManager};

/// Connect-phase timeout covering transport setup and the schema handshake.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

struct OpenState {
    manager: ServerManager,
    host: Option<String>,
    token: Option<String>,
}

static STATE: OnceLock<Mutex<OpenState>> = OnceLock::new();
static SESSIONS: Mutex<Vec<(Weak<Connection>, String)>> = Mutex::new(Vec::new());

fn state() -> &'static Mutex<OpenState> {
    STATE.get_or_init(|| {
        Mutex::new(OpenState {
            manager: ServerManager::new(),
            host: None,
            token: None,
        })
    })
}

/// Remember a live connection with its target name for `sessions()`/`close_all()`.
pub(crate) fn register(conn: &Arc<Connection>, target: String) {
    SESSIONS
        .lock()
        .push((Arc::downgrade(conn), target));
}

/// Prune dead entries and describe the remaining live connections.
pub(crate) fn session_list() -> Vec<String> {
    let mut reg = SESSIONS.lock();
    reg.retain(|(w, _)| w.upgrade().is_some());
    reg.iter()
        .map(|(_, target)| format!("Session(target={target})"))
        .collect()
}

/// Mark every live connection closed so their next call fails fast, then
/// forget them. The transports themselves close when the Python objects are
/// collected; the server also releases probes when clients disconnect.
pub(crate) fn close_all() {
    let mut reg = SESSIONS.lock();
    for (weak, _) in reg.drain(..) {
        if let Some(conn) = weak.upgrade() {
            conn.mark_closed();
        }
    }
}

/// Store the remote mode (host/token) for future attaches; `None` returns to
/// spawned-local mode.
pub(crate) fn set_remote(host: Option<String>, token: Option<String>) {
    let mut s = state().lock();
    s.host = host;
    s.token = token;
}

/// The configured remote host, if remote mode is active.
pub(crate) fn remote_mode() -> Option<String> {
    state().lock().host.clone()
}

/// True when a server is already ensured (remote host configured or a local
/// server is running) — `list_probes` uses the server's view only then.
pub(crate) fn server_active() -> bool {
    if remote_mode().is_some() {
        return true;
    }
    state().lock().manager.socket_connectable()
}

/// Kill the spawned server. Refused for foreign servers and in remote mode.
pub(crate) fn shutdown_server() -> Result<(), BridgeError> {
    let mut s = state().lock();
    if s.host.is_some() {
        return Err(BridgeError::Connection(
            "pyprobe-rs is in remote mode; there is no local server to shut down".into(),
        ));
    }
    if !s.manager.spawned_by_us() {
        return Err(BridgeError::Connection(
            "refusing to shut down a server pyprobe-rs did not spawn; stop that process yourself"
                .into(),
        ));
    }
    s.manager.kill();
    Ok(())
}

/// Ring-buffer snapshot of the spawned server's stdout/stderr.
pub(crate) fn server_logs() -> Vec<String> {
    state().lock().manager.logs()
}

/// Ensure the configured server is running and return a fresh, schema-checked
/// client connection to it. Idempotent; heals a stale own server once.
pub(crate) fn ensure_server_client() -> Result<RpcClient, BridgeError> {
    Bridge::global()?;
    let (host, token) = {
        let s = state().lock();
        (s.host.clone(), s.token.clone())
    };
    match host {
        Some(host) => connect_remote(&host, token.as_deref()),
        None => connect_local(),
    }
}

fn connect_remote(host: &str, token: Option<&str>) -> Result<RpcClient, BridgeError> {
    // Bridge::wait yields the future's own Result; flatten the nesting.
    Bridge::global()?
        .wait(CONNECT_TIMEOUT, "connect", Connection::connect(host, token))
        .and_then(|inner| inner)
}

fn connect_local() -> Result<RpcClient, BridgeError> {
    let url = {
        let mut s = state().lock();
        s.manager.ensure_running().map_err(manager_err)?;
        s.manager.socket_url()
    };
    let bridge = Bridge::global()?;
    match bridge
        .wait(CONNECT_TIMEOUT, "connect", Connection::connect(&url, None))
        .and_then(|inner| inner)
    {
        Ok(client) => Ok(client),
        Err(e @ BridgeError::ServerVersion(_)) => {
            // A schema mismatch on a server we spawned means a stale binary
            // from an older build; restarting picks up the current one.
            if heal_restart().is_ok() {
                bridge
                    .wait(CONNECT_TIMEOUT, "connect", Connection::connect(&url, None))
                    .and_then(|inner| inner)
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Restart a server pyprobe-rs spawned (stale-kernel recovery). Refused for
/// foreign servers.
pub(crate) fn heal_restart() -> Result<(), BridgeError> {
    let mut s = state().lock();
    if !s.manager.spawned_by_us() {
        return Err(BridgeError::ProbeBusy(
            "cannot restart a server pyprobe-rs did not spawn; stop the other client first"
                .into(),
        ));
    }
    s.manager.kill();
    s.manager.ensure_running().map_err(manager_err)?;
    Ok(())
}

/// True when the running server was spawned by this crate and no live
/// connection exists — the preconditions for automatic stale-session healing.
pub(crate) fn own_server_idle() -> bool {
    let s = state().lock();
    if !s.manager.spawned_by_us() {
        return false;
    }
    let reg = SESSIONS.lock();
    reg.iter().all(|(w, _)| w.upgrade().is_none())
}

/// Map a server-manager failure onto the bridge taxonomy.
fn manager_err(e: ManagerError) -> BridgeError {
    use ManagerError as M;
    match e {
        M::BinaryNotFound => BridgeError::Connection(
            "probe-rs binary not found; install probe-rs or point PYPROBE_RS_BIN at it".into(),
        ),
        M::Spawn(e) => BridgeError::Connection(format!("failed to spawn probe-rs serve: {e}")),
        M::ReadinessTimeout => BridgeError::Connection(
            "probe-rs serve did not become ready in time; inspect pyprobe_rs.server_logs()".into(),
        ),
        M::Config(e) => BridgeError::Connection(format!("failed to write the server config: {e}")),
    }
}
