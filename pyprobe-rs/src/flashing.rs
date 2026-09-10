//! Flashing: download_file, build_loader/FlashLoader, DownloadOptions, progress callbacks.
//!
//! Flash operations are RPC round-trips on the shared bridge runtime with the
//! interpreter detached (`Python::detach`), so a long flash no longer freezes
//! interactive Python loops. Progress events arrive on the RPC stream and are
//! forwarded to the Python callback via `Python::attach`; callback exceptions
//! are printed to stderr and never abort the operation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use probe_rs_rpc::flash::{
    BootInfo, DownloadOptions as WireDownloadOptions, Operation as WireOperation,
    ProgressEvent as WireProgressEvent, VerifyResult,
};
use probe_rs_rpc::format::{BinaryCliOptions, FormatKind, FormatOptions};
use probe_rs_rpc::{FlashLoader as FlashLoaderKey, Key};
use pyo3::prelude::*;

use crate::error::{BridgeError, to_bridge_err};
use crate::events::Pump;
use crate::rpc::{Connection, DEFAULT_TIMEOUT, call_with_recovery, rpc_call};
use crate::session::Session;

/// Deadline for programming and verification: a full read-back on a large
/// device takes minutes on slow probes, so the bridge deadline must be
/// generous, not just the request-level one.
const FLASH_TIMEOUT: Duration = Duration::from_secs(600);
/// Deadline for resolving and building an image; covers the remote temp-file
/// upload path, not just the local in-place read.
const BUILD_TIMEOUT: Duration = Duration::from_secs(300);

/// Options for a flash download operation.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct DownloadOptions {
    #[pyo3(get, set)]
    pub keep_unwritten_bytes: bool,
    #[pyo3(get, set)]
    pub dry_run: bool,
    #[pyo3(get, set)]
    pub do_chip_erase: bool,
    #[pyo3(get, set)]
    pub skip_erase: bool,
    #[pyo3(get, set)]
    pub preverify: bool,
    #[pyo3(get, set)]
    pub verify: bool,
    #[pyo3(get, set)]
    pub disable_double_buffering: bool,
    #[pyo3(get, set)]
    pub ram_chunk_size: Option<u64>,
}

#[pymethods]
impl DownloadOptions {
    #[new]
    pub fn new() -> Self {
        Self {
            keep_unwritten_bytes: true,
            dry_run: false,
            do_chip_erase: false,
            skip_erase: false,
            preverify: false,
            verify: true,
            disable_double_buffering: false,
            ram_chunk_size: None,
        }
    }
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Flash progress event forwarded to Python callbacks.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct ProgressEvent {
    #[pyo3(get)]
    pub kind: String,
    #[pyo3(get)]
    pub operation: String,
    #[pyo3(get)]
    pub size: u64,
    #[pyo3(get)]
    pub total: Option<u64>,
    #[pyo3(get)]
    pub time_s: f32,
}

/// Wire flash operation name, as the Python proxy's `operation` field. The
/// wire enum derives no `Debug`, so the names are spelled out.
fn operation_name(op: &WireOperation) -> &'static str {
    match op {
        WireOperation::Fill => "Fill",
        WireOperation::Erase => "Erase",
        WireOperation::Program => "Program",
        WireOperation::Verify => "Verify",
        WireOperation::Ram => "Ram",
    }
}

/// Map a wire progress event onto the Python proxy. The wire stream carries
/// no timestamps, so `time_s` is always 0 — the field stays for the
/// Python-visible shape.
fn event_to_py(e: &WireProgressEvent) -> ProgressEvent {
    let (kind, operation, size, total, time_s) = match e {
        WireProgressEvent::FlashLayoutReady { .. } => {
            ("FlashLayoutReady", "layout".into(), 0, None, 0.0)
        }
        WireProgressEvent::AddProgressBar { operation, total } => (
            "AddProgressBar",
            operation_name(operation).to_string(),
            0,
            *total,
            0.0,
        ),
        WireProgressEvent::Started(op) => ("Started", operation_name(op).to_string(), 0, None, 0.0),
        WireProgressEvent::Progress { operation, size } => (
            "Progress",
            operation_name(operation).to_string(),
            *size,
            None,
            0.0,
        ),
        WireProgressEvent::Finished(op) => {
            ("Finished", operation_name(op).to_string(), 0, None, 0.0)
        }
        WireProgressEvent::Failed(op) => ("Failed", operation_name(op).to_string(), 0, None, 0.0),
        WireProgressEvent::DiagnosticMessage { message } => {
            ("DiagnosticMessage", message.clone(), 0, None, 0.0)
        }
    };
    ProgressEvent {
        kind: kind.into(),
        operation,
        size,
        total,
        time_s,
    }
}

