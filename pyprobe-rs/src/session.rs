//! Session, SessionConfig, Permissions, Registry, Target.

use std::sync::Arc;
use std::time::Duration;

use probe_rs::CoreType;
use probe_rs::config::{Registry as RsRegistry, Target as RsTarget};
use probe_rs_rpc::core_ops::WireCoreType;
use probe_rs_rpc::info::WireSessionTargetMetadata;
use probe_rs_rpc::probe::{
    AttachRequest, DebugProbeEntry, DebugProbeSelector as WireProbeSelector, WireProtocol,
};
use probe_rs_rpc_client::RpcClient;
use probe_rs_target::MemoryRegion;
use pyo3::prelude::*;

/// pyo3 0.29 removed the `PyObject` alias; spell it out once here.
type PyObject = Py<PyAny>;

use crate::error::{BridgeError, py_err_with_kind, to_bridge_err, to_py_err};
use crate::open_state;
use crate::rpc::{Bridge, Connection, DEFAULT_TIMEOUT, call_with_recovery, rpc_call};
use crate::types::*;

/// Session configuration.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct SessionConfig {
    #[pyo3(get, set)]
    pub speed: Option<u32>,
    #[pyo3(get, set)]
    pub protocol: Option<WireProtocolProxy>,
    #[pyo3(get, set)]
    pub permissions: Option<Permissions>,
}

impl SessionConfig {
    /// Wire protocol for the attach request.
    pub(crate) fn wire_protocol(&self) -> Option<WireProtocol> {
        self.protocol.map(|p| match p {
            WireProtocolProxy::Swd => WireProtocol::Swd,
            WireProtocolProxy::Jtag => WireProtocol::Jtag,
        })
    }

    /// Whether the configured permissions allow the erase-all sequence.
    pub(crate) fn allow_erase_all(&self) -> bool {
        self.permissions.as_ref().is_some_and(|p| p.allow_erase_all)
    }
}

impl From<Option<Permissions>> for SessionConfig {
    fn from(perms: Option<Permissions>) -> Self {
        Self {
            speed: None,
            protocol: None,
            permissions: perms,
        }
    }
}

#[pymethods]
impl SessionConfig {
    #[new]
    #[pyo3(signature = (speed=None, protocol=None, permissions=None))]
    pub fn new(
        speed: Option<u32>,
        protocol: Option<WireProtocolProxy>,
        permissions: Option<Permissions>,
    ) -> Self {
        Self {
            speed,
            protocol,
            permissions,
        }
    }
}

/// Permissions granted to a debug session.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct Permissions {
    pub(crate) allow_erase_all: bool,
}

#[pymethods]
impl Permissions {
    #[new]
    pub fn new() -> Self {
        Self {
            allow_erase_all: false,
        }
    }

    /// Allow the erase-all sequence.
    pub fn allow_erase_all(&mut self) {
        self.allow_erase_all = true;
    }
}

impl Default for Permissions {
    fn default() -> Self {
        Self::new()
    }
}

/// Target specification (read-only from Python).
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct Target {
    pub(crate) inner: RsTarget,
}

#[pymethods]
impl Target {
    #[getter]
    fn name(&self) -> String {
        self.inner.name.clone()
    }

    #[getter]
    fn architecture(&self) -> ArchitectureProxy {
        architecture_to_py(self.inner.architecture())
    }

    #[getter]
    fn cores(&self) -> Vec<(String, CoreTypeProxy)> {
        self.inner
            .cores
            .iter()
            .map(|c| (c.name.clone(), core_type_to_py(c.core_type)))
            .collect()
    }

    #[getter]
    fn flash_algorithms(&self) -> Vec<String> {
        self.inner
            .flash_algorithms
            .iter()
            .map(|a| a.name.clone())
            .collect()
    }

