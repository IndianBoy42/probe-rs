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

/// The RPC protocol has no fake-probe attach: upstream removed `--dry-run` and
/// the `dry_run` fields of the attach request. Reject the request loudly rather
/// than attaching to a real probe the caller did not intend to touch.
pub fn dry_run_unsupported() -> PyErr {
    py_err_with_kind(
        "dry-run sessions are no longer supported: probe-rs removed the fake-probe \
         attach path from the RPC protocol. Attach a real probe instead; the probe-gated \
         suite lives in pyprobe-rs/tests/test_hil.py"
            .into(),
        "NotImplemented",
    )
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
            // The scan chain reached no core: a probe/protocol problem, not a
            // core-index problem, so the fix is on the probe side.
            probe_rs::Error::NoCoreOnJtagScanChain(_) => "Probe",
            probe_rs::Error::CoreDisabled(_)
            | probe_rs::Error::CoreNotFound(_)
            | probe_rs::Error::GenericCoreError(_)
            | probe_rs::Error::Other(_) => "Core",
            probe_rs::Error::Register(_)
            | probe_rs::Error::RegisterAddressOutOfBounds(_)
            | probe_rs::Error::WriteRegister { .. }
            | probe_rs::Error::ReadRegister { .. } => "Register",
            probe_rs::Error::MemoryNotAligned(_)
            | probe_rs::Error::InvalidDataLength(_)
            | probe_rs::Error::MemoryAccessPortNotFound(_) => "Memory",
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
    /// The server reported a target-level failure. The second field is the
    /// best-effort Python `.kind` classified from the error string (the wire
    /// carries no structured error kind).
    Rpc(String, &'static str),
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
            Self::Rpc(msg, _) => write!(f, "{msg}"),
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
            Self::Rpc(_, kind) => kind,
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
            E::FileRead(path, e) => {
                Self::Rpc(format!("could not read {}: {e}", path.display()), "Rpc")
            }
            E::Remote(e) => {
                let msg = e.to_string();
                let kind = classify_remote(&msg);
                Self::Rpc(msg, kind)
            }
            E::Transport(t) => {
                let msg = t.to_string();
                match phase {
                    Phase::Connect => Self::Connection(msg),
                    Phase::Call => Self::ServerGone(msg),
                }
            }
            _ => Self::Rpc(e.to_string(), "Rpc"),
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

/// Best-effort classification of a server-side probe-rs error string into a
/// Python `.kind`. The wire serializes server errors with `format!("{e:?}")`
/// (anyhow debug: the variant Display plus its `Caused by` chain), so the
/// variant display texts of `probe_rs::Error`, `ArmError`,
/// `DebugProbeError` and `rtt::Error` all reach the client. Matched
/// case-insensitively, most specific first; the result feeds script
/// branching (`e.kind == "Probe"`), never control flow here.
pub(crate) fn classify_remote(msg: &str) -> &'static str {
    let m = msg.to_ascii_lowercase();
    // RTT strings beat the generic probe wrapper they may sit under
    // (`rtt::Error::Probe` wraps a probe error chain).
    if m.contains("control block") || m.contains("rtt") {
        return "Rtt";
    }
    if m.contains("no target responded")
        || m.contains("usb")
        || m.contains("debug probe")
        || m.contains("debug port")
        || m.contains("re-attach")
        || m.contains("probe")
    {
        return "Probe";
    }
    if m.contains("is not enabled") || m.contains("has to be halted") {
        return "Core";
    }
    if m.contains("not aligned") {
        return "Memory";
    }
    if m.contains("timeout") || m.contains("timed out") {
        return "Timeout";
    }
    "Rpc"
}

/// Whether a server-side failure describes a dead probe/target link that a
/// fresh attach can fix: target power cycle (`No target responded`),
/// USB replug, or an upstream error whose remedy literally is a re-attach
/// (`ArmError::ReAttachRequired`). Conservative on purpose — timeouts,
/// halted-state mismatches and target-level errors are NOT recoverable by
/// re-attaching and must surface unchanged.
pub(crate) fn is_fatal_recoverable(kind: &str, msg: &str) -> bool {
    if kind != "Probe" {
        return false;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("no target responded") || m.contains("usb") || m.contains("re-attach")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_remote_matches_upstream_display_texts() {
        // ArmError::NoTargetResponse (target power cycled, SWD link dead).
        assert_eq!(
            classify_remote(
                "No target responded to the debug port. Check that the target \
                 is connected and powered"
            ),
            "Probe"
        );
        // DebugProbeError::Usb.
        assert!(classify_remote("USB Communication Error: No such device").starts_with('P'));
        // ArmError::CoreNotHalted after an unexpected reset started the core.
        assert_eq!(
            classify_remote("The core has to be halted for the operation"),
            "Core"
        );
        // probe_rs::Error::CoreDisabled (debug domain lost).
        assert_eq!(classify_remote("Core 0 is not enabled."), "Core");
        // probe_rs::Error::Timeout.
        assert_eq!(classify_remote("A timeout occurred."), "Timeout");
        // rtt::Error::ControlBlockNotFound.
        assert_eq!(
            classify_remote("RTT control block not found in target memory."),
            "Rtt"
        );
        // Unrecognized text stays generic.
        assert_eq!(classify_remote("something entirely unexpected"), "Rpc");
    }

    #[test]
    fn fatal_recovery_only_fires_for_dead_link_signatures() {
        assert!(is_fatal_recoverable(
            "Probe",
            "No target responded to the debug port"
        ));
        assert!(is_fatal_recoverable("Probe", "USB Communication Error"));
        // Target-level and timeout failures must never trigger a re-attach.
        assert!(!is_fatal_recoverable("Timeout", "A timeout occurred."));
        assert!(!is_fatal_recoverable("Core", "The core has to be halted"));
        assert!(!is_fatal_recoverable(
            "Rtt",
            "RTT control block not found in target memory."
        ));
    }
}