/// Spawn the progress pump for one flash call, if a callback was given.
///
/// Events are delivered from a dedicated pump thread: the callback runs
/// outside the flash future's poll context, so it may call back into
/// pyprobe_rs (halt, memory reads, RTT writes) without nesting `block_on`
/// inside the entered runtime and panicking.
fn progress_pump(progress: Option<Py<PyAny>>) -> Option<Pump<WireProgressEvent>> {
    progress.map(|callback| {
        Pump::spawn(move |event: WireProgressEvent| deliver_progress(&callback, event))
    })
}

/// Forwarder handed to the streamed RPC calls: non-blocking enqueue.
fn progress_forward(
    pump: Option<&Pump<WireProgressEvent>>,
) -> impl AsyncFnMut(WireProgressEvent) + '_ {
    async move |event: WireProgressEvent| {
        if let Some(pump) = pump {
            pump.send(event);
        }
    }
}

/// Convert and deliver one progress event to the Python callback.
/// Exceptions are printed to stderr, never propagated: the RPC stream has
/// no error channel, and a misbehaving callback must not kill a flash.
fn deliver_progress(callback: &Py<PyAny>, event: WireProgressEvent) {
    // Python::attach panics during interpreter finalization (pyo3, Py 3.13+);
    // the pump thread must never take the process down at shutdown.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        Python::attach(|py| {
            let event = event_to_py(&event);
            if let Err(err) = callback.call1(py, (event,)) {
                err.print(py);
            }
        });
    }));
}

/// Map the Python `format` string and bin positioning onto the wire
/// `FormatOptions`. `base_address`/`skip` position a raw `bin` image and are
/// rejected for other formats.
fn wire_format_options(
    format: &str,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<FormatOptions> {
    let kind = FormatKind::from_optional(Some(format)).map_err(|_| {
        pyo3::exceptions::PyValueError::new_err(format!("unknown image format: {format}"))
    })?;
    if (base_address.is_some() || skip != 0) && kind != FormatKind::Bin {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "base_address/skip only apply to the 'bin' image format",
        ));
    }
    Ok(FormatOptions {
        binary_format: kind,
        bin_options: BinaryCliOptions { base_address, skip },
        idf_options: Default::default(),
        elf_options: Default::default(),
    })
}

/// Map the Python options onto the wire struct. `dry_run` has no
/// per-download wire form — the server derives it from the attached
/// session — so a user-set `dry_run` is rejected loudly rather than
/// silently ignored on a destructive operation. `preverify` is implemented
/// client-side via `SessionInterface::verify` (skip the write when the
/// image already matches flash).
fn wire_download_options(opts: &DownloadOptions) -> PyResult<WireDownloadOptions> {
    if opts.dry_run {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "DownloadOptions.dry_run is no longer a per-download switch; \
             attach a hardwareless session with \
             Session.auto_attach(..., dry_run=True) instead",
        ));
    }
    Ok(WireDownloadOptions {
        keep_unwritten_bytes: opts.keep_unwritten_bytes,
        do_chip_erase: opts.do_chip_erase,
        skip_erase: opts.skip_erase,
        verify: opts.verify,
        disable_double_buffering: opts.disable_double_buffering,
        preferred_algos: Vec::new(),
        ram_chunk_size: opts.ram_chunk_size,
    })
}

/// Flash `path` over `conn`: resolve → build → (pre-verify) → program → boot.
///
/// When `preverify` is set and the server reports the image already matches
/// flash, the program step is skipped (upstream `preverify` semantics). When
/// `run` is set, all cores resume after boot; otherwise they stay halted.
fn flash_image(
    conn: &Arc<Connection>,
    path: &str,
    format: FormatOptions,
    wire: WireDownloadOptions,
    preverify: bool,
    run: bool,
    progress: Option<Py<PyAny>>,
) -> Result<(), BridgeError> {
    // Server-side objects (loader, boot info) belong to the session that
    // built them, so a dead-link recovery (target power cycle, USB replug)
    // restarts the whole pipeline once instead of resuming a stage.
    call_with_recovery(conn, FLASH_TIMEOUT, "flash", || {
        flash_once(
            conn,
            path,
            format.clone(),
            wire.clone(),
            preverify,
            run,
            progress.clone(),
        )
    })
}