    #[getter]
    fn memory_map(&self) -> Vec<(String, u64, u64)> {
        self.inner
            .memory_map
            .iter()
            .map(|r| match r {
                MemoryRegion::Nvm(r) => ("nvm", r.range.start, r.range.end),
                MemoryRegion::Ram(r) => ("ram", r.range.start, r.range.end),
                MemoryRegion::Generic(r) => ("generic", r.range.start, r.range.end),
            })
            .map(|(k, s, e)| (k.to_string(), s, e))
            .collect()
    }
}

/// Target registry: lookup targets by name/family.
#[pyclass(name = "Registry")]
pub struct RegistryPy {
    inner: RsRegistry,
}

#[pymethods]
impl RegistryPy {
    #[staticmethod]
    fn from_builtin_families() -> Self {
        Self {
            inner: RsRegistry::from_builtin_families(),
        }
    }

    /// Get all chip family names.
    fn families(&self) -> Vec<String> {
        self.inner
            .families()
            .iter()
            .map(|f| f.name.clone())
            .collect()
    }

    /// Get a target by name.
    fn get_target_by_name(&self, name: &str) -> PyResult<Target> {
        self.inner
            .get_target_by_name(name)
            .map(|t| Target { inner: t })
            .map_err(to_py_err)
    }

    /// Search chips by prefix.
    fn search_chips(&self, prefix: &str) -> Vec<String> {
        self.inner
            .search_chips(prefix)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[pyclass]
pub struct Session {
    pub(crate) conn: Option<Arc<Connection>>,
    pub(crate) meta: WireSessionTargetMetadata,
}

/// One attach attempt, plus the stale-session heal retry.
///
/// The `AttachGuard` is scoped to the attempt: while it is held, the
/// idle-server heal is suppressed because a granted-but-unregistered lease
/// looks exactly like a stale session. A `ProbeBusy` outcome means the
/// attempt was REFUSED and holds no lease, so the guard is dropped before
/// the heal decision runs and `own_server_idle()` can actually fire — the
/// restart also picks up a rebuilt server binary after a code change.
#[allow(clippy::too_many_arguments)]
fn attach_with_heal(
    py: Python<'_>,
    target: &str,
    cfg: &SessionConfig,
    selector: Option<WireProbeSelector>,
    allow_erase: bool,
    dry_run: bool,
    wait: Option<Duration>,
    connect_under_reset: bool,
    deadline: Duration,
) -> Result<Connection, BridgeError> {
    let mut outcome = {
        let _inflight = open_state::AttachGuard::new();
        attach_with(
            py,
            target,
            cfg,
            selector.clone(),
            allow_erase,
            dry_run,
            wait,
            connect_under_reset,
            deadline,
        )
    };
    // Stale-session healing: on a server pyprobe-rs spawned with no live
    // connections, a lingering previous-kernel session is the only
    // plausible ProbeBusy cause; restart the server once and retry.
    if matches!(&outcome, Err(BridgeError::ProbeBusy(_))) && open_state::own_server_idle() {
        eprintln!(
            "pyprobe-rs: probe busy on our own idle server; restarting it to clear a stale session"
        );
        py.detach(open_state::heal_restart)?;
        outcome = attach_with(
            py,
            target,
            cfg,
            selector,
            allow_erase,
            dry_run,
            wait,
            connect_under_reset,
            deadline,
        );
    }
    outcome
}

impl Session {
    /// Connection accessor; fails fast after `close()` or `close_all()`.
    pub(crate) fn conn(&self) -> PyResult<Arc<Connection>> {
        match &self.conn {
            Some(c) if !c.is_closed() => Ok(c.clone()),
            Some(_) => Err(py_err_with_kind(
                "this session was closed by pyprobe_rs.close_all(); attach again".into(),
                "Connection",
            )),
            None => Err(py_err_with_kind(
                "this session is closed; attach again with Session.auto_attach()".into(),
                "Connection",
            )),
        }
    }

    /// Attach preferring `selector` (None = first accessible probe). Used by
    /// `Probe.attach` to hand a locally-chosen probe (and its locally
    /// applied speed/protocol) to the server flow.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_selected(
        target: &str,
        selector: Option<WireProbeSelector>,
        speed: Option<u32>,
        protocol: Option<WireProtocolProxy>,
        allow_erase_all: bool,
        dry_run: bool,
        connect_under_reset: bool,
        wait_for_probe: Option<f64>,
    ) -> PyResult<Session> {
        Python::attach(|py| {
            let cfg = SessionConfig {
                speed,
                protocol,
                permissions: None,
            };
            let wait = wait_for_probe.map(Duration::from_secs_f64);
            let deadline = wait.unwrap_or_default() + ATTACH_MARGIN;
            let conn = attach_with_heal(
                py,
                target,
                &cfg,
                selector,
                allow_erase_all,
                dry_run,
                wait,
                connect_under_reset,
                deadline,
            )
            .map_err(to_bridge_err)?;
            let conn = Arc::new(conn);
            let meta = py
                .detach(|| {
                    call_with_recovery(&conn, DEFAULT_TIMEOUT, "target metadata", || {
                        rpc_call(
                            DEFAULT_TIMEOUT,
                            "target metadata",
                            conn.session()?.target_metadata(),
                        )
                    })
                })
                .map_err(to_bridge_err)?;
            open_state::register(&conn, target.to_string());
            Ok(Session {
                conn: Some(conn),
                meta,
            })
        })
    }
}

/// Extra RPC deadline headroom on top of any attach lease wait.
const ATTACH_MARGIN: Duration = Duration::from_secs(30);

/// Deadline for chip-erase style operations.
const ERASE_TIMEOUT: Duration = Duration::from_secs(300);

#[pymethods]
impl Session {
    /// Attach to a target by name through the probe-rs server.
    ///
    /// Converges: the server is started or verified on demand, and
    /// `wait_for_probe` (seconds) queues for the probe lease instead of
    /// failing immediately while another client holds it.
    #[staticmethod]
    #[pyo3(signature = (target, config=None, *, dry_run=false, wait_for_probe=None, connect_under_reset=false))]
    pub fn auto_attach(
        py: Python<'_>,
        target: &str,
        config: Option<SessionConfig>,
        dry_run: bool,
        wait_for_probe: Option<f64>,
        connect_under_reset: bool,
    ) -> PyResult<Session> {
        let cfg = config.unwrap_or(SessionConfig {
            speed: None,
            protocol: None,
            permissions: None,
        });
        let allow_erase = cfg.allow_erase_all();
        let wait = wait_for_probe.map(Duration::from_secs_f64);
        let deadline = wait.unwrap_or_default() + ATTACH_MARGIN;
        let conn = attach_with_heal(
            py,
            target,
            &cfg,
            None,
            allow_erase,
            dry_run,
            wait,
            connect_under_reset,
            deadline,
        )
        .map_err(to_bridge_err)?;
        let conn = Arc::new(conn);
        let meta = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "target metadata", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "target metadata",
                        conn.session()?.target_metadata(),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        open_state::register(&conn, target.to_string());
        Ok(Session {
            conn: Some(conn),
            meta,
        })
    }

