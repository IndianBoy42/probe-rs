//! `ServerManager`: owns the spawned `probe-rs serve` child for pyprobe-rs.
//!
//! This is the process-side half of the RPC refactor: it resolves the
//! `probe-rs` binary, generates a private unix-socket server config, spawns
//! the server in an environment that cannot pick up a user's own
//! `~/.probe-rs.toml [server]` table, waits for socket readiness, and provides
//! the kill / log / pidfile bookkeeping. The RPC schema handshake
//! (`ensure_compatible`) is the bridge's job, not ours.
//!
//! Unix-only end-to-end: the private-socket transport and its auth story
//! (`0o660`, no user list) have no Windows equivalent in the PoC.

#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Lines kept in the child-output ring; older lines drop first.
const LOG_RING_LINES: usize = 200;
/// Poll interval while waiting for the server socket to open.
const READINESS_POLL: Duration = Duration::from_millis(50);
/// Total socket-readiness budget. serve binds almost immediately; the budget
/// only has to cover process start on a loaded machine.
const READINESS_BUDGET: Duration = Duration::from_secs(10);

/// How an `ensure_running` call converged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnsureOutcome {
    /// A live server already answers on our socket (ours or an adopted one).
    AlreadyRunning,
    /// We spawned a fresh `probe-rs serve` and its socket is ready.
    Spawned,
}

/// Why the server could not be delivered ready. Every message names the fix.
#[derive(Debug)]
pub(crate) enum ManagerError {
    /// No `probe-rs` executable found (env override, workspace target dirs, PATH).
    BinaryNotFound,
    /// The child could not be spawned at all.
    Spawn(std::io::Error),
    /// The socket never became connectable inside the readiness budget.
    ReadinessTimeout,
    /// The generated server config could not be written.
    Config(std::io::Error),
}

impl std::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryNotFound => write!(
                f,
                "no probe-rs server binary found (checked PYPROBE_RS_BIN, \
                 workspace target/{{debug,release}}, PATH); \
                 set PYPROBE_RS_BIN to a built `probe-rs` executable"
            ),
            Self::Spawn(e) => {
                write!(f, "failed to spawn the probe-rs server ({e}); check the PYPROBE_RS_BIN path")
            }
            Self::ReadinessTimeout => write!(
                f,
                "the probe-rs server did not open its socket in time; \
                 inspect the server logs for its startup crash"
            ),
            Self::Config(e) => {
                write!(f, "failed to write the generated server config ({e}); check that the runtime dir is writable")
            }
        }
    }
}

impl std::error::Error for ManagerError {}

/// Owns (or adopts) the local `probe-rs serve` child on a private unix socket.
pub(crate) struct ServerManager {
    /// Our child, when we spawned it in this process instance; adopted
    /// (foreign or orphaned) servers have no handle here.
    child: Option<Child>,
    /// The private runtime dir holding the generated config, socket and pidfile.
    dir: PathBuf,
    /// Child stdout+stderr lines, drop-oldest, surfaced by [`Self::logs`].
    ring: Arc<Mutex<VecDeque<String>>>,
    /// Whether the current child is one we spawned ourselves.
    spawned_by_us: bool,
}

impl ServerManager {
    pub(crate) fn new() -> Self {
        let dir = Self::private_dir();
        // Best-effort here: a failure surfaces where it matters
        // (`write_config`), not as a constructor panic.
        let _ = std::fs::create_dir_all(&dir);
        Self {
            child: None,
            dir,
            ring: Arc::new(Mutex::new(VecDeque::new())),
            spawned_by_us: false,
        }
    }

    /// `<$XDG_RUNTIME_DIR|$TMPDIR|/tmp>/pyprobe-rs-<uid>`: a per-user runtime
    /// dir so concurrent users (and our own stale servers) never collide.
    pub(crate) fn private_dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .or_else(|| std::env::var_os("TMPDIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join(format!("pyprobe-rs-{}", current_uid()))
    }

    pub(crate) fn socket_path(&self) -> PathBuf {
        self.dir.join("serve.sock")
    }

    pub(crate) fn socket_url(&self) -> String {
        format!("socket://{}", self.socket_path().display())
    }

    fn pidfile_path(&self) -> PathBuf {
        let mut os = self.socket_path().into_os_string();
        os.push(".pid");
        PathBuf::from(os)
    }

    /// The generated config: unix-socket mode with no `users` key. serve then
    /// performs no auth and grants `ProbeAccess::All`; privacy comes from the
    /// socket's 0o660 perms, not from a user list.
    pub(crate) fn write_config(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.dir.join(".probe-rs.toml"),
            format!("[server]\naddress = \"{}\"\n", self.socket_url()),
        )
    }

