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
            probe_rs::Error::MemoryNotAligned(_) | probe_rs::Error::InvalidDataLength(_) => {
                "Memory"
            }
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

// ---------------------------------------------------------------------------
// RPC bridge errors

use probe_rs_rpc_client::ClientError;

/// Which side of a client call failed; decides Connection vs ServerGone.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    /// Establishing the transport or the schema handshake.
    Connect,
    /// An established connection failed mid-call.
    Call,
}

/// Failure of the bridge layer (transport, server lifecycle, attach policy),
/// classified for the Python `.kind` attribute. Every variant's message
/// states the one-line fix.
#[derive(Debug)]
pub(crate) enum BridgeError {
    /// The server could not be reached.
    Connection(String),
    /// The server answered but speaks a different RPC schema. The payload is
    /// unused because the fix message is version-independent.
    ServerVersion(#[allow(dead_code)] String),
    /// An established connection was lost (server crash, panic, transport closed).
    ServerGone(String),
    /// The probe is claimed by another session.
    ProbeBusy(String),
    /// Attach failed for a reason other than busy-ness; carries the Python kind.
    Attach(String, &'static str),
    /// A bridge call exceeded its deadline.
    Timeout(String),
    /// The server reported a target-level failure.
    Rpc(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) => write!(
                f,
                "could not connect to the probe-rs server ({msg}); run \
                 pyprobe_rs.open() to start or verify it, or fix the host URL \
                 (ws://host:port, ssh://[user@]host, socket:///path)"
            ),
            Self::ServerVersion(_) => write!(
                f,
                "the probe-rs server uses a different RPC schema than this \
                 client; use the matching probe-rs version for both (point \
                 PYPROBE_RS_BIN at the matching binary or update the server)"
            ),
            Self::ServerGone(msg) => write!(
                f,
                "the connection to the probe-rs server was lost ({msg}); call \
                 pyprobe_rs.open() and attach again — restarting the server \
                 releases its probe"
            ),
            Self::ProbeBusy(msg) => write!(f, "{msg}"),
            Self::Attach(msg, _) => write!(f, "{msg}"),
            Self::Timeout(msg) => write!(
                f,
                "{msg}; pass a larger timeout or check that the target responds"
            ),
            Self::Rpc(msg) => write!(f, "{msg}"),
        }
    }
}

impl BridgeError {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Connection(_) => "Connection",
            Self::ServerVersion(_) => "ServerVersion",
            Self::ServerGone(_) => "ServerGone",
            Self::ProbeBusy(_) => "ProbeBusy",
            Self::Attach(_, kind) => kind,
            Self::Timeout(_) => "Timeout",
            Self::Rpc(_) => "Rpc",
        }
    }

    /// Map a client-crate failure by phase: transport trouble while
    /// connecting is a Connection error, mid-call it means the server is gone.
    pub(crate) fn from_client(e: ClientError, phase: Phase) -> Self {
        use ClientError as E;
        match e {
            E::IncompatibleServer | E::UnknownEndpoint => {
                Self::ServerVersion("schema mismatch".into())
            }
            E::InvalidRemoteHost => Self::Connection(
                "invalid host URL; expected ws://host:port, wss://host:port, \
                 ssh://[user@]host[:port], or socket:///path"
                    .into(),
            ),
            E::SshSpawn(e) => Self::Connection(format!("could not start the ssh tunnel: {e}")),
            E::FileRead(path, e) => Self::Rpc(format!("could not read {}: {e}", path.display())),
            E::Remote(e) => Self::Rpc(e.to_string()),
            E::Transport(t) => {
                let msg = t.to_string();
                match phase {
                    Phase::Connect => Self::Connection(msg),
                    Phase::Call => Self::ServerGone(msg),
                }
            }
            _ => Self::Rpc(e.to_string()),
        }
    }
}

/// Build a ProbeRsError with an explicit kind for bridge-layer failures.
pub(crate) fn to_bridge_err(e: BridgeError) -> PyErr {
    py_err_with_kind(e.to_string(), e.kind())
}

/// Build a ProbeRsError carrying `kind` so scripts can branch without
/// string matching.
pub(crate) fn py_err_with_kind(msg: String, kind: &str) -> PyErr {
    Python::attach(|py| {
        let err = ProbeRsError::new_err(msg);
        let _ = err.value(py).setattr("kind", kind);
        err
    })
}