    /// Get a CoreHandle for the core at `core_index`.
    pub fn core(slf: &Bound<'_, Self>, core_index: usize) -> PyResult<crate::core_::CoreHandle> {
        let this = slf.borrow();
        let count = this.meta.cores.len();
        if core_index >= count {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "core index {core_index} out of range (have {count} cores)"
            )));
        }
        let conn = this.conn()?;
        drop(this);
        Ok(crate::core_::CoreHandle::new(conn, core_index as u32))
    }

    /// List available cores as (index, CoreType) tuples.
    pub fn list_cores(&self) -> Vec<(usize, CoreTypeProxy)> {
        self.meta
            .cores
            .iter()
            .map(|c| (c.index as usize, core_type_to_py(rs_core_type(c.core_type))))
            .collect()
    }

    /// Get the target specification (resolved from the local builtin registry).
    pub fn target(&self) -> PyResult<Target> {
        builtin_target(&self.meta.target_name)
            .map(|t| Target { inner: t })
            .map_err(to_py_err)
    }

    /// Get the target architecture.
    pub fn architecture(&self) -> PyResult<ArchitectureProxy> {
        builtin_target(&self.meta.target_name)
            .map(|t| architecture_to_py(t.architecture()))
            .map_err(to_py_err)
    }

    /// Clear all hardware breakpoints across all cores.
    pub fn clear_all_hw_breakpoints(&mut self, py: Python<'_>) -> PyResult<()> {
        let conn = self.conn()?;
        // Collect before releasing the GIL: the lock guard is not Send.
        let per_core: Vec<(u32, Vec<u64>)> = conn.breakpoints.lock().drain().collect();
        py.detach(|| {
            for (core, addresses) in &per_core {
                if addresses.is_empty() {
                    continue;
                }
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "clear breakpoints", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "clear breakpoints",
                        conn.session()?
                            .core(*core as usize)
                            .clear_hw_breakpoints(addresses.clone()),
                    )
                })?;
            }
            Ok(())
        })
        .map_err(to_bridge_err)
    }

    /// Resume all cores.
    pub fn resume_all_cores(&mut self, py: Python<'_>) -> PyResult<()> {
        let conn = self.conn()?;
        py.detach(|| {
            call_with_recovery(&conn, DEFAULT_TIMEOUT, "resume all cores", || {
                rpc_call(
                    DEFAULT_TIMEOUT,
                    "resume all cores",
                    conn.session()?.resume_all_cores(),
                )
            })
        })
        .map_err(to_bridge_err)
    }

    /// Run the erase-all sequence (an alias of `erase_all`; the server
    /// applies the vendor sequence itself when one exists).
    pub fn sequence_erase_all(&mut self, py: Python<'_>) -> PyResult<()> {
        self.erase_all(py)
    }

    // ── SWO / Trace ────────────────────────────────────────────────

    /// Set up tracing for a core.
    pub fn setup_tracing(&mut self, _core_index: usize, _sink: TraceSinkProxy) -> PyResult<()> {
        Err(swo_unavailable())
    }

    /// Disable SWV for a core.
    pub fn disable_swv(&mut self, _core_index: usize) -> PyResult<()> {
        Err(swo_unavailable())
    }

    /// Add a SWV data trace unit.
    pub fn add_swv_data_trace(&mut self, _unit: usize, _addr: u64) -> PyResult<()> {
        Err(swo_unavailable())
    }

    /// Remove a SWV data trace unit.
    pub fn remove_swv_data_trace(&mut self, _unit: usize) -> PyResult<()> {
        Err(swo_unavailable())
    }

    /// Read available trace/SWO data.
    pub fn read_trace_data(&mut self) -> PyResult<Vec<u8>> {
        Err(swo_unavailable())
    }

    /// Read available SWO data.
    pub fn read_swo(&mut self) -> PyResult<Vec<u8>> {
        Err(swo_unavailable())
    }

    /// Erase all flash: the vendor erase-all sequence when the target
    /// provides one (the server decides), otherwise a full chip erase
    /// through the flash loader.
    pub fn erase_all(&mut self, py: Python<'_>) -> PyResult<()> {
        let conn = self.conn()?;
        py.detach(|| {
            call_with_recovery(&conn, ERASE_TIMEOUT, "erase all", || {
                rpc_call(
                    ERASE_TIMEOUT,
                    "erase all",
                    conn.session()?.erase_all(false, async |_| {}),
                )
            })
        })
        .map_err(to_bridge_err)
    }

    /// Flash an image and boot (the reflash-loop primitive).
    ///
    /// One call: resolve → build → (verify) → flash → boot → re-apply
    /// tracked hardware breakpoints. `verify=False` trades validation for
    /// iteration speed.
    #[pyo3(signature = (path, *, run=true, verify=false, progress=None))]
    pub fn flash(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
        path: &str,
        run: bool,
        verify: bool,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let this = slf.borrow();
        this.conn()?;
        crate::flashing::flash_and_run(py, &this, path, run, verify, progress)
    }

    // ── Debug / DWARF (RPC-only surface) ───────────────────────────

    /// Load an ELF with debug info for server-side DWARF operations
    /// (stack traces, variables, evaluate, source breakpoints).
    pub fn load_debug_info(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        let conn = self.conn()?;
        py.detach(|| {
            call_with_recovery(&conn, DEFAULT_TIMEOUT, "load debug info", || {
                rpc_call(
                    DEFAULT_TIMEOUT,
                    "load debug info",
                    conn.session()?
                        .load_debug_info(std::path::PathBuf::from(path)),
                )
            })
        })
        .map_err(to_bridge_err)?;
        // The reattach recovery re-pushes this file: the debug state lives
        // in the (new) server session.
        conn.debug_info
            .lock()
            .replace(std::path::PathBuf::from(path));
        Ok(())
    }

    /// Rich stack trace of `core` (server-side DWARF; call
    /// `load_debug_info` first). Returns frame dicts with keys
    /// `function, pc, inlined, file, line, column, id`.
    #[pyo3(signature = (core=0, limit=20))]
    pub fn stack_trace(&self, py: Python<'_>, core: u32, limit: u32) -> PyResult<Vec<PyObject>> {
        let conn = self.conn()?;
        let traces = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "stack trace", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "stack trace",
                        conn.session()?.take_rich_stack_trace(Some(core), limit),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        let mut out = Vec::new();
        for trace in traces.cores {
            for frame in trace.frames {
                out.push(rich_frame_dict(py, &frame)?);
            }
        }
        Ok(out)
    }

    /// Variable scopes for a stack frame (as returned by `stack_trace`):
    /// list of dicts with `name, variables_reference, line, column`.
    #[pyo3(signature = (core=0, frame=0))]
    pub fn scopes(&self, py: Python<'_>, core: u32, frame: u32) -> PyResult<Vec<PyObject>> {
        let conn = self.conn()?;
        let scopes = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "scopes", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "scopes",
                        conn.session()?.scopes(core, frame),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        scopes
            .into_iter()
            .map(|s| {
                py_dict(
                    py,
                    vec![
                        ("name", jv(py, s.name)?),
                        ("variables_reference", jv(py, s.variables_reference)?),
                        ("line", jv(py, s.line)?),
                        ("column", jv(py, s.column)?),
                    ],
                )
            })
            .collect::<PyResult<_>>()
    }

    /// Variables in a scope or container (from `scopes`/a variable's
    /// `variables_reference`): list of dicts with
    /// `name, value, type, variables_reference`.
    #[pyo3(signature = (variables_reference, core=0, filter=None))]
    pub fn variables(
        &self,
        py: Python<'_>,
        variables_reference: u32,
        core: u32,
        filter: Option<String>,
    ) -> PyResult<Vec<PyObject>> {
        let conn = self.conn()?;
        let vars = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "variables", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "variables",
                        conn.session()?
                            .variables(core, variables_reference, filter.clone()),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        vars.into_iter()
            .map(|v| {
                py_dict(
                    py,
                    vec![
                        ("name", jv(py, v.name)?),
                        ("value", jv(py, v.value)?),
                        ("type", jv(py, v.type_)?),
                        ("variables_reference", jv(py, v.variables_reference)?),
                        ("memory_reference", jv(py, v.memory_reference)?),
                    ],
                )
            })
            .collect::<PyResult<_>>()
    }

    /// Evaluate an expression against server-side DWARF (the REPL `p`):
    /// dict with `result, type, variables_reference, memory_reference`.
    #[pyo3(signature = (expression, core=0, frame=None))]
    pub fn evaluate(
        &self,
        py: Python<'_>,
        expression: &str,
        core: u32,
        frame: Option<u32>,
    ) -> PyResult<PyObject> {
        let conn = self.conn()?;
        let resp = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "evaluate", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "evaluate",
                        conn.session()?
                            .evaluate(core, frame, expression.to_string()),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        py_dict(
            py,
            vec![
                ("result", jv(py, resp.result)?),
                ("type", jv(py, resp.type_)?),
                ("variables_reference", jv(py, resp.variables_reference)?),
                ("memory_reference", jv(py, resp.memory_reference)?),
            ],
        )
    }

    /// Load a CMSIS-SVD file for peripheral register names on `core`
    /// (or `None` to clear). The client uploads the file for remote servers
    /// and reads it in place for local ones — never hand a server-side path
    /// back through this API.
    #[pyo3(signature = (path, core=0))]
    pub fn load_svd(&self, py: Python<'_>, path: &str, core: u32) -> PyResult<()> {
        let conn = self.conn()?;
        py.detach(|| {
            call_with_recovery(&conn, DEFAULT_TIMEOUT, "load svd", || {
                rpc_call(
                    DEFAULT_TIMEOUT,
                    "load svd",
                    conn.session()?
                        .load_svd(core, Some(std::path::PathBuf::from(path))),
                )
            })
        })
        .map_err(to_bridge_err)
    }

    /// Resolve source file lines to verified breakpoint addresses
    /// (requires `load_debug_info`): list of dicts with
    /// `address, file, line` or `error`.
    pub fn resolve_source_breakpoints(
        &self,
        py: Python<'_>,
        file: &str,
        lines: Vec<u64>,
    ) -> PyResult<Vec<PyObject>> {
        let conn = self.conn()?;
        let locations: Vec<probe_rs_rpc::breakpoints::SourceBreakpointLocation> = lines
            .into_iter()
            .map(|line| probe_rs_rpc::breakpoints::SourceBreakpointLocation {
                path: file.to_string(),
                line,
                column: None,
            })
            .collect();
        let res = py
            .detach(|| {
                call_with_recovery(&conn, DEFAULT_TIMEOUT, "resolve breakpoints", || {
                    rpc_call(
                        DEFAULT_TIMEOUT,
                        "resolve breakpoints",
                        conn.session()?
                            .resolve_source_breakpoints(locations.clone()),
                    )
                })
            })
            .map_err(to_bridge_err)?;
        res.into_iter()
            .map(|r| match r.breakpoint {
                Some(bp) => py_dict(
                    py,
                    vec![
                        ("address", jv(py, bp.address)?),
                        ("file", jv(py, bp.source_location.path)?),
                        ("line", jv(py, bp.source_location.line.map(|v| v as i64))?),
                    ],
                ),
                None => py_dict(
                    py,
                    vec![
                        ("address", jv(py, Option::<u64>::None)?),
                        ("error", jv(py, r.error.unwrap_or_default())?),
                    ],
                ),
            })
            .collect::<PyResult<_>>()
    }

    /// Clear cached debug state for a core (e.g. after a reflash made
    /// frame/variable handles stale).
    #[pyo3(signature = (core=0))]
    pub fn clear_debug_state(&self, py: Python<'_>, core: u32) -> PyResult<()> {
        let conn = self.conn()?;
        py.detach(|| {
            call_with_recovery(&conn, DEFAULT_TIMEOUT, "clear debug state", || {
                rpc_call(
                    DEFAULT_TIMEOUT,
                    "clear debug state",
                    conn.session()?.clear_core_debug_state(core),
                )
            })
        })
        .map_err(to_bridge_err)
    }

    // ── RTT ────────────────────────────────────────────────────────

    /// Attach to RTT on the given core, optionally at a specific pointer.
    #[pyo3(signature = (core_index=0, ptr=None))]
    pub fn attach_rtt(
        slf: &Bound<'_, Self>,
        core_index: usize,
        ptr: Option<u64>,
    ) -> PyResult<crate::rtt_::Rtt> {
        let conn = slf.borrow().conn()?;
        crate::rtt_::attach(conn, core_index as u32, ptr)
    }

    /// Close the session (idempotent). Later calls on this session and its
    /// handles fail fast; the probe lease is released when the transport
    /// drops — immediately when idle, otherwise as soon as the in-flight
    /// operation ends (its own deadline bounds the wait). close() itself
    /// never blocks, so it is safe from RTT/flash progress callbacks.
    pub fn close(&mut self) {
        if let Some(conn) = self.conn.take() {
            conn.sever();
        }
    }

    pub fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    pub fn __exit__(
        &mut self,
        _exc_type: &Bound<'_, pyo3::types::PyAny>,
        _exc_value: &Bound<'_, pyo3::types::PyAny>,
        _traceback: &Bound<'_, pyo3::types::PyAny>,
    ) -> PyResult<()> {
        self.close();
        Ok(())
    }

    fn __repr__(&self) -> String {
        match &self.conn {
            Some(_) => format!(
                "Session(target={}, cores={})",
                self.meta.target_name,
                self.meta.cores.len()
            ),
            None => format!("Session(target={}, closed)", self.meta.target_name),
        }
    }
}

