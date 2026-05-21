//! Flashing: download_file, FlashLoader, DownloadOptions, progress callbacks.

use crate::error::to_py_err;
use crate::session::Session;
use pyo3::prelude::*;
use probe_rs::flashing::{
    DownloadOptions as RsDownloadOptions, FlashProgress, FlashLoader as RsFlashLoader,
    ProgressEvent as RsProgressEvent,
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
    pub preferred_algos: Vec<String>,
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
            preferred_algos: Vec::new(),
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
        let sess = session
            .inner
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Session consumed"))?;
        let mut rs_opts = RsDownloadOptions::default();
        rs_opts.keep_unwritten_bytes = opts.keep_unwritten_bytes;
        rs_opts.dry_run = opts.dry_run;
        rs_opts.do_chip_erase = opts.do_chip_erase;
        rs_opts.skip_erase = opts.skip_erase;
        rs_opts.preverify = opts.preverify;
        rs_opts.verify = opts.verify;
        rs_opts.disable_double_buffering = opts.disable_double_buffering;
        rs_opts.preferred_algos = opts.preferred_algos.clone();

        match progress {
            Some(cb) => {
                let fp = FlashProgress::new(move |e: RsProgressEvent| {
                    let _ = Python::attach(|py| {
                        let event = event_to_py(&e);
                        let _ = cb.call1(py, (event,));
                    });
                });
                rs_opts.progress = fp;
            }
            None => {}
        }

        self.inner.commit(sess, rs_opts).map_err(to_py_err)
    }
}

/// Download a firmware file to the target's flash.
#[pyfunction]
#[pyo3(signature = (session, path, format, options=None, progress=None))]
pub fn download_file(
    session: &mut Session,
    path: &str,
    format: &str,
    options: Option<DownloadOptions>,
    progress: Option<Py<PyAny>>,
) -> PyResult<()> {
    let fmt = probe_rs::flashing::image_format(format).ok_or_else(|| {
        pyo3::exceptions::PyValueError::new_err(format!("unknown image format: {format}"))
    })?;
    let loader = fmt.create_loader(None);
    let opts = options.unwrap_or_default();
    let sess = session
        .inner
        .as_mut()
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Session consumed"))?;

    let mut rs_opts = RsDownloadOptions::default();
    rs_opts.keep_unwritten_bytes = opts.keep_unwritten_bytes;
    rs_opts.dry_run = opts.dry_run;
    rs_opts.do_chip_erase = opts.do_chip_erase;
    rs_opts.skip_erase = opts.skip_erase;
    rs_opts.preverify = opts.preverify;
    rs_opts.verify = opts.verify;
    rs_opts.disable_double_buffering = opts.disable_double_buffering;
    rs_opts.preferred_algos = opts.preferred_algos.clone();

    match progress {
        Some(cb) => {
            let fp = FlashProgress::new(move |e: RsProgressEvent| {
                let _ = Python::attach(|py| {
                    let event = event_to_py(&e);
                    let _ = cb.call1(py, (event,));
                });
            });
            rs_opts.progress = fp;
        }
        None => {}
    }

    probe_rs::flashing::download_file_with_options(sess, path, loader, rs_opts)
        .map_err(to_py_err)
}

/// Build a FlashLoader for a session + image.
#[pyfunction]
#[pyo3(signature = (session, path, format))]
pub fn build_loader(session: &mut Session, path: &str, format: &str) -> PyResult<FlashLoader> {
    let fmt = probe_rs::flashing::image_format(format).ok_or_else(|| {
        pyo3::exceptions::PyValueError::new_err(format!("unknown image format: {format}"))
    })?;
    let loader = fmt.create_loader(None);
    let sess = session
        .inner
        .as_mut()
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Session consumed"))?;
    let rs_loader =
        probe_rs::flashing::build_loader(sess, path, loader, None).map_err(to_py_err)?;
    Ok(FlashLoader { inner: rs_loader })
}
