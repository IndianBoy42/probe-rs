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
    /// Attaches holding a granted-but-unregistered lease — the automatic
    /// heal must not fire under them.
    attach_inflight: usize,
}

static STATE: OnceLock<Mutex<OpenState>> = OnceLock::new();
static SESSIONS: Mutex<Vec<(Weak<Connection>, String)>> = Mutex::new(Vec::new());

fn state() -> &'static Mutex<OpenState> {
    STATE.get_or_init(|| {
        Mutex::new(OpenState {
            manager: ServerManager::new(),
            host: None,
            token: None,
            attach_inflight: 0,
        })
    })
}

/// Remember a live connection with its target name for `sessions()`/`close_all()`.
pub(crate) fn register(conn: &Arc<Connection>, target: String) {
    let mut reg = SESSIONS.lock();
    prune(&mut reg);
    reg.push((Arc::downgrade(conn), target));
}

/// Prune dead entries and describe the remaining live connections.
pub(crate) fn session_list() -> Vec<String> {
    let mut reg = SESSIONS.lock();
    prune(&mut reg);
    reg.iter()
        .map(|(_, target)| format!("Session(target={target})"))
        .collect()
}

/// Sever every live connection — the transports drop immediately, so the
/// server releases every probe lease even while Session objects (and their
/// handles) are still referenced.
pub(crate) fn close_all() {
    let mut reg = SESSIONS.lock();
    for (weak, _) in reg.iter() {
        if let Some(conn) = weak.upgrade() {
            conn.sever();
        }
    }
    prune(&mut reg);
}

/// Drop registry entries whose connection is gone or severed.
fn prune(reg: &mut Vec<(Weak<Connection>, String)>) {
    reg.retain(|(w, _)| w.upgrade().is_some_and(|c| !c.is_closed()));
}

/// Marks one attach as in flight. While any guard is alive the automatic
/// heal is disabled: a granted-but-unregistered lease looks exactly like a
/// stale session to [`own_server_idle`].
pub(crate) struct AttachGuard;

impl AttachGuard {
    pub(crate) fn new() -> Self {
        state().lock().attach_inflight += 1;
        Self
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        let mut s = state().lock();
        s.attach_inflight = s.attach_inflight.saturating_sub(1);
    }
}

/// Store the remote mode (host/token) for future attaches; `None` returns to
/// spawned-local mode.
pub(crate) fn set_remote(host: Option<String>, token: Option<String>) {
    let mut s = state().lock();
    s.host = host;
    s.token = token;
}

/// The active remote configuration, for restoring it after a failed `open()`.
pub(crate) fn remote_config() -> (Option<String>, Option<String>) {
    let s = state().lock();
    (s.host.clone(), s.token.clone())
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

/// Kill the local server. Refused for servers pyprobe-rs did not spawn
/// unless `force` is set (then only after verifying the pidfile points at a
/// live `probe-rs serve` process). Always refused in remote mode.
pub(crate) fn shutdown_server(force: bool) -> Result<(), BridgeError> {
    let mut s = state().lock();
    if s.host.is_some() {
        return Err(BridgeError::Connection(
            "pyprobe-rs is in remote mode; there is no local server to shut down".into(),
        ));
    }
    if s.manager.spawned_by_us() {
        s.manager.kill();
        return Ok(());
    }
    if force {
        // Adopted-server recovery: a stranded spawned endpoint (design §11.3)
        // keeps the probe lease after its client died; the only fix is
        // killing that server. The pidfile gate in the manager keeps this
        // from touching anything but a `probe-rs serve` process.
        s.manager
            .kill_by_pidfile()
            .map_err(BridgeError::Connection)
    } else {
        Err(BridgeError::Connection(
            "refusing to shut down a server pyprobe-rs did not spawn; \
             call shutdown_server(force=True) to restart it anyway"
                .into(),
        ))
    }
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
    let attempt = || {
        bridge
            .wait(CONNECT_TIMEOUT, "connect", Connection::connect(&url, None))
            .and_then(|inner| inner)
    };
    match attempt() {
        Ok(client) => Ok(client),
        Err(e @ BridgeError::ServerVersion(_)) => {
            // A schema mismatch on a server we spawned means a stale binary
            // from an older build; restarting picks up the current one.
            if heal_restart().is_ok() {
                attempt()
            } else {
                Err(e)
            }
        }
        // A server that answered the readiness probe can die before the
        // first RPC lands (adopt-vs-connect race): converge with one
        // respawn + retry instead of failing the idempotent open().
        Err(e @ (BridgeError::Connection(_) | BridgeError::ServerGone(_))) => {
            if state().lock().manager.ensure_running().is_ok() {
                attempt().map_err(|_| e)
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

/// True when the running server was spawned by this crate, no attach is in
/// flight, and no live connection exists — the preconditions for automatic
/// stale-session healing.
pub(crate) fn own_server_idle() -> bool {
    let s = state().lock();
    if !s.manager.spawned_by_us() || s.attach_inflight > 0 {
        return false;
    }
    let mut reg = SESSIONS.lock();
    prune(&mut reg);
    reg.is_empty()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::test_support::for_tests;

    #[test]
    fn registry_prunes_dead_and_severed_entries() {
        SESSIONS.lock().clear();
        let live = for_tests();
        register(&live, "chip-a".into());
        let severed = for_tests();
        register(&severed, "chip-b".into());
        severed.sever();
        let list = session_list();
        assert_eq!(list.len(), 1);
        assert!(list[0].contains("chip-a"));
        // Dropping the last Arc prunes on the next registry touch.
        let weak = Arc::downgrade(&live);
        drop(live);
        assert!(weak.upgrade().is_none());
        assert!(session_list().is_empty());
        SESSIONS.lock().clear();
    }

    #[test]
    fn attach_guard_blocks_the_idle_heal_decision() {
        let before = state().lock().attach_inflight;
        {
            let _guard = AttachGuard::new();
            assert_eq!(state().lock().attach_inflight, before + 1);
        }
        assert_eq!(state().lock().attach_inflight, before);
    }
}