/// Resolve a target from the builtin registry; used where the facade mirrors
fn builtin_target(name: &str) -> Result<RsTarget, probe_rs::config::RegistryError> {
    RsRegistry::from_builtin_families().get_target_by_name(name)
}

/// Map the wire core type onto the local registry type for the Python proxy.
fn rs_core_type(w: WireCoreType) -> CoreType {
    use WireCoreType as W;
    match w {
        W::Armv6m => CoreType::Armv6m,
        W::Armv7a => CoreType::Armv7a,
        W::Armv7r => CoreType::Armv7r,
        W::Armv7m => CoreType::Armv7m,
        W::Armv7em => CoreType::Armv7em,
        W::Armv8a => CoreType::Armv8a,
        W::Armv8m => CoreType::Armv8m,
        W::Riscv => CoreType::Riscv,
        W::Riscv64 => CoreType::Riscv64,
        W::Xtensa => CoreType::Xtensa,
    }
}

/// SWO/SWV tracing has no RPC endpoints yet; the direct-probe path was
/// removed with the RPC cutover.
fn swo_unavailable() -> PyErr {
    py_err_with_kind(
        "SWO/SWV tracing is not available over the RPC interface; it needs a \
         direct probe session (planned once probe-rs adds tracing endpoints)"
            .into(),
        "NotImplemented",
    )
}

