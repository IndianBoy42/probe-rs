//! RTT (Real-Time Transfer) channel access.

use crate::error::to_py_err;
use crate::session::Session;
use crate::types::ChannelModeProxy;
use pyo3::prelude::*;
use probe_rs::rtt::Rtt as RsRtt;
use std::cell::RefCell;

/// Attach to RTT on a session's core, optionally at a specific pointer.
pub fn attach(
    slf: &Bound<'_, Session>,
    core_index: usize,
    ptr: Option<u64>,
) -> PyResult<Rtt> {
    let _py = slf.py();
    let session_ref: Py<Session> = slf.clone().unbind();
    let mut sess = slf.borrow_mut();
    let sess_ref = sess
        .inner
        .as_mut()
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Session consumed"))?;
    let mut core = sess_ref.core(core_index).map_err(to_py_err)?;
    let rtt = match ptr {
        Some(addr) => RsRtt::attach_at(&mut core, addr),
        None => RsRtt::attach(&mut core),
    };
    drop(core);
    drop(sess);
    let rtt = rtt.map_err(to_py_err)?;

    let up_channels: Vec<ChannelInfo> = rtt
        .up_channels
        .iter()
        .enumerate()
        .map(|(i, c)| ChannelInfo {
            number: i,
            name: c.name().map(|s| s.to_string()),
            buffer_size: c.buffer_size(),
        })
        .collect();
    let down_channels: Vec<ChannelInfo> = rtt
        .down_channels
        .iter()
        .enumerate()
        .map(|(i, c)| ChannelInfo {
            number: i,
            name: c.name().map(|s| s.to_string()),
            buffer_size: c.buffer_size(),
        })
        .collect();

    Ok(Rtt {
        session: session_ref,
        core_index,
        rtt: RefCell::new(rtt),
        up_channels,
        down_channels,
    })
}

#[pyclass]
#[derive(Clone)]
pub struct ChannelInfo {
    #[pyo3(get)]
    pub number: usize,
    #[pyo3(get)]
    pub name: Option<String>,
    #[pyo3(get)]
    pub buffer_size: usize,
}

#[pyclass(unsendable)]
pub struct Rtt {
    session: Py<Session>,
    core_index: usize,
    rtt: RefCell<RsRtt>,
    up_channels: Vec<ChannelInfo>,
    down_channels: Vec<ChannelInfo>,
}

#[pymethods]
impl Rtt {
    fn up_channels(&self) -> Vec<ChannelInfo> {
        self.up_channels.clone()
    }

    fn down_channels(&self) -> Vec<ChannelInfo> {
        self.down_channels.clone()
    }

    fn up_channel(slf: &Bound<'_, Self>, n: usize) -> PyResult<UpChannel> {
        let rtt_ref = slf.borrow();
        if n >= rtt_ref.up_channels.len() {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "up channel {n} out of range (have {})",
                rtt_ref.up_channels.len()
            )));
        }
        drop(rtt_ref);
        Ok(UpChannel {
            rtt: slf.clone().unbind(),
            channel_number: n,
        })
    }

    fn down_channel(slf: &Bound<'_, Self>, n: usize) -> PyResult<DownChannel> {
        let rtt_ref = slf.borrow();
        if n >= rtt_ref.down_channels.len() {
            return Err(pyo3::exceptions::PyIndexError::new_err(format!(
                "down channel {n} out of range (have {})",
                rtt_ref.down_channels.len()
            )));
        }
        drop(rtt_ref);
        Ok(DownChannel {
            rtt: slf.clone().unbind(),
            channel_number: n,
        })
    }

    fn ptr(&self) -> u64 {
        self.rtt.borrow().ptr()
    }
}

