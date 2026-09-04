//! PyO3 enum proxies mirroring probe-rs / probe-rs-target enums.

use probe_rs::{BreakpointCause, CoreStatus, HaltReason, VectorCatchCondition};
use probe_rs_target::{Architecture, CoreType, Endian, InstructionSet};
use pyo3::prelude::*;

/// Wire protocol for debug probe communication.
#[pyclass(name = "WireProtocol", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WireProtocolProxy {
    Swd = 0,
    Jtag = 1,
}

#[pyclass(name = "Architecture", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureProxy {
    Arm = 0,
    Riscv = 1,
    Xtensa = 2,
}

#[pyclass(name = "CoreType", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CoreTypeProxy {
    Armv6m = 0,
    Armv7a = 1,
    Armv7r = 2,
    Armv7m = 3,
    Armv7em = 4,
    Armv8a = 5,
    Armv8m = 6,
    Riscv = 7,
    Riscv64 = 8,
    Xtensa = 9,
}

#[pyclass(name = "InstructionSet", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InstructionSetProxy {
    Thumb2 = 0,
    A32 = 1,
    A64 = 2,
    RV32 = 3,
    RV32C = 4,
    RV64 = 5,
    RV64C = 6,
    Xtensa = 7,
}

#[pyclass(name = "Endian", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EndianProxy {
    Little = 0,
    Big = 1,
}

/// RTT channel buffering mode.
#[pyclass(name = "ChannelMode", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelModeProxy {
    NoBlockSkip = 0,
    NoBlockTrim = 1,
    BlockIfFull = 2,
}

#[pyclass(name = "VectorCatchCondition", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VectorCatchConditionProxy {
    HardFault = 0,
    CoreReset = 1,
    SecureFault = 2,
    All = 3,
    Svc = 4,
    Hlt = 5,
}

// ── Payload-carrying status proxies ────────────────────────────────

/// Core run state, from `CoreStatus.state`.
#[pyclass(name = "CoreState", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CoreStateProxy {
    Running = 0,
    Halted = 1,
    LockedUp = 2,
    Sleeping = 3,
    Unknown = 4,
}

/// Category of the most recent halt, from `CoreStatus.halt_reason.kind`.
#[pyclass(name = "HaltKind", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HaltKindProxy {
    Breakpoint = 0,
    Watchpoint = 1,
    Step = 2,
    Request = 3,
    External = 4,
    Exception = 5,
    Multiple = 6,
    Unknown = 7,
}

/// Why the core last halted.
#[pyclass(name = "HaltReason", from_py_object)]
#[derive(Clone)]
pub struct HaltReasonProxy {
    #[pyo3(get)]
    pub kind: HaltKindProxy,
    /// Human-readable detail, e.g. "hardware", "semihosting: ...".
    #[pyo3(get)]
    pub detail: String,
}

/// Snapshot of a core's run state.
#[pyclass(name = "CoreStatus", from_py_object)]
#[derive(Clone)]
pub struct CoreStatusProxy {
    #[pyo3(get)]
    pub state: CoreStateProxy,
    #[pyo3(get)]
    pub is_halted: bool,
    #[pyo3(get)]
    pub is_running: bool,
    #[pyo3(get)]
    pub halt_reason: Option<HaltReasonProxy>,
}

/// Where trace data is collected from, for `Session.setup_tracing`.
#[pyclass(name = "TraceSink", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TraceSinkProxy {
    Swo = 0,
    Tpiu = 1,
    TraceMemory = 2,
}

// ── Conversion helpers ─────────────────────────────────────────────

pub fn core_status_to_py(s: CoreStatus) -> CoreStatusProxy {
    let (state, is_halted, is_running, halt_reason) = match s {
        CoreStatus::Running => (CoreStateProxy::Running, false, true, None),
        CoreStatus::Halted(reason) => (
            CoreStateProxy::Halted,
            true,
            false,
            Some(halt_reason_to_py(reason)),
        ),
        CoreStatus::LockedUp => (CoreStateProxy::LockedUp, false, false, None),
        CoreStatus::Sleeping => (CoreStateProxy::Sleeping, false, false, None),
        CoreStatus::Unknown => (CoreStateProxy::Unknown, false, false, None),
    };
    CoreStatusProxy {
        state,
        is_halted,
        is_running,
        halt_reason,
    }
}

