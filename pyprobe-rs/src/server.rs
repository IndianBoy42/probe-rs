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

use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    /// No `probe-rs` executable found (env override, embedded copy, workspace target dirs).
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
                "no probe-rs server binary found (checked PYPROBE_RS_BIN, the embedded \
                 binary, and workspace target/{{server,release,debug}}); run \
                 `just build-serve-bin` or set PYPROBE_RS_BIN to a built `probe-rs` executable"
            ),
            Self::Spawn(e) => {
                write!(
                    f,
                    "failed to spawn the probe-rs server ({e}); check the PYPROBE_RS_BIN path"
                )
            }
            Self::ReadinessTimeout => write!(
                f,
                "the probe-rs server did not open its socket in time; \
                 inspect the server logs for its startup crash"
            ),
            Self::Config(e) => {
                write!(
                    f,
                    "failed to write the generated server config ({e}); check that the runtime dir is writable"
                )
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
    /// The private runtime dir holding the generated config, socket,
    /// pidfile and server log.
    dir: PathBuf,
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
            config_toml(&self.socket_url()),
        )
    }

    /// The workspace `probe-rs` server binary embedded at build time; empty
    /// when the `bundled-server` feature is off or no binary had been built.
    #[cfg(feature = "bundled-server")]
    const EMBEDDED: &[u8] = include_bytes!(env!("PYPROBE_RS_EMBED"));
    #[cfg(feature = "bundled-server")]
    const EMBEDDED_SHA: &str = env!("PYPROBE_RS_EMBED_SHA");

    /// Binary resolution order: `PYPROBE_RS_BIN` → the embedded server
    /// binary (feature `bundled-server`) → workspace
    /// `target/{server,release,debug}/probe-rs`. `PATH` is deliberately not
    /// consulted: the RPC protocol is version-unstable, so only a binary
    /// built from this source tree is schema-guaranteed; a foreign `probe-rs`
    /// can pass the handshake yet behave differently.
    pub(crate) fn resolve_binary() -> Option<PathBuf> {
        if let Some(bin) = std::env::var_os("PYPROBE_RS_BIN") {
            let bin = PathBuf::from(bin);
            if bin.is_file() {
                return Some(bin);
            }
        }
        #[cfg(feature = "bundled-server")]
        if let Some(bin) = Self::extract_embedded() {
            return Some(bin);
        }
        // The workspace root is the parent of this crate dir; a sibling build
        // of the same checkout is the cheapest drift-checked default.
        if let Some(root) = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent() {
            for profile in ["server", "release", "debug"] {
                let bin = root.join("target").join(profile).join("probe-rs");
                if bin.is_file() {
                    return Some(bin);
                }
            }
        }
        None
    }

    /// Materialize the embedded server binary under the private dir and
    /// return its path. `None` when nothing was embedded. Idempotent: a
    /// file with the right size and exec bits is reused; otherwise a
    /// pid-suffixed temp file is written, chmodded, and atomically renamed
    /// into place (POSIX rename replaces the destination), so concurrent
    /// processes converge on a complete file.
    #[cfg(feature = "bundled-server")]
    fn extract_embedded() -> Option<PathBuf> {
        if Self::EMBEDDED.is_empty() {
            return None;
        }
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dest = Self::private_dir()
            .join("bin")
            .join(format!("probe-rs-{}", &Self::EMBEDDED_SHA[..8]));
        #[allow(clippy::unnecessary_cast)] // m.len() is u64, EMBEDDED.len() is usize
        let usable = std::fs::metadata(&dest)
            .map(|m| m.len() as usize == Self::EMBEDDED.len() && m.mode() & 0o111 != 0)
            .unwrap_or(false);
        if !usable {
            std::fs::create_dir_all(dest.parent().unwrap()).ok()?;
            let tmp = dest.with_extension(format!("tmp.{}", std::process::id()));
            std::fs::write(&tmp, Self::EMBEDDED).ok()?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok()?;
            std::fs::rename(&tmp, &dest).ok()?;
        }
        Some(dest)
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

    /// Force-kill an ADOPTED server recorded in our pidfile. Safety gate: the
    /// pid must be a live `probe-rs serve` process before any signal is sent,
    /// so a stale or forged pidfile can never kill an unrelated process.
    /// Returns an error when there is no pidfile (a server not spawned by any
    /// pyprobe-rs manager) or the pid is not a probe-rs serve process.
    pub(crate) fn kill_by_pidfile(&mut self) -> Result<(), String> {
        let text = std::fs::read_to_string(self.pidfile_path()).map_err(|_| {
            "no server pidfile: this server was not spawned by pyprobe-rs".to_string()
        })?;
        let pid: u32 = text
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .ok_or_else(|| "server pidfile is malformed".to_string())?;
        let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .map_err(|_| format!("pid {pid} from the server pidfile no longer exists"))?;
        let argv: Vec<&str> = cmdline.split('\0').filter(|a| !a.is_empty()).collect();
        let is_serve = argv.iter().any(|a| a.ends_with("probe-rs")) && argv.contains(&"serve");
        if !is_serve {
            return Err(format!(
                "pid {pid} from the server pidfile is not a `probe-rs serve` process; \
                 refusing to kill it"
            ));
        }
        Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("failed to signal pid {pid}: {e}"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.spawned_by_us = false;
        let _ = std::fs::remove_file(self.pidfile_path());
        let _ = std::fs::remove_file(self.socket_path());
        Ok(())
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("serve.log")
    }

    /// Tail of the server log, oldest first.
    ///
    /// The log is a file, not pipes to this process: an orphaned server
    /// whose parent died keeps writing safely, and the next kernel can read
    /// what happened while nobody was attached.
    pub(crate) fn logs(&self) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(self.log_path()) else {
            return Vec::new();
        };
        let lines: Vec<&str> = text.lines().collect();
        let skip = lines.len().saturating_sub(LOG_RING_LINES);
        lines.iter().skip(skip).map(|s| s.to_string()).collect()
    }

    pub(crate) fn spawned_by_us(&self) -> bool {
        self.spawned_by_us
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
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(self.log_path())
            .map_err(ManagerError::Config)?;
        let err_log = log.try_clone().map_err(ManagerError::Config)?;
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
            // Never inherit the parent's stdio: a dead parent's pipes turn
            // server-side output into EPIPE for the orphan's whole lifetime.
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log))
            // Own process group: the server must survive its parent (and
            // process-group kills from kernel runners like `uv run`) so the
            // next kernel can adopt it.
            .process_group(0);
        let child = cmd.spawn().map_err(ManagerError::Spawn)?;
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