    /// Binary resolution order: `PYPROBE_RS_BIN` → workspace
    /// `target/{debug,release}/probe-rs` → PATH. An env override that does not
    /// exist falls through rather than hard-failing; binary drift is caught by
    /// the bridge's schema handshake.
    pub(crate) fn resolve_binary() -> Option<PathBuf> {
        if let Some(bin) = std::env::var_os("PYPROBE_RS_BIN") {
            let bin = PathBuf::from(bin);
            if bin.is_file() {
                return Some(bin);
            }
        }
        // The workspace root is the parent of this crate dir; a sibling build
        // of the same checkout is the cheapest drift-checked default.
        if let Some(root) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent() {
            for profile in ["debug", "release"] {
                let bin = root.join("target").join(profile).join("probe-rs");
                if bin.is_file() {
                    return Some(bin);
                }
            }
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join("probe-rs"))
            .find(|bin| bin.is_file())
    }

    /// Raw transport probe: can we connect to the unix socket right now? This
    /// is readiness in the socket sense only; the RPC schema handshake lives
    /// in the bridge. std's `connect_timeout` for unix sockets is
    /// nightly-only, but a filesystem-socket connect resolves (or refuses)
    /// locally without hanging, so a plain connect is the stable equivalent
    /// of the short per-attempt probe.
    pub(crate) fn socket_connectable(&self) -> bool {
        UnixStream::connect(self.socket_path()).is_ok()
    }
    /// True when the pidfile describes a live process spawned into our
    /// private dir (our spawn signature). Used only for the stale-server heal
    /// decision; we never kill a pid from the pidfile alone.
    pub(crate) fn is_ours(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.pidfile_path()) else {
            return false;
        };
        let mut lines = text.lines();
        let Some(Ok(pid)) = lines.next().map(str::parse::<u32>) else {
            return false;
        };
        match lines.next() {
            Some(dir) if dir == self.dir.display().to_string() => pid_alive(pid),
            _ => false,
        }
    }

    /// Idempotent convergence to "a ready server on our socket": a live server
    /// is adopted (`AlreadyRunning`), our own stale-but-signed socket is
    /// cleaned up and respawned, and an absent socket gets a fresh spawn plus
    /// the readiness wait.
    pub(crate) fn ensure_running(&mut self) -> Result<EnsureOutcome, ManagerError> {
        self.ensure_running_budget(READINESS_BUDGET)
    }

    /// [`Self::ensure_running`] with an explicit readiness budget (tests use a
    /// short one).
    fn ensure_running_budget(&mut self, budget: Duration) -> Result<EnsureOutcome, ManagerError> {
        if self.socket_connectable() {
            return Ok(EnsureOutcome::AlreadyRunning);
        }

        // We hold a child but its socket is dead: it either crashed or hung.
        // It is ours, so kill/reap is always safe and respawn is the only way
        // forward.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.spawned_by_us = false;
        }

        // A stale socket node signed by our pidfile is ours to remove.
        // Foreign stale nodes are left alone: serve unlinks them on start.
        if self.is_ours() {
            let _ = std::fs::remove_file(self.socket_path());
            let _ = std::fs::remove_file(self.pidfile_path());
        }

        self.spawn()?;
        if self.wait_ready(budget) {
            Ok(EnsureOutcome::Spawned)
        } else {
            // A server that never opened its socket is dead weight; it is
            // ours, so take it (and its junk node) down instead of parking it.
            self.kill();
            let _ = std::fs::remove_file(self.socket_path());
            Err(ManagerError::ReadinessTimeout)
        }
    }

    /// Idempotent shutdown: kill+reap our child, unlink the pidfile and clear
    /// ownership state. serve has no signal handlers, so kill is the only
    /// shutdown path; adopted foreign servers have no child handle and are
    /// untouched.
    pub(crate) fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.spawned_by_us = false;
        let _ = std::fs::remove_file(self.pidfile_path());
    }

    /// Snapshot of the child's stdout+stderr lines, oldest first.
    pub(crate) fn logs(&self) -> Vec<String> {
        self.log_ring().iter().cloned().collect()
    }

    pub(crate) fn spawned_by_us(&self) -> bool {
        self.spawned_by_us
    }

    fn log_ring(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.ring.lock().expect("server log ring poisoned")
    }

    fn wait_ready(&self, budget: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < budget {
            if self.socket_connectable() {
                return true;
            }
            std::thread::sleep(READINESS_POLL);
        }
        self.socket_connectable()
    }

    fn spawn(&mut self) -> Result<(), ManagerError> {
        let bin = Self::resolve_binary().ok_or(ManagerError::BinaryNotFound)?;
        self.write_config().map_err(ManagerError::Config)?;
        let mut cmd = Command::new(bin);
        cmd.arg("serve")
            .current_dir(&self.dir)
            // cwd AND HOME are pinned to the private dir so a user's
            // ~/.probe-rs.toml [server] table cannot leak into our instance
            // (residual exe-dir-config risk is accepted and documented).
            .env("HOME", &self.dir)
            // serve logs via tracing; default to quiet unless the embedder
            // chose a level.
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string()),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(ManagerError::Spawn)?;
        if let Some(out) = child.stdout.take() {
            spawn_log_reader(Arc::clone(&self.ring), out);
        }
        if let Some(err) = child.stderr.take() {
            spawn_log_reader(Arc::clone(&self.ring), err);
        }
        // Bookkeeping only: a failed pidfile degrades the heal decision, not
        // the server itself.
        let _ = std::fs::write(
            self.pidfile_path(),
            format!("{}\n{}\n", child.id(), self.dir.display()),
        );
        self.child = Some(child);
        self.spawned_by_us = true;
        Ok(())
    }
}