pub fn halt_reason_to_py(r: HaltReason) -> HaltReasonProxy {
    let (kind, detail) = match r {
        HaltReason::Multiple => (HaltKindProxy::Multiple, "multiple reasons".into()),
        HaltReason::Breakpoint(cause) => (
            HaltKindProxy::Breakpoint,
            match cause {
                BreakpointCause::Hardware => "hardware".into(),
                BreakpointCause::Software => "software".into(),
                BreakpointCause::Unknown => "unknown".into(),
                BreakpointCause::Semihosting(cmd) => format!("semihosting: {:?}", cmd),
            },
        ),
        HaltReason::Exception => (HaltKindProxy::Exception, "exception".into()),
        HaltReason::Watchpoint => (HaltKindProxy::Watchpoint, "watchpoint".into()),
        HaltReason::Step => (HaltKindProxy::Step, "step".into()),
        HaltReason::Request => (HaltKindProxy::Request, "request".into()),
        HaltReason::External => (HaltKindProxy::External, "external".into()),
        HaltReason::Unknown => (HaltKindProxy::Unknown, "unknown".into()),
    };
    HaltReasonProxy { kind, detail }
}

pub fn architecture_to_py(a: Architecture) -> ArchitectureProxy {
    match a {
        Architecture::Arm => ArchitectureProxy::Arm,
        Architecture::Riscv => ArchitectureProxy::Riscv,
        Architecture::Xtensa => ArchitectureProxy::Xtensa,
    }
}

pub fn core_type_to_py(c: CoreType) -> CoreTypeProxy {
    match c {
        CoreType::Armv6m => CoreTypeProxy::Armv6m,
        CoreType::Armv7a => CoreTypeProxy::Armv7a,
        CoreType::Armv7r => CoreTypeProxy::Armv7r,
        CoreType::Armv7m => CoreTypeProxy::Armv7m,
        CoreType::Armv7em => CoreTypeProxy::Armv7em,
        CoreType::Armv8a => CoreTypeProxy::Armv8a,
        CoreType::Armv8m => CoreTypeProxy::Armv8m,
        CoreType::Riscv => CoreTypeProxy::Riscv,
        CoreType::Riscv64 => CoreTypeProxy::Riscv64,
        CoreType::Xtensa => CoreTypeProxy::Xtensa,
    }
}

pub fn instruction_set_to_py(i: InstructionSet) -> InstructionSetProxy {
    match i {
        InstructionSet::Thumb2 => InstructionSetProxy::Thumb2,
        InstructionSet::A32 => InstructionSetProxy::A32,
        InstructionSet::A64 => InstructionSetProxy::A64,
        InstructionSet::RV32 => InstructionSetProxy::RV32,
        InstructionSet::RV32C => InstructionSetProxy::RV32C,
        InstructionSet::RV64 => InstructionSetProxy::RV64,
        InstructionSet::RV64C => InstructionSetProxy::RV64C,
        InstructionSet::Xtensa => InstructionSetProxy::Xtensa,
    }
}

pub fn endian_to_py(e: Endian) -> EndianProxy {
    match e {
        Endian::Little => EndianProxy::Little,
        Endian::Big => EndianProxy::Big,
    }
}

pub fn py_to_vector_catch(v: VectorCatchConditionProxy) -> VectorCatchCondition {
    match v {
        VectorCatchConditionProxy::HardFault => VectorCatchCondition::HardFault,
        VectorCatchConditionProxy::CoreReset => VectorCatchCondition::CoreReset,
        VectorCatchConditionProxy::SecureFault => VectorCatchCondition::SecureFault,
        VectorCatchConditionProxy::All => VectorCatchCondition::All,
        VectorCatchConditionProxy::Svc => VectorCatchCondition::Svc,
        VectorCatchConditionProxy::Hlt => VectorCatchCondition::Hlt,
    }
}

pub fn channel_mode_to_py(m: probe_rs::rtt::ChannelMode) -> ChannelModeProxy {
    match m {
        probe_rs::rtt::ChannelMode::NoBlockSkip => ChannelModeProxy::NoBlockSkip,
        probe_rs::rtt::ChannelMode::NoBlockTrim => ChannelModeProxy::NoBlockTrim,
        probe_rs::rtt::ChannelMode::BlockIfFull => ChannelModeProxy::BlockIfFull,
    }
}

pub fn py_to_channel_mode(m: ChannelModeProxy) -> probe_rs::rtt::ChannelMode {
    match m {
        ChannelModeProxy::NoBlockSkip => probe_rs::rtt::ChannelMode::NoBlockSkip,
        ChannelModeProxy::NoBlockTrim => probe_rs::rtt::ChannelMode::NoBlockTrim,
        ChannelModeProxy::BlockIfFull => probe_rs::rtt::ChannelMode::BlockIfFull,
    }
}
