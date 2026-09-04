//! Flashing: download_file, FlashLoader, DownloadOptions, progress callbacks.
//!
//! All flashing operations run with the Python interpreter blocked (the GIL is
//! held for the whole call): probe-rs's `FlashProgress` handler is not `Send`,
//! so the operation cannot be moved out from under pyo3's `allow_threads`.
//! A long flash therefore freezes interactive loops (e.g. Jupyter) until done.

use crate::error::to_py_err;
use crate::session::Session;
use pyo3::prelude::*;
use probe_rs::flashing::{
    BinLoader, BinOptions, DownloadOptions as RsDownloadOptions, FlashProgress,
    FlashLoader as RsFlashLoader, ImageLoader, ProgressEvent as RsProgressEvent,
};

/// Options for a flash download operation.
#[pyclass]
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
#[pyclass]
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

fn event_to_py(e: &RsProgressEvent) -> ProgressEvent {
    match e {
        RsProgressEvent::FlashLayoutReady { .. } => ProgressEvent {
            kind: "FlashLayoutReady".into(),
            operation: "layout".into(),
            size: 0,
            total: None,
            time_s: 0.0,
        },
        RsProgressEvent::AddProgressBar { operation, total } => ProgressEvent {
            kind: "AddProgressBar".into(),
            operation: format!("{:?}", operation),
            size: 0,
            total: *total,
            time_s: 0.0,
        },
        RsProgressEvent::Started(op) => ProgressEvent {
            kind: "Started".into(),
            operation: format!("{:?}", op),
            size: 0,
            total: None,
            time_s: 0.0,
        },
        RsProgressEvent::Progress { operation, size, time } => ProgressEvent {
            kind: "Progress".into(),
            operation: format!("{:?}", operation),
            size: *size,
            total: None,
            time_s: time.as_secs_f32(),
        },
        RsProgressEvent::Finished(op) => ProgressEvent {
            kind: "Finished".into(),
            operation: format!("{:?}", op),
            size: 0,
            total: None,
            time_s: 0.0,
        },
        RsProgressEvent::Failed(op) => ProgressEvent {
            kind: "Failed".into(),
            operation: format!("{:?}", op),
            size: 0,
            total: None,
            time_s: 0.0,
        },
        RsProgressEvent::DiagnosticMessage { message } => ProgressEvent {
            kind: "DiagnosticMessage".into(),
            operation: message.clone(),
            size: 0,
            total: None,
            time_s: 0.0,
        },
    }
}

/// Wire a Python callable into a `FlashProgress` handler.
///
/// The callback runs synchronously while the operation holds the Session
/// mutably borrowed, so it must not call back into pyprobe-rs session/core
/// APIs. Exceptions it raises cannot abort the operation (the handler API is
/// infallible); they are printed to stderr instead of being swallowed.
fn flash_progress(callback: Py<PyAny>) -> FlashProgress<'static> {
    FlashProgress::new(move |e: RsProgressEvent| {
        Python::attach(|py| {
            let event = event_to_py(&e);
            if let Err(err) = callback.call1(py, (event,)) {
                err.print(py);
            }
        })
    })
}

/// Map Python DownloadOptions onto the Rust struct, attaching the progress
/// callback when provided.
fn rs_download_options(
    opts: &DownloadOptions,
    progress: Option<Py<PyAny>>,
) -> RsDownloadOptions<'static> {
    let mut rs_opts = RsDownloadOptions::default();
    rs_opts.keep_unwritten_bytes = opts.keep_unwritten_bytes;
    rs_opts.dry_run = opts.dry_run;
    rs_opts.do_chip_erase = opts.do_chip_erase;
    rs_opts.skip_erase = opts.skip_erase;
    rs_opts.preverify = opts.preverify;
    rs_opts.verify = opts.verify;
    rs_opts.disable_double_buffering = opts.disable_double_buffering;
    rs_opts.ram_chunk_size = opts.ram_chunk_size;
    if let Some(cb) = progress {
        rs_opts.progress = flash_progress(cb);
    }
    rs_opts
}

/// Build the image loader for `format`. `base_address`/`skip` position a raw
/// `bin` image and are rejected for other formats.
fn build_image_loader(
    format: &str,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<Box<dyn ImageLoader>> {
    if (base_address.is_some() || skip != 0)
        && !matches!(format.to_lowercase().as_str(), "bin" | "binary")
    {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "base_address/skip only apply to the 'bin' image format",
        ));
    }
    if matches!(format.to_lowercase().as_str(), "bin" | "binary") {
        return Ok(Box::new(BinLoader(BinOptions {
            base_address,
            skip,
        })));
    }
    probe_rs::flashing::image_format(format)
        .map(|fmt| fmt.create_loader(None))
        .ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("unknown image format: {format}"))
        })
}

/// FlashLoader wrapper for staged flash writes.
#[pyclass(unsendable)]
pub struct FlashLoader {
    inner: RsFlashLoader,
}

#[pymethods]
impl FlashLoader {
    pub fn add_data(&mut self, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.inner.add_data(addr, &data).map_err(to_py_err)
    }

    #[pyo3(signature = (session, options=None, progress=None))]
    pub fn commit(
        &mut self,
        session: &mut Session,
        options: Option<DownloadOptions>,
        progress: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        let opts = options.unwrap_or_default();
        let rs_opts = rs_download_options(&opts, progress);
        let sess = &mut session.inner;
        self.inner.commit(sess, rs_opts).map_err(to_py_err)
    }
}

/// Download a firmware file to the target's flash.
///
/// `base_address`/`skip` position a raw `bin` image and are rejected for
/// other formats. `progress`, when given, receives ProgressEvent objects; it
/// must not touch the session and its exceptions are printed to stderr.
///
/// Blocks the Python interpreter for the duration of the flash (see module docs).
#[pyfunction]
#[pyo3(signature = (session, path, format, options=None, progress=None, *, base_address=None, skip=0))]
pub fn download_file(
    session: &mut Session,
    path: &str,
    format: &str,
    options: Option<DownloadOptions>,
    progress: Option<Py<PyAny>>,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<()> {
    let loader = build_image_loader(format, base_address, skip)?;
    let opts = options.unwrap_or_default();
    let rs_opts = rs_download_options(&opts, progress);
    let sess = &mut session.inner;
    probe_rs::flashing::download_file_with_options(sess, path, loader, rs_opts)
        .map_err(to_py_err)
}

/// Build a FlashLoader for a session + image.
///
/// `base_address`/`skip` position a raw `bin` image and are rejected for
/// other formats.
#[pyfunction]
#[pyo3(signature = (session, path, format, *, base_address=None, skip=0))]
pub fn build_loader(
    session: &mut Session,
    path: &str,
    format: &str,
    base_address: Option<u64>,
    skip: u32,
) -> PyResult<FlashLoader> {
    let loader = build_image_loader(format, base_address, skip)?;
    let sess = &mut session.inner;
    let rs_loader =
        probe_rs::flashing::build_loader(sess, path, loader, None).map_err(to_py_err)?;
    Ok(FlashLoader { inner: rs_loader })
}