impl Drop for ServerManager {
    fn drop(&mut self) {
        // No kill on drop: an orphaned server is adopted by the next kernel,
        // not murdered with it. Reap only a child that already exited, so we
        // leave no zombie behind.
        if let Some(child) = self.child.as_mut() {
            let _ = child.try_wait();
        }
    }
}

/// Feeds one child output stream into the drop-oldest log ring.
fn spawn_log_reader(ring: Arc<Mutex<VecDeque<String>>>, stream: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut q = ring.lock().expect("server log ring poisoned");
            if q.len() >= LOG_RING_LINES {
                q.pop_front();
            }
            q.push_back(line);
        }
    });
}

fn current_uid() -> String {
    // std has no uid API; /proc/self/status carries the real uid on Linux.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(real) = rest.split_whitespace().next() {
                    return real.to_string();
                }
        }
    }
    // Non-Linux unix: $USER keeps the dir per-user (where XDG_RUNTIME_DIR
    // exists it is already user-private, so the suffix is belt-and-braces).
    std::env::var("USER").unwrap_or_else(|_| "shared".to_string())
}

fn pid_alive(pid: u32) -> bool {
    // std has no liveness probe; /proc is authoritative on Linux.
    if Path::new(&format!("/proc/{pid}")).exists() {
        return true;
    }
    // Non-Linux unix fallback: `kill -0` succeeds for a live, signalable pid.
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The env-touching tests must not interleave: the process environment is
    /// global, and the stub children inherit it.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// One isolated env + dir sandbox per test. Created and dropped while the
    /// env lock is held.
    struct Sandbox {
        dir: PathBuf,
        script: PathBuf,
        saved_path: Option<std::ffi::OsString>,
    }

    fn sandbox(mode: &str) -> Sandbox {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pyprobe-rs-server-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("sandbox dir");
        // The stub "server" is this test binary itself: a sh wrapper execs it
        // into the `stub_server` test, which reads PYPROBE_RS_STUB_MODE.
        let exe = std::env::current_exe().expect("current_exe");
        let script = dir.join("stub-bin.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nexec env PYPROBE_RS_STUB_ACTIVE=1 \"{}\" stub_server --nocapture\n",
                exe.display()
            ),
        )
        .expect("stub script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub script");
        // SAFETY: single-threaded by the env lock; the child processes inherit
        // these values, which is the point.
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
            std::env::set_var("PYPROBE_RS_BIN", &script);
            std::env::set_var("PYPROBE_RS_STUB_MODE", mode);
        }
        Sandbox {
            dir,
            script,
            saved_path: std::env::var_os("PATH"),
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            // SAFETY: the env lock is still held by the owning test.
            unsafe {
                std::env::remove_var("PYPROBE_RS_BIN");
                std::env::remove_var("PYPROBE_RS_STUB_MODE");
                std::env::remove_var("XDG_RUNTIME_DIR");
                match self.saved_path.take() {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The stub "server" the manager tests spawn: this same test binary, exec'd
    /// via the sandbox wrapper. The wrapper-set `PYPROBE_RS_STUB_ACTIVE`
    /// sentinel keeps it inert in a normal suite run (where the env lock
    /// holder may have stub mode set): only a spawned stub child carries it.
    #[test]
    fn stub_server() {
        if std::env::var_os("PYPROBE_RS_STUB_ACTIVE").is_none() {
            return;
        }
        let Ok(mode) = std::env::var("PYPROBE_RS_STUB_MODE") else {
            return;
        };
        let cfg = std::fs::read_to_string(".probe-rs.toml")
            .expect("stub: generated config present in cwd");
        let addr = cfg
            .lines()
            .find_map(|l| l.trim().strip_prefix("address = \"socket://"))
            .expect("stub: socket address in config");
        let sock = PathBuf::from(addr.trim_end_matches('"'));
        match mode.as_str() {
            // Bind + listen: readiness must succeed. Park until killed, with a
            // lifetime cap so a broken test cannot leak a parker forever.
            "bind" => {
                let _ = std::fs::remove_file(&sock);
                let listener = std::os::unix::net::UnixListener::bind(&sock)
                    .expect("stub: bind socket");
                println!("pyprobe-stub listening");
                for _ in 0..300 {
                    let _ = listener.accept();
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            // Socket node present but nothing listening: readiness must time out.
            "stall" => {
                std::fs::write(&sock, b"").expect("stub: create stall node");
                println!("pyprobe-stub stalled");
                for _ in 0..300 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            _ => {}
        }
    }

    fn wait_for_log(m: &ServerManager, needle: &str, budget: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < budget {
            if m.logs().iter().any(|l| l.contains(needle)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn server_config_is_exact() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let m = ServerManager::new();
        m.write_config().expect("write config");
        let text =
            std::fs::read_to_string(m.dir.join(".probe-rs.toml")).expect("config readable");
        assert_eq!(text, format!("[server]\naddress = \"{}\"\n", m.socket_url()));
        // No `users` key: unix-socket mode performs no auth and grants all.
        assert!(!text.contains("users"));
    }

    #[test]
    fn server_binary_resolution_env_and_path() {
        let _g = env_lock();
        let sb = sandbox("bind");
        // An env override that exists wins over everything.
        assert_eq!(ServerManager::resolve_binary(), Some(sb.script.clone()));
        // An env override that does not exist falls through.
        unsafe { std::env::set_var("PYPROBE_RS_BIN", sb.dir.join("missing-bin")) };
        // With the override gone and PATH empty, only the workspace bins can
        // answer; none exist in a bare checkout.
        unsafe {
            std::env::remove_var("PYPROBE_RS_BIN");
            let empty = sb.dir.join("empty-path");
            std::fs::create_dir_all(&empty).unwrap();
            std::env::set_var("PATH", &empty);
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let ws = ["debug", "release"]
            .iter()
            .map(|p| root.join("target").join(p).join("probe-rs"))
            .find(|b| b.is_file());
        assert_eq!(ServerManager::resolve_binary(), ws);
    }

    #[test]
    fn server_readiness_times_out_on_stalled_stub() {
        let _g = env_lock();
        let _sb = sandbox("stall");
        let mut m = ServerManager::new();
        assert!(matches!(
            m.ensure_running_budget(Duration::from_millis(600)),
            Err(ManagerError::ReadinessTimeout)
        ));
        // The timed-out child was taken down; nothing of ours remains.
        assert!(!m.spawned_by_us());
        assert!(!m.pidfile_path().exists());
        assert!(!m.socket_path().exists());
    }

    #[test]
    fn server_spawn_reaches_readiness_logs_and_pidfile() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        assert!(m.spawned_by_us());
        assert!(m.socket_connectable());
        // The pidfile records our spawn signature (pid + private dir).
        let text = std::fs::read_to_string(m.pidfile_path()).expect("pidfile");
        let mut lines = text.lines();
        let pid: u32 = lines.next().unwrap().parse().unwrap();
        assert_eq!(lines.next(), Some(m.dir.display().to_string().as_str()));
        assert!(pid_alive(pid));
        // The stub announces itself through the log ring.
        assert!(wait_for_log(&m, "pyprobe-stub", Duration::from_secs(5)));
        m.kill();
    }

    #[test]
    fn server_ensure_running_is_idempotent() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::AlreadyRunning);
        m.kill();
    }

    #[test]
    fn server_kill_terminates_stub_and_clears_state() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        let pid: u32 = std::fs::read_to_string(m.pidfile_path())
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid_alive(pid));
        m.kill();
        assert!(!m.spawned_by_us());
        assert!(!m.pidfile_path().exists());
        assert!(!pid_alive(pid));
        // Idempotent: a second kill is a no-op.
        m.kill();
    }

    #[test]
    fn server_is_ours_pidfile_logic() {
        let _g = env_lock();
        let sb = sandbox("bind");
        let m = ServerManager::new();
        let pidfile = m.pidfile_path();
        assert!(!m.is_ours());
        // A real live stub with our dir signature: ours.
        m.write_config().unwrap();
        let mut stub = Command::new(std::env::current_exe().unwrap())
            .args(["stub_server", "--nocapture"])
            .env("PYPROBE_RS_STUB_ACTIVE", "1")
            .current_dir(&m.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn live stub");
        let pid = stub.id();
        std::fs::write(&pidfile, format!("{pid}\n{}\n", m.dir.display())).unwrap();
        assert!(m.is_ours());
        stub.kill().unwrap();
        stub.wait().unwrap();
        // Same signature, now-dead pid: never ours.
        assert!(!m.is_ours());
        // Right pid (this test process), wrong dir: not ours.
        std::fs::write(&pidfile, format!("{}\n/somewhere/else\n", std::process::id())).unwrap();
        assert!(!m.is_ours());
        // Garbage pidfile: not ours.
        std::fs::write(&pidfile, "garbage\n").unwrap();
        assert!(!m.is_ours());
        drop(sb);
    }

    #[test]
    fn server_stale_our_signature_socket_respawns() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        m.kill();
        // Our own stale signature: live pid + our dir; the socket node the
        // killed stub left behind is the dead node (a regular-file write
        // over a socket node would fail with ENXIO).
        std::fs::write(
            m.pidfile_path(),
            format!("{}\n{}\n", std::process::id(), m.dir.display()),
        )
        .unwrap();
        assert!(!m.socket_connectable());
        assert!(m.is_ours());
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        assert!(m.socket_connectable());
        m.kill();
    }

    #[test]
    fn server_dead_child_is_reaped_and_respawned() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        // The server dies behind our back (kernel-restart orphan shape): the
        // next ensure_running reaps it and converges to a fresh ready server.
        let mut child = m.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        m.spawned_by_us = false;
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        assert!(m.socket_connectable());
        m.kill();
    }

    #[test]
    fn server_foreign_stale_node_is_left_to_serve() {
        let _g = env_lock();
        let _sb = sandbox("bind");
        let mut m = ServerManager::new();
        // A stale node with no pidfile of ours: not ours to clean, so spawn
        // anyway and let serve unlink it, as it does on start.
        std::fs::write(m.socket_path(), b"junk").unwrap();
        assert!(!m.is_ours());
        assert_eq!(m.ensure_running().unwrap(), EnsureOutcome::Spawned);
        assert!(m.socket_connectable());
        m.kill();
    }
}