/// Attach helper shared by the first attempt and the heal retry.
// Every parameter is a distinct attach-policy axis shared by both callers.
#[allow(clippy::too_many_arguments)]
fn attach_with(
    py: Python<'_>,
    target: &str,
    cfg: &SessionConfig,
    selector: Option<WireProbeSelector>,
    allow_erase: bool,
    dry_run: bool,
    wait: Option<Duration>,
    connect_under_reset: bool,
    deadline: Duration,
) -> Result<Connection, BridgeError> {
    py.detach(|| {
        let client = open_state::ensure_server_client()?;
        let entry = resolve_probe_entry(&client, selector.as_ref(), dry_run)?;
        let request = AttachRequest {
            chip: Some(target.to_string()),
            protocol: cfg.wire_protocol(),
            probe: entry,
            speed: cfg.speed,
            connect_under_reset,
            dry_run,
            allow_erase_all: allow_erase,
            resume_target: false,
            wait_for_probe: wait,
        };
        // The reattach recovery rebuilds the transport from the same source
        // (module-managed server or remote host) as the original attach.
        let (host, token) = open_state::remote_config();
        let source = match host {
            Some(host) => crate::rpc::ConnSource::Remote { host, token },
            None => crate::rpc::ConnSource::Local,
        };
        Bridge::global()?
            .wait(
                deadline,
                "attach",
                Connection::attach(client, request, source),
            )
            .and_then(|inner| inner)
    })
}