/// One full flash pipeline attempt: resolve → build → (pre-verify) →
/// program → boot.
#[allow(clippy::too_many_arguments)]
fn flash_once(
    conn: &Arc<Connection>,
    path: &str,
    format: FormatOptions,
    wire: WireDownloadOptions,
    preverify: bool,
    run: bool,
    progress: Option<Py<PyAny>>,
) -> Result<(), BridgeError> {
    // Localhost resolves the path in place (zero copy); a remote server gets
    // the image via its temp-file upload path.
    let upload = rpc_call(
        BUILD_TIMEOUT,
        "resolve upload",
        conn.session()?.resolve_upload(Path::new(path)),
    )?;
    let pump = progress_pump(progress);
    let built = rpc_call(
        BUILD_TIMEOUT,
        "build flash loader",
        conn.session()?
            .build_flash_loader_resolved(&upload, format, None, false, None),
    )?;

    let mut program = true;
    if preverify {
        program = matches!(
            rpc_call(
                FLASH_TIMEOUT,
                "pre-verify flash",
                conn.session()?
                    .verify(built.loader, progress_forward(pump.as_ref())),
            )?,
            VerifyResult::Mismatch
        );
    }
    if program {
        rpc_call(
            FLASH_TIMEOUT,
            "flash",
            conn.session()?
                .flash(wire, built.loader, progress_forward(pump.as_ref())),
        )?;
    }
    // A dry-run session's boot sequence times out on the mocked core; the
    // dry run ends with the cores halted.
    if conn.dry_run {
        return Ok(());
    }
    // The two boot futures are distinct opaque types, so each branch runs
    // its own bridge call instead of unifying into one.
    let session = conn.session()?;
    if run {
        rpc_call(DEFAULT_TIMEOUT, "boot", session.boot(built.boot_info, 0))?;
    } else {
        rpc_call(
            DEFAULT_TIMEOUT,
            "boot",
            session.prepare_boot(built.boot_info, 0),
        )?;
    }
    Ok(())
}

/// Re-apply tracked hardware breakpoints on every core that has them. A
/// failure is logged, not raised: the flash itself succeeded, and the next
/// successful iteration restores the breakpoints. The GIL is released for
/// the round-trips like every other blocking bridge call.
fn reapply_breakpoints(py: Python<'_>, conn: &Arc<Connection>) {
    let cores: Vec<u32> = conn.breakpoints.lock().keys().copied().collect();
    py.detach(|| {
        for core in cores {
            if let Err(err) = crate::core_::reapply_breakpoints(conn, core) {
                eprintln!("pyprobe-rs: failed to re-apply breakpoints on core {core}: {err}");
            }
        }
    });
}

