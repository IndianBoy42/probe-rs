//! Error handling for the pyprobe-rs bridge.

use pyo3::prelude::*;

pyo3::create_exception!(
    pyprobe_rs,
    ProbeRsError,
    pyo3::exceptions::PyException,
    "Error raised for probe-rs operation failures."
);

/// Exception attribute classifying the failure so scripts can branch without
/// string matching (e.g. `e.kind == "Timeout"`). Set on every ProbeRsError
/// instance created by [`to_py_err`].
///
/// Kinds come from the failing probe-rs error type (`Flash`, `Rtt`,
/// `CoreDump`, `DebugProbeError`, ...) and, for the central
/// `probe_rs::Error`, from its variant (`Timeout`, `Probe`, `MissingPermissions`, ...).
pub fn to_py_err<E: std::fmt::Display + 'static>(e: E) -> PyErr {
    let kind = error_kind(&e);
    Python::attach(|py| {
        let err = ProbeRsError::new_err(format!("{e}"));
        let _ = err.value(py).setattr("kind", kind);
        err
    })
}

fn error_kind<E: std::fmt::Display + 'static>(e: &E) -> String {
    let any: &dyn std::any::Any = e;
    if let Some(err) = any.downcast_ref::<probe_rs::Error>() {
        return match err {
            probe_rs::Error::Probe(_) => "Probe",
            probe_rs::Error::Arm(_) => "Arm",
            probe_rs::Error::Riscv(_) => "Riscv",
            probe_rs::Error::Xtensa(_) => "Xtensa",
            probe_rs::Error::ChipNotFound(_) => "ChipNotFound",
            probe_rs::Error::MissingPermissions(_) => "MissingPermissions",
            probe_rs::Error::Timeout => "Timeout",
            probe_rs::Error::NotImplemented(_) => "NotImplemented",
            probe_rs::Error::CoreDisabled(_)
            | probe_rs::Error::CoreNotFound(_)
            | probe_rs::Error::GenericCoreError(_)
            | probe_rs::Error::Other(_) => "Core",
            probe_rs::Error::Register(_)
            | probe_rs::Error::RegisterAddressOutOfBounds(_)
            | probe_rs::Error::WriteRegister { .. }
            | probe_rs::Error::ReadRegister { .. } => "Register",
            probe_rs::Error::MemoryNotAligned(_)
            | probe_rs::Error::InvalidDataLength(_) => "Memory",
            probe_rs::Error::BreakpointOperation(_) => "Breakpoint",
        }
        .into();
    }
    if any.downcast_ref::<probe_rs::rtt::Error>().is_some() {
        return "Rtt".into();
    }
    // Fallback: the concrete error type name (FileDownloadError, FlashError,
    // CoreDumpError, ArmError, DebugProbeError, ...).
    std::any::type_name::<E>()
        .rsplit("::")
        .next()
        .unwrap_or("Error")
        .into()
}