/// Pick a probe entry for attach, honoring an optional selector. Dry runs
/// accept a synthetic entry because the server swaps in a fake probe and
/// claims no device.
fn resolve_probe_entry(
    client: &RpcClient,
    selector: Option<&WireProbeSelector>,
    dry_run: bool,
) -> Result<DebugProbeEntry, BridgeError> {
    let probes = rpc_call(DEFAULT_TIMEOUT, "probe list", client.list_probes())?;
    let found = match selector {
        Some(sel) => probes.into_iter().find(|p| selector_matches(p, sel)),
        None => probes.into_iter().find(|p| !p.inaccessible),
    };
    if let Some(entry) = found {
        return Ok(entry);
    }
    if dry_run {
        return Ok(DebugProbeEntry {
            identifier: "dry-run".into(),
            vendor_id: 0,
            product_id: 0,
            interface: None,
            serial_number: "dry-run".into(),
            probe_type: "dry-run".into(),
            inaccessible: false,
        });
    }
    Err(BridgeError::Attach(
        "no accessible debug probe found; connect one and check pyprobe_rs.list_probes()".into(),
        "Probe",
    ))
}

/// Wire-selector match: populated selector fields must equal the entry.
fn selector_matches(entry: &DebugProbeEntry, sel: &WireProbeSelector) -> bool {
    entry.vendor_id == sel.vendor_id
        && entry.product_id == sel.product_id
        && sel
            .serial_number
            .as_deref()
            .is_none_or(|s| entry.serial_number == s)
        && sel.interface.is_none_or(|i| entry.interface == Some(i))
}