/// Download a firmware file to the target's flash and run it.
///
/// `base_address`/`skip` position a raw `bin` image and are rejected for
/// other formats. `options.preverify` skips programming entirely when the
/// server reports the image already matches flash. `progress`, when given,
/// receives ProgressEvent objects on a dedicated callback thread (it may
/// call session/core/RTT APIs) and its exceptions are printed to stderr.
///
/// The GIL is released for the RPC traffic, so interactive Python loops stay
/// responsive while the flash runs.
#[pyfunction]
#[pyo3(signature = (session, path, format, options=None, progress=None, *, base_address=None, skip=0))]
// Python API surface: the download options + progress callback are part of
// the public call shape.
#[allow(clippy::too_many_arguments)]
pub fn download_file(
    py: Python<'_>,
    session: &Session,
    path: &str,
    format: &str,
    options: Option<DownloadOptions>,
    progress: Option<Py<PyAny>>,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<()> {
    let conn = session.conn()?;
    let format_opts = wire_format_options(format, base_address, skip)?;
    let opts = options.unwrap_or_default();
    let wire = wire_download_options(&opts)?;
    let conn2 = Arc::clone(&conn);
    py.detach(move || {
        flash_image(
            &conn2,
            path,
            format_opts,
            wire,
            opts.preverify,
            true,
            progress,
        )
    })
    .map_err(to_bridge_err)?;
    reapply_breakpoints(py, &conn);
    Ok(())
}

/// Flash-and-run iteration primitive backing `Session.flash`.
///
/// Flashes `path`, then boots with `run` deciding whether the cores resume.
/// `verify` does both halves of verification: the image is pre-checked
/// against flash (an unchanged image skips the program step entirely, which
/// is what makes the default `verify=False` the fast iteration path), and a
/// program that does run is read back afterwards. Tracked hardware
/// breakpoints are re-applied after boot because the reset clears the debug
/// unit while our session stays alive.
pub(crate) fn flash_and_run(
    py: Python<'_>,
    sess: &Session,
    path: &str,
    run: bool,
    verify: bool,
    progress: Option<Py<PyAny>>,
) -> PyResult<()> {
    let conn = sess.conn()?;
    // Iteration defaults: preserve unwritten flash (matches
    // `DownloadOptions::new`) and let the server resolve the image format
    // from the target metadata.
    let wire = WireDownloadOptions {
        keep_unwritten_bytes: true,
        verify,
        ..WireDownloadOptions::default()
    };
    let format = FormatOptions {
        binary_format: FormatKind::Target,
        ..FormatOptions::default()
    };
    let conn2 = Arc::clone(&conn);
    py.detach(move || flash_image(&conn2, path, format, wire, verify, run, progress))
        .map_err(to_bridge_err)?;
    reapply_breakpoints(py, &conn);
    Ok(())
}

/// FlashLoader wrapper for staged flash writes.
///
/// The image staged by `build_loader` lives server-side; `add_data` patches
/// are accumulated client-side and staged at `commit`.
#[pyclass]
pub struct FlashLoader {
    /// Server-side loader object holding the staged image.
    key: Key<FlashLoaderKey>,
    /// Boot info from the server build; drives the boot in `commit`.
    boot_info: BootInfo,
    /// `add_data` patches, staged via load_region at commit.
    segments: Vec<(u64, Vec<u8>)>,
}

#[pymethods]
impl FlashLoader {
    /// Accumulate a patch. Region validation happens server-side when the
    /// patch is staged at `commit`.
    pub fn add_data(&mut self, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.segments.push((addr, data));
        Ok(())
    }

    /// Flash the staged image plus any add_data patches, then boot it.
    ///
    /// `options.preverify` skips programming when the image already matches
    /// flash. The GIL is released during the RPC traffic.
    #[pyo3(signature = (session, options=None, progress=None))]
    pub fn commit(
        &mut self,
        py: Python<'_>,
        session: &Session,
        options: Option<DownloadOptions>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let conn = session.conn()?;
        let opts = options.unwrap_or_default();
        let wire = wire_download_options(&opts)?;
        let key = self.key;
        let boot_info = self.boot_info.clone();
        let segments = std::mem::take(&mut self.segments);
        let pump = progress_pump(progress);
        py.detach(|| {
            // Stage the add_data patches before the image goes down.
            for (addr, data) in &segments {
                rpc_call(
                    FLASH_TIMEOUT,
                    "stage flash data",
                    conn.session()?.load_region(key, *addr, data.clone()),
                )?;
            }
            let mut program = true;
            if opts.preverify {
                program = matches!(
                    rpc_call(
                        FLASH_TIMEOUT,
                        "pre-verify flash",
                        conn.session()?.verify(key, progress_forward(pump.as_ref())),
                    )?,
                    VerifyResult::Mismatch
                );
            }
            if program {
                rpc_call(
                    FLASH_TIMEOUT,
                    "flash",
                    conn.session()?
                        .flash(wire, key, progress_forward(pump.as_ref())),
                )?;
            }
            // A dry-run session's boot sequence times out on the mocked core.
            if !conn.dry_run {
                rpc_call(DEFAULT_TIMEOUT, "boot", conn.session()?.boot(boot_info, 0))?;
            }
            Ok(())
        })
        .map_err(to_bridge_err)?;
        reapply_breakpoints(py, &conn);
        Ok(())
    }
}

/// Build a FlashLoader for a session + image.
///
/// The image is staged server-side (the server resolves the chip and builds
/// the flash layout); `add_data` patches layer on top and `commit` flashes.
/// `base_address`/`skip` position a raw `bin` image and are rejected for
/// other formats.
#[pyfunction]
#[pyo3(signature = (session, path, format, *, base_address=None, skip=0))]
pub fn build_loader(
    py: Python<'_>,
    session: &Session,
    path: &str,
    format: &str,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<FlashLoader> {
    let conn = session.conn()?;
    let format_opts = wire_format_options(format, base_address, skip)?;
    let built = py
        .detach(|| {
            let upload = rpc_call(
                BUILD_TIMEOUT,
                "resolve upload",
                conn.session()?.resolve_upload(Path::new(path)),
            )?;
            rpc_call(
                BUILD_TIMEOUT,
                "build flash loader",
                conn.session()?.build_flash_loader_resolved(
                    &upload,
                    format_opts,
                    None,
                    false,
                    None,
                ),
            )
        })
        .map_err(to_bridge_err)?;
    Ok(FlashLoader {
        key: built.loader,
        boot_info: built.boot_info,
        segments: Vec::new(),
    })
}