fn current_uid() -> String {
    // std has no uid API; /proc/self/status carries the real uid on Linux.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(real) = rest.split_whitespace().next()
            {
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
    use parking_lot::{Mutex, MutexGuard};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The env-touching tests must not interleave: the process environment is
    /// global, and the stub children inherit it.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
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
                let listener =
                    std::os::unix::net::UnixListener::bind(&sock).expect("stub: bind socket");
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
        let text = std::fs::read_to_string(m.dir.join(".probe-rs.toml")).expect("config readable");
        assert_eq!(
            text,
            format!("[server]\naddress = \"{}\"\n", m.socket_url())
        );
        // No `users` key: unix-socket mode performs no auth and grants all.
        assert!(!text.contains("users"));
    }
    #[test]
    fn server_binary_resolution_env_and_workspace() {
        let _g = env_lock();
        let sb = sandbox("bind");
        // An env override that exists wins over everything — including the
        // embedded copy and the workspace binaries.
        assert_eq!(ServerManager::resolve_binary(), Some(sb.script.clone()));
        // An env override that does not exist falls through.
        unsafe { std::env::set_var("PYPROBE_RS_BIN", sb.dir.join("missing-bin")) };
        // PATH is no longer consulted at all: point it at a directory with a
        // `probe-rs` stub and assert it is ignored.
        unsafe {
            use std::os::unix::fs::PermissionsExt;
            std::env::remove_var("PYPROBE_RS_BIN");
            let fake = sb.dir.join("fake-path");
            std::fs::create_dir_all(&fake).unwrap();
            let stub = fake.join("probe-rs");
            std::fs::write(&stub, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::env::set_var("PATH", &fake);
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let ws = ["server", "release", "debug"]
            .iter()
            .map(|p| root.join("target").join(p).join("probe-rs"))
            .find(|b| b.is_file());
        let resolved = ServerManager::resolve_binary();
        #[cfg(feature = "bundled-server")]
        if !ServerManager::EMBEDDED.is_empty() {
            // The embedded copy exists in this build: it wins over both the
            // (ignored) PATH stub and any workspace binary.
            let path = resolved.expect("embedded copy resolves");
            assert!(path.starts_with(ServerManager::private_dir()));
            return;
        }
        match (ws, resolved) {
            // Dev checkout with a workspace build: it must win over PATH.
            (Some(_), Some(resolved)) => assert!(resolved.starts_with(&root)),
            // Bare checkout: resolution must fail rather than hit PATH.
            (None, None) => {}
            other => panic!("unexpected resolution {other:?}"),
        }
    }

    #[cfg(feature = "bundled-server")]
    #[test]
    fn embedded_server_extracts_idempotently() {
        use std::os::unix::fs::MetadataExt;
        let _g = env_lock();
        let _sb = sandbox("embed");
        let bin = ServerManager::extract_embedded();
        if ServerManager::EMBEDDED.is_empty() {
            assert!(bin.is_none(), "empty embed must resolve to None");
            return;
        }
        let bin = bin.expect("non-empty embed extracts");
        let meta = std::fs::metadata(&bin).expect("extracted file exists");
        #[allow(clippy::unnecessary_cast)] // m.len() is u64, EMBEDDED.len() is usize
        {
            assert_eq!(meta.len() as usize, ServerManager::EMBEDDED.len());
        }
        assert_ne!(meta.mode() & 0o111, 0, "extracted binary is executable");
        assert_eq!(
            bin.file_name().and_then(|n| n.to_str()),
            Some(format!("probe-rs-{}", &ServerManager::EMBEDDED_SHA[..8]).as_str())
        );
        // A second call must reuse the extracted file, not rewrite it.
        let mtime = meta.modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let bin2 = ServerManager::extract_embedded().expect("re-extract");
        assert_eq!(bin, bin2);
        assert_eq!(
            std::fs::metadata(&bin2).unwrap().modified().unwrap(),
            mtime,
            "usable file must be reused, not rewritten"
        );
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
        std::fs::write(
            &pidfile,
            format!("{}\n/somewhere/else\n", std::process::id()),
        )
        .unwrap();
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

/// The generated `[server]` table. TOML basic strings escape `\` and `"` —
/// the socket path comes from the environment and can contain either.
fn config_toml(socket_url: &str) -> String {
    let url = socket_url.replace('\\', "\\\\").replace('"', "\\\"");
    format!("[server]\naddress = \"{url}\"\n")
}

#[cfg(test)]
mod config_tests {
    use super::config_toml;

    #[test]
    fn config_escapes_quotes_and_backslashes() {
        let toml = config_toml("socket:///tmp/w\"eird\\path/serve.sock");
        assert_eq!(
            toml,
            "[server]\naddress = \"socket:///tmp/w\\\"eird\\\\path/serve.sock\"\n"
        );
    }
}