/// Build a Python dict from `(key, erased value)` pairs.
fn py_dict(py: Python<'_>, entries: Vec<(&'static str, PyObject)>) -> PyResult<PyObject> {
    let dict = pyo3::types::PyDict::new(py);
    for (key, value) in entries {
        dict.set_item(key, value)?;
    }
    Ok(dict.into_any().unbind())
}

/// Erase a value into a type-homogeneous dict entry.
fn jv<'py, V: pyo3::IntoPyObject<'py> + pyo3::IntoPyObjectExt<'py>>(
    py: Python<'py>,
    value: V,
) -> PyResult<Py<PyAny>> {
    value.into_py_any(py)
}

/// Convert a rich stack frame to a plain dict for Python consumption.
fn rich_frame_dict(
    py: Python<'_>,
    frame: &probe_rs_rpc::stack_trace::RichStackTraceFrame,
) -> PyResult<PyObject> {
    let (file, line, column) = frame.location.as_ref().map_or((None, None, None), |l| {
        (Some(l.file.clone()), l.line, l.column)
    });
    py_dict(
        py,
        vec![
            ("function", jv(py, frame.function_name.clone())?),
            ("pc", jv(py, wire_value_u64(&frame.program_counter))?),
            ("inlined", jv(py, frame.is_inlined)?),
            ("file", jv(py, file)?),
            ("line", jv(py, line.map(|v| v as i64))?),
            ("column", jv(py, column.map(|v| v as i64))?),
            ("id", jv(py, frame.id)?),
        ],
    )
}

/// Flatten a wire register value to u64 (truncating 128-bit values).
fn wire_value_u64(value: &probe_rs_rpc::core_ops::WireRegisterValue) -> u64 {
    match value {
        probe_rs_rpc::core_ops::WireRegisterValue::U32(x) => *x as u64,
        probe_rs_rpc::core_ops::WireRegisterValue::U64(x) => *x,
        probe_rs_rpc::core_ops::WireRegisterValue::U128(x) => *x as u64,
    }
}