fn with_core_and_rtt<R>(
    rtt_handle: &Py<Rtt>,
    py: Python<'_>,
    f: impl FnOnce(&mut probe_rs::rtt::Rtt, &mut probe_rs::Core<'_>) -> PyResult<R>,
) -> PyResult<R> {
    let rtt_bound = rtt_handle.bind(py);
    let rtt_guard = rtt_bound.borrow_mut();
    let session_clone = rtt_guard.session.clone_ref(py);
    let core_index = rtt_guard.core_index;

    let sess_bound = session_clone.bind(py);
    let mut sess = sess_bound.borrow_mut();
    let sess_ref = sess
        .inner
        .as_mut()
        .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Session consumed"))?;
    let mut core = sess_ref.core(core_index).map_err(to_py_err)?;
    let mut rtt_ref = rtt_guard.rtt.borrow_mut();
    f(&mut rtt_ref, &mut core)
}

#[pyclass(unsendable)]
pub struct UpChannel {
    rtt: Py<Rtt>,
    channel_number: usize,
}

#[pymethods]
impl UpChannel {
    #[pyo3(signature = (count=4096))]
    fn read(&self, py: Python<'_>, count: usize) -> PyResult<Vec<u8>> {
        with_core_and_rtt(&self.rtt, py, |rtt, core| {
            let chan = rtt
                .up_channel(self.channel_number)
                .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("channel not found"))?;
            let mut buf = vec![0u8; count];
            let n = chan.read(core, &mut buf).map_err(to_py_err)?;
            buf.truncate(n);
            Ok(buf)
        })
    }

    #[pyo3(signature = (count=4096))]
    fn read_str(&self, py: Python<'_>, count: usize) -> PyResult<String> {
        let data = self.read(py, count)?;
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    #[pyo3(signature = (count=4096))]
    fn poll(&self, py: Python<'_>, count: usize) -> PyResult<Vec<u8>> {
        self.read(py, count)
    }

    fn number(&self) -> usize {
        self.channel_number
    }

    fn name(&self, py: Python<'_>) -> Option<String> {
        self.rtt
            .bind(py)
            .borrow()
            .up_channels
            .get(self.channel_number)
            .and_then(|c| c.name.clone())
    }

    fn buffer_size(&self, py: Python<'_>) -> usize {
        self.rtt
            .bind(py)
            .borrow()
            .up_channels
            .get(self.channel_number)
            .map(|c| c.buffer_size)
            .unwrap_or(0)
    }

    fn mode(&self, py: Python<'_>) -> PyResult<ChannelModeProxy> {
        with_core_and_rtt(&self.rtt, py, |rtt, core| {
            let chan = rtt
                .up_channel(self.channel_number)
                .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("channel not found"))?;
            let mode = chan.mode(core).map_err(to_py_err)?;
            Ok(crate::types::channel_mode_to_py(mode))
        })
    }
}

#[pyclass(unsendable)]
pub struct DownChannel {
    rtt: Py<Rtt>,
    channel_number: usize,
}

#[pymethods]
impl DownChannel {
    fn write(&self, py: Python<'_>, data: &[u8]) -> PyResult<usize> {
        with_core_and_rtt(&self.rtt, py, |rtt, core| {
            let chan = rtt
                .down_channel(self.channel_number)
                .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("channel not found"))?;
            chan.write(core, data).map_err(to_py_err)
        })
    }

    fn write_str(&self, py: Python<'_>, s: &str) -> PyResult<()> {
        self.write(py, s.as_bytes())?;
        Ok(())
    }

    fn number(&self) -> usize {
        self.channel_number
    }

    fn name(&self, py: Python<'_>) -> Option<String> {
        self.rtt
            .bind(py)
            .borrow()
            .down_channels
            .get(self.channel_number)
            .and_then(|c| c.name.clone())
    }

    fn buffer_size(&self, py: Python<'_>) -> usize {
        self.rtt
            .bind(py)
            .borrow()
            .down_channels
            .get(self.channel_number)
            .map(|c| c.buffer_size)
            .unwrap_or(0)
    }
}
