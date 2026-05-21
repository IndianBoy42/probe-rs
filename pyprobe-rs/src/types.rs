//! PyO3 enum proxies mirroring probe-rs / probe-rs-target enums.

use pyo3::prelude::*;
use probe_rs::{
    BreakpointCause, CoreStatus, HaltReason, VectorCatchCondition,
};
use probe_rs_target::{Architecture, CoreType, Endian, InstructionSet};

/// Wire protocol for debug probe communication.
#[pyclass(name = "WireProtocol", eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WireProtocolProxy {
    Swd = 0,
    Jtag = 1,
}

/// How to attach to a target.
#[pyclass(name = "AttachMethod", eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AttachMethodProxy {
    Normal = 0,
    UnderReset = 1,
}

#[pyclass(name = "Architecture", eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureProxy {
    Arm = 0,
    Riscv = 1,
    Xtensa = 2,
}

#[pyclass(name = "CoreType", eq, eq_int)]
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

#[pyclass(name = "InstructionSet", eq, eq_int)]
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

#[pyclass(name = "Endian", eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EndianProxy {
    Little = 0,
    Big = 1,
}

/// RTT channel buffering mode.
#[pyclass(name = "ChannelMode", eq, eq_int)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChannelModeProxy {
    NoBlockSkip = 0,
    NoBlockTrim = 1,
    BlockIfFull = 2,
}

#[pyclass(name = "VectorCatchCondition", eq, eq_int)]
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

#[pyclass(name = "HaltReason")]
#[derive(Clone)]
pub struct HaltReasonProxy {
    #[pyo3(get)]
    pub reason: String,
    #[pyo3(get)]
    pub kind: String,
}

#[pyclass(name = "BreakpointCause")]
#[derive(Clone)]
pub struct BreakpointCauseProxy {
    #[pyo3(get)]
    pub cause: String,
}

#[pyclass(name = "CoreStatus")]
#[derive(Clone)]
pub struct CoreStatusProxy {
    #[pyo3(get)]
    pub status: String,
    #[pyo3(get)]
    pub is_halted: bool,
    #[pyo3(get)]
    pub is_running: bool,
    #[pyo3(get)]
    pub halt_reason: Option<HaltReasonProxy>,
}

#[pyclass(name = "ProgressEvent")]
#[derive(Clone)]
pub struct ProgressEventPy {
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

// ── Conversion helpers ─────────────────────────────────────────────

pub fn core_status_to_py(s: CoreStatus) -> CoreStatusProxy {
    match s {
        CoreStatus::Running => CoreStatusProxy {
            status: "Running".into(),
            is_halted: false,
            is_running: true,
            halt_reason: None,
        },
        CoreStatus::Halted(reason) => CoreStatusProxy {
            status: "Halted".into(),
            is_halted: true,
            is_running: false,
            halt_reason: Some(halt_reason_to_py(reason)),
        },
        CoreStatus::LockedUp => CoreStatusProxy {
            status: "LockedUp".into(),
            is_halted: false,
            is_running: false,
            halt_reason: None,
        },
        CoreStatus::Sleeping => CoreStatusProxy {
            status: "Sleeping".into(),
            is_halted: false,
            is_running: false,
            halt_reason: None,
        },
        CoreStatus::Unknown => CoreStatusProxy {
            status: "Unknown".into(),
            is_halted: false,
            is_running: false,
            halt_reason: None,
        },
    }
}

pub fn halt_reason_to_py(r: HaltReason) -> HaltReasonProxy {
    let (kind, reason) = match r {
        HaltReason::Multiple => ("Multiple".into(), "multiple reasons".into()),
        HaltReason::Breakpoint(cause) => (
            "Breakpoint".into(),
            match cause {
                BreakpointCause::Hardware => "hardware".into(),
                BreakpointCause::Software => "software".into(),
                BreakpointCause::Unknown => "unknown".into(),
                BreakpointCause::Semihosting(cmd) => format!("semihosting: {:?}", cmd),
            },
        ),
        HaltReason::Exception => ("Exception".into(), "exception".into()),
        HaltReason::Watchpoint => ("Watchpoint".into(), "watchpoint".into()),
        HaltReason::Step => ("Step".into(), "step".into()),
        HaltReason::Request => ("Request".into(), "request".into()),
        HaltReason::External => ("External".into(), "external".into()),
        HaltReason::Unknown => ("Unknown".into(), "unknown".into()),
    };
    HaltReasonProxy { reason, kind }
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
