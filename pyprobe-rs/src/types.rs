//! PyO3 enum proxies mirroring probe-rs / probe-rs-target enums.

use probe_rs_target::{Architecture, CoreType, InstructionSet, Xlen};
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
    // The F extension (single-precision floating point) is separate from the
    // register width and the C extension, so it needs its own variants rather
    // than a hidden bit.
    RV32F = 8,
    RV32FC = 9,
    RV64F = 10,
    RV64FC = 11,
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

/// Wire form of the RTT channel mode, for the attach-time channel config.
pub(crate) fn channel_mode_to_wire(
    mode: ChannelModeProxy,
) -> probe_rs_rpc::rtt_config::ChannelMode {
    use probe_rs_rpc::rtt_config::ChannelMode as W;
    match mode {
        ChannelModeProxy::NoBlockSkip => W::NoBlockSkip,
        ChannelModeProxy::NoBlockTrim => W::NoBlockTrim,
        ChannelModeProxy::BlockIfFull => W::BlockIfFull,
    }
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
        InstructionSet::Riscv { xlen, c, f } => match (xlen, c, f) {
            (Xlen::X32, false, false) => InstructionSetProxy::RV32,
            (Xlen::X32, true, false) => InstructionSetProxy::RV32C,
            (Xlen::X32, false, true) => InstructionSetProxy::RV32F,
            (Xlen::X32, true, true) => InstructionSetProxy::RV32FC,
            (Xlen::X64, false, false) => InstructionSetProxy::RV64,
            (Xlen::X64, true, false) => InstructionSetProxy::RV64C,
            (Xlen::X64, false, true) => InstructionSetProxy::RV64F,
            (Xlen::X64, true, true) => InstructionSetProxy::RV64FC,
        },
        InstructionSet::Xtensa => InstructionSetProxy::Xtensa,
    }
}
