//! CoreHandle: an RPC-backed handle to one core of an attached session.
//!
//! Every method issues one RPC to the managed `probe-rs serve` process with
//! the GIL released, so long-running hardware operations never block the
//! interpreter. The wire carries no register-name table, so name→id
//! resolution is served from the static tables in this file.

use std::sync::{Arc, OnceLock};
use std::time::Duration;
use probe_rs_rpc::core_ops::{
    WireBreakpointCause, WireCoreStatus, WireCoreType, WireHaltReason, WireInstructionSet,
    WireRegisterId, WireRegisterValue, WireSteppingMode, WireVectorCatchCondition,
};
use probe_rs_rpc_client::{ClientError, CoreInterface, SessionInterface};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::error::{py_err_with_kind, to_bridge_err, BridgeError, Phase};
use crate::rpc::{rpc_call, Bridge, Connection, DEFAULT_TIMEOUT};
use crate::types::{
    core_type_to_py, instruction_set_to_py, ArchitectureProxy, CoreStateProxy, CoreStatusProxy,
    CoreTypeProxy, EndianProxy, HaltKindProxy, HaltReasonProxy, InstructionSetProxy,
    VectorCatchConditionProxy,
};

/// Interval between status polls in `wait_for_core_halted`.
const HALT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Headroom added on top of a server-side wait when sizing the bridge
/// deadline (the RPC round-trip itself must not eat into the wait).
const BRIDGE_SLACK: Duration = Duration::from_secs(5);

// ── Register name tables ───────────────────────────────────────────
//
// The RPC wire carries bare `WireRegisterId(u16)` values with no name or
// width metadata, so name resolution happens here. Both tables mirror the
// register files in the probe-rs sources named in each block comment; the
// server maps `WireRegisterId` straight onto `probe_rs::RegisterId`
// (`probe-rs-tools/src/bin/probe-rs/rpc/functions/core_ops.rs`,
// `from_wire_register_id`), so the ids are the probe-rs ids verbatim.

/// (name, id, base width in bits) for every register we expose, lowercase.
type RegEntry = (&'static str, u16, u32);

/// Mirror of `probe-rs/src/architecture/arm/core/registers/cortex_m.rs`:
/// r0..r12 = 0..12, sp = 13, lr = 14, pc = 15, xpsr = 0b1_0000,
/// msp = 0b10001, psp = 0b10010, control (EXTRA) = 0b10100,
/// fpscr = 33, s0..s31 = 64..95. Everything is 32-bit.
const CORTEX_M_REGISTERS: &[RegEntry] = &[
    ("r0", 0, 32),
    ("r1", 1, 32),
    ("r2", 2, 32),
    ("r3", 3, 32),
    ("r4", 4, 32),
    ("r5", 5, 32),
    ("r6", 6, 32),
    ("r7", 7, 32),
    ("r8", 8, 32),
    ("r9", 9, 32),
    ("r10", 10, 32),
    ("r11", 11, 32),
    ("r12", 12, 32),
    ("r13", 13, 32),
    ("r14", 14, 32),
    ("r15", 15, 32),
    ("sp", 13, 32),
    ("lr", 14, 32),
    ("pc", 15, 32),
    ("fp", 7, 32),
    ("xpsr", 0b1_0000, 32),
    ("msp", 0b10001, 32),
    ("psp", 0b10010, 32),
    ("control", 0b10100, 32),
    ("extra", 0b10100, 32),
    ("fpscr", 33, 32),
    ("s0", 64, 32),
    ("s1", 65, 32),
    ("s2", 66, 32),
    ("s3", 67, 32),
    ("s4", 68, 32),
    ("s5", 69, 32),
    ("s6", 70, 32),
    ("s7", 71, 32),
    ("s8", 72, 32),
    ("s9", 73, 32),
    ("s10", 74, 32),
    ("s11", 75, 32),
    ("s12", 76, 32),
    ("s13", 77, 32),
    ("s14", 78, 32),
    ("s15", 79, 32),
    ("s16", 80, 32),
    ("s17", 81, 32),
    ("s18", 82, 32),
    ("s19", 83, 32),
    ("s20", 84, 32),
    ("s21", 85, 32),
    ("s22", 86, 32),
    ("s23", 87, 32),
    ("s24", 88, 32),
    ("s25", 89, 32),
    ("s26", 90, 32),
    ("s27", 91, 32),
    ("s28", 92, 32),
    ("s29", 93, 32),
    ("s30", 94, 32),
    ("s31", 95, 32),
];

/// Mirror of `probe-rs/src/architecture/riscv/registers.rs`: pc = 0x7b1,
/// x0..x31 = 0x1000..0x101f, f0..f31 = 0x1020..0x103f, and the FP CSRs
/// fflags/frm/fcsr = 0x001/0x002/0x003. `registers64.rs` keeps every id and
/// role identical and only widens the data types, so one table serves RV32
/// and RV64 and the width is adjusted at lookup time. The second half of the
/// table lists the ABI aliases from the file's `RegisterRole::Other` names.
const RISCV_REGISTERS: &[RegEntry] = &[
    ("pc", 0x7b1, 32),
    ("x0", 0x1000, 32),
    ("x1", 0x1001, 32),
    ("x2", 0x1002, 32),
    ("x3", 0x1003, 32),
    ("x4", 0x1004, 32),
    ("x5", 0x1005, 32),
    ("x6", 0x1006, 32),
    ("x7", 0x1007, 32),
    ("x8", 0x1008, 32),
    ("x9", 0x1009, 32),
    ("x10", 0x100A, 32),
    ("x11", 0x100B, 32),
    ("x12", 0x100C, 32),
    ("x13", 0x100D, 32),
    ("x14", 0x100E, 32),
    ("x15", 0x100F, 32),
    ("x16", 0x1010, 32),
    ("x17", 0x1011, 32),
    ("x18", 0x1012, 32),
    ("x19", 0x1013, 32),
    ("x20", 0x1014, 32),
    ("x21", 0x1015, 32),
    ("x22", 0x1016, 32),
    ("x23", 0x1017, 32),
    ("x24", 0x1018, 32),
    ("x25", 0x1019, 32),
    ("x26", 0x101A, 32),
    ("x27", 0x101B, 32),
    ("x28", 0x101C, 32),
    ("x29", 0x101D, 32),
    ("x30", 0x101E, 32),
    ("x31", 0x101F, 32),
    ("zero", 0x1000, 32),
    ("ra", 0x1001, 32),
    ("sp", 0x1002, 32),
    ("gp", 0x1003, 32),
    ("tp", 0x1004, 32),
    ("t0", 0x1005, 32),
    ("t1", 0x1006, 32),
    ("t2", 0x1007, 32),
    ("fp", 0x1008, 32),
    ("s0", 0x1008, 32),
    ("s1", 0x1009, 32),
    ("a0", 0x100A, 32),
    ("a1", 0x100B, 32),
    ("a2", 0x100C, 32),
    ("a3", 0x100D, 32),
    ("a4", 0x100E, 32),
    ("a5", 0x100F, 32),
    ("a6", 0x1010, 32),
    ("a7", 0x1011, 32),
    ("s2", 0x1012, 32),
    ("s3", 0x1013, 32),
    ("s4", 0x1014, 32),
    ("s5", 0x1015, 32),
    ("s6", 0x1016, 32),
    ("s7", 0x1017, 32),
    ("s8", 0x1018, 32),
    ("s9", 0x1019, 32),
    ("s10", 0x101A, 32),
    ("s11", 0x101B, 32),
    ("t3", 0x101C, 32),
    ("t4", 0x101D, 32),
    ("t5", 0x101E, 32),
    ("t6", 0x101F, 32),
    ("fflags", 0x001, 32),
    ("frm", 0x002, 32),
    ("fcsr", 0x003, 32),
    ("f0", 0x1020, 32),
    ("f1", 0x1021, 32),
    ("f2", 0x1022, 32),
    ("f3", 0x1023, 32),
    ("f4", 0x1024, 32),
    ("f5", 0x1025, 32),
    ("f6", 0x1026, 32),
    ("f7", 0x1027, 32),
    ("f8", 0x1028, 32),
    ("f9", 0x1029, 32),
    ("f10", 0x102A, 32),
    ("f11", 0x102B, 32),
    ("f12", 0x102C, 32),
    ("f13", 0x102D, 32),
    ("f14", 0x102E, 32),
    ("f15", 0x102F, 32),
    ("f16", 0x1030, 32),
    ("f17", 0x1031, 32),
    ("f18", 0x1032, 32),
    ("f19", 0x1033, 32),
    ("f20", 0x1034, 32),
    ("f21", 0x1035, 32),
    ("f22", 0x1036, 32),
    ("f23", 0x1037, 32),
    ("f24", 0x1038, 32),
    ("f25", 0x1039, 32),
    ("f26", 0x103A, 32),
    ("f27", 0x103B, 32),
    ("f28", 0x103C, 32),
    ("f29", 0x103D, 32),
    ("f30", 0x103E, 32),
    ("f31", 0x103F, 32),
    ("ft0", 0x1020, 32),
    ("ft1", 0x1021, 32),
    ("ft2", 0x1022, 32),
    ("ft3", 0x1023, 32),
    ("ft4", 0x1024, 32),
    ("ft5", 0x1025, 32),
    ("ft6", 0x1026, 32),
    ("ft7", 0x1027, 32),
    ("fs0", 0x1028, 32),
    ("fs1", 0x1029, 32),
    ("fa0", 0x102A, 32),
    ("fa1", 0x102B, 32),
    ("fa2", 0x102C, 32),
    ("fa3", 0x102D, 32),
    ("fa4", 0x102E, 32),
    ("fa5", 0x102F, 32),
    ("fa6", 0x1030, 32),
    ("fa7", 0x1031, 32),
    ("fs2", 0x1032, 32),
    ("fs3", 0x1033, 32),
    ("fs4", 0x1034, 32),
    ("fs5", 0x1035, 32),
    ("fs6", 0x1036, 32),
    ("fs7", 0x1037, 32),
    ("fs8", 0x1038, 32),
    ("fs9", 0x1039, 32),
    ("fs10", 0x103A, 32),
    ("fs11", 0x103B, 32),
    ("ft8", 0x103C, 32),
    ("ft9", 0x103D, 32),
    ("ft10", 0x103E, 32),
    ("ft11", 0x103F, 32),
];

/// The register catalog for `instruction_set`, or `None` for architectures
/// whose register file we do not mirror (AArch64, Xtensa).
fn table_for(instruction_set: WireInstructionSet) -> Option<&'static [RegEntry]> {
    match instruction_set {
        WireInstructionSet::RV32
        | WireInstructionSet::RV32C
        | WireInstructionSet::RV64
        | WireInstructionSet::RV64C => Some(RISCV_REGISTERS),
        WireInstructionSet::Thumb2 | WireInstructionSet::A32 => Some(CORTEX_M_REGISTERS),
        _ => None,
    }
}

/// Width of `name` under `instruction_set`, applying the RV64 widening from
/// `registers64.rs`: data registers widen to 64-bit, the FP CSRs stay 32-bit.
fn reg_bits(instruction_set: WireInstructionSet, name: &str, base: u32) -> u32 {
    match instruction_set {
        WireInstructionSet::RV64 | WireInstructionSet::RV64C => {
            if matches!(name, "fflags" | "frm" | "fcsr") {
                32
            } else {
                64
            }
        }
        _ => base,
    }
}

fn is_riscv(instruction_set: WireInstructionSet) -> bool {
    matches!(
        instruction_set,
        WireInstructionSet::RV32
            | WireInstructionSet::RV32C
            | WireInstructionSet::RV64
            | WireInstructionSet::RV64C
    )
}

fn instruction_set_is_64_bit(instruction_set: WireInstructionSet) -> bool {
    matches!(
        instruction_set,
        WireInstructionSet::A64 | WireInstructionSet::RV64 | WireInstructionSet::RV64C
    )
}

/// Which table defines a lowercase register name.
enum NameHit {
    /// Defined only in the Cortex-M table; always 32-bit.
    CortexM(u16),
    /// Defined only in the RISC-V table; the width follows RV64 widening.
    Riscv(u16),
    /// Defined in both tables with different ids (`pc`, `sp`, `fp`,
    /// `s0`..`s11`); the core's instruction set decides.
    Ambiguous,
    /// Unknown name.
    None,
}

/// Look `name` up across both static tables. Most names live in exactly one
/// table, so this usually avoids the metadata roundtrip.
fn lookup_name(name: &str) -> NameHit {
    let cm = CORTEX_M_REGISTERS.iter().find(|(n, _, _)| *n == name);
    let rv = RISCV_REGISTERS.iter().find(|(n, _, _)| *n == name);
    match (cm, rv) {
        (Some((_, id, _)), None) => NameHit::CortexM(*id),
        (None, Some((_, id, _))) => NameHit::Riscv(*id),
        (Some(_), Some(_)) => NameHit::Ambiguous,
        (None, None) => NameHit::None,
    }
}

fn unknown_register(name: &str) -> PyErr {
    let mut known: Vec<&str> = CORTEX_M_REGISTERS
        .iter()
        .chain(RISCV_REGISTERS.iter())
        .map(|(n, _, _)| *n)
        .collect();
    known.sort_unstable();
    known.dedup();
    py_err_with_kind(format!("unknown register: {name} (known: {})", known.join(", ")), "Register")
}

// ── Wire → local enum helpers ──────────────────────────────────────

pub(crate) fn wire_instruction_set_to_local(w: WireInstructionSet) -> probe_rs_target::InstructionSet {
    use probe_rs_target::InstructionSet as I;
    match w {
        WireInstructionSet::Thumb2 => I::Thumb2,
        WireInstructionSet::A32 => I::A32,
        WireInstructionSet::A64 => I::A64,
        WireInstructionSet::RV32 => I::RV32,
        WireInstructionSet::RV32C => I::RV32C,
        WireInstructionSet::RV64 => I::RV64,
        WireInstructionSet::RV64C => I::RV64C,
        WireInstructionSet::Xtensa => I::Xtensa,
    }
}

pub(crate) fn wire_core_type_to_local(w: WireCoreType) -> probe_rs::CoreType {
    use probe_rs::CoreType as C;
    match w {
        WireCoreType::Armv6m => C::Armv6m,
        WireCoreType::Armv7a => C::Armv7a,
        WireCoreType::Armv7r => C::Armv7r,
        WireCoreType::Armv7m => C::Armv7m,
        WireCoreType::Armv7em => C::Armv7em,
        WireCoreType::Armv8a => C::Armv8a,
        WireCoreType::Armv8m => C::Armv8m,
        WireCoreType::Riscv => C::Riscv,
        WireCoreType::Riscv64 => C::Riscv64,
        WireCoreType::Xtensa => C::Xtensa,
    }
}

/// Map a wire register value onto the local enum the coredump
/// reconstruction needs.
pub(crate) fn wire_register_value_to_local(v: WireRegisterValue) -> probe_rs::RegisterValue {
    match v {
        WireRegisterValue::U32(x) => probe_rs::RegisterValue::U32(x),
        WireRegisterValue::U64(x) => probe_rs::RegisterValue::U64(x),
        WireRegisterValue::U128(x) => probe_rs::RegisterValue::U128(x),
    }
}

fn wire_status_to_py(s: WireCoreStatus) -> CoreStatusProxy {
    let (state, is_halted, is_running, halt_reason) = match s {
        WireCoreStatus::Running => (CoreStateProxy::Running, false, true, None),
        WireCoreStatus::Halted(reason) => (
            CoreStateProxy::Halted,
            true,
            false,
            Some(wire_halt_reason_to_py(reason)),
        ),
        WireCoreStatus::LockedUp => (CoreStateProxy::LockedUp, false, false, None),
        WireCoreStatus::Sleeping => (CoreStateProxy::Sleeping, false, false, None),
        WireCoreStatus::Unknown => (CoreStateProxy::Unknown, false, false, None),
    };
    CoreStatusProxy {
        state,
        is_halted,
        is_running,
        halt_reason,
    }
}

/// Wire halt reasons mirror `probe_rs::HaltReason` except that the
/// semihosting breakpoint cause carries no command payload, so the detail
/// string is built directly instead of round-tripping through
/// `BreakpointCause::Semihosting` (whose `SemihostingCommand` cannot be
/// reconstructed from the wire).
fn wire_halt_reason_to_py(r: WireHaltReason) -> HaltReasonProxy {
    let (kind, detail) = match r {
        WireHaltReason::Multiple => (HaltKindProxy::Multiple, "multiple reasons".into()),
        WireHaltReason::Breakpoint(cause) => (
            HaltKindProxy::Breakpoint,
            match cause {
                WireBreakpointCause::Hardware => "hardware".into(),
                WireBreakpointCause::Software => "software".into(),
                WireBreakpointCause::Unknown => "unknown".into(),
                WireBreakpointCause::Semihosting(_) => "semihosting".into(),
            },
        ),
        WireHaltReason::Exception => (HaltKindProxy::Exception, "exception".into()),
        WireHaltReason::Watchpoint => (HaltKindProxy::Watchpoint, "watchpoint".into()),
        WireHaltReason::Step => (HaltKindProxy::Step, "step".into()),
        WireHaltReason::Request => (HaltKindProxy::Request, "request".into()),
        WireHaltReason::External => (HaltKindProxy::External, "external".into()),
        WireHaltReason::Unknown => (HaltKindProxy::Unknown, "unknown".into()),
    };
    HaltReasonProxy { kind, detail }
}

fn wire_value_to_u128(v: WireRegisterValue) -> u128 {
    match v {
        WireRegisterValue::U32(x) => x as u128,
        WireRegisterValue::U64(x) => x as u128,
        WireRegisterValue::U128(x) => x,
    }
}

/// Narrowest wire encoding that fits `value`.
fn u128_to_wire_value(value: u128) -> WireRegisterValue {
    if value <= u32::MAX as u128 {
        WireRegisterValue::U32(value as u32)
    } else if value <= u64::MAX as u128 {
        WireRegisterValue::U64(value as u64)
    } else {
        WireRegisterValue::U128(value)
    }
}

fn py_to_wire_vector_catch(v: VectorCatchConditionProxy) -> WireVectorCatchCondition {
    match v {
        VectorCatchConditionProxy::HardFault => WireVectorCatchCondition::HardFault,
        VectorCatchConditionProxy::CoreReset => WireVectorCatchCondition::CoreReset,
        VectorCatchConditionProxy::SecureFault => WireVectorCatchCondition::SecureFault,
        VectorCatchConditionProxy::All => WireVectorCatchCondition::All,
        VectorCatchConditionProxy::Svc => WireVectorCatchCondition::Svc,
        VectorCatchConditionProxy::Hlt => WireVectorCatchCondition::Hlt,
    }
}

/// Info about a single register.
#[pyclass(from_py_object)]
#[derive(Clone)]
pub struct CoreRegisterInfo {
    #[pyo3(get)]
    pub name: String,
    #[pyo3(get)]
    pub id: u16,
    #[pyo3(get)]
    pub size_in_bits: u32,
}

/// Handle to a core within an attached session. Every call is an RPC to the
/// managed server; the handle is a plain (connection, core index) pair with
/// no hardware state of its own, so it can be held and shared freely.
#[pyclass]
pub struct CoreHandle {
    conn: Arc<Connection>,
    core: u32,
    /// Instruction set of this core, fetched once on first need. Decides
    /// between the static register tables and the RV64 width adjustment.
    /// `OnceLock` because the value only exists after a runtime RPC.
    instruction_set: OnceLock<WireInstructionSet>,
}

impl CoreHandle {
    pub(crate) fn new(conn: Arc<Connection>, core: u32) -> Self {
        Self {
            conn,
            core,
            instruction_set: OnceLock::new(),
        }
    }

    /// CoreInterface for this handle's core.
    fn core_if(&self) -> CoreInterface {
        self.conn.session().core(self.core as usize)
    }

    /// Run `f` over this core's `CoreInterface` on the bridge runtime with
    /// the GIL released. The returned future owns the interface, so the
    /// borrow ends inside `py.detach`.
    fn rpc<T, F>(
        &self,
        py: Python<'_>,
        op: &str,
        f: impl FnOnce(CoreInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        self.rpc_with_deadline(py, DEFAULT_TIMEOUT, op, f)
    }

    /// [`Self::rpc`] with an explicit bridge deadline; used when the
    /// server-side wait can exceed the default 30 s.
    fn rpc_with_deadline<T, F>(
        &self,
        py: Python<'_>,
        deadline: Duration,
        op: &str,
        f: impl FnOnce(CoreInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        let core_if = self.core_if();
        py.detach(|| rpc_call(deadline, op, f(core_if))).map_err(to_bridge_err)
    }

    /// Same as [`Self::rpc`] for calls that address the session rather than
    /// a single core (target metadata, stepping, disassembly).
    fn rpc_session<T, F>(
        &self,
        py: Python<'_>,
        op: &str,
        f: impl FnOnce(SessionInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        let session = self.conn.session().clone();
        py.detach(|| rpc_call(DEFAULT_TIMEOUT, op, f(session))).map_err(to_bridge_err)
    }

    /// Core metadata (instruction set, FPU) for this core.
    fn core_metadata(&self, py: Python<'_>) -> PyResult<probe_rs_rpc::core_ops::WireCoreMetadata> {
        self.rpc(py, "core metadata", |c| async move { c.metadata().await })
    }

    /// Fetch and cache this core's instruction set (one metadata RPC).
    fn ensure_instruction_set(&self, py: Python<'_>) -> PyResult<WireInstructionSet> {
        if let Some(is) = self.instruction_set.get() {
            return Ok(*is);
        }
        let meta = self.core_metadata(py)?;
        let _ = self.instruction_set.set(meta.instruction_set);
        Ok(meta.instruction_set)
    }

    /// Resolve a register name or raw id to a wire register id. Names are
    /// matched case-insensitively across both static tables; names defined
    /// in both (`pc`, `sp`, `fp`, `s0`..`s11`) are disambiguated through the
    /// core's instruction set. Raw ids pass through untouched.
    fn resolve_register(
        &self,
        py: Python<'_>,
        name_or_id: &Bound<'_, PyAny>,
    ) -> PyResult<(WireRegisterId, Option<u32>)> {
        if let Ok(id) = name_or_id.extract::<u16>() {
            return Ok((WireRegisterId(id), None));
        }
        if let Ok(id) = name_or_id.extract::<i64>() {
            return Ok((WireRegisterId(id as u16), None));
        }
        let name = name_or_id.extract::<String>()?.to_lowercase();
        match lookup_name(&name) {
            NameHit::CortexM(id) => Ok((WireRegisterId(id), Some(32))),
            NameHit::Riscv(id) => {
                // Only the RISC-V width depends on the instruction set
                // (RV64 widens the data registers); the id itself does not.
                let is = self.ensure_instruction_set(py)?;
                Ok((WireRegisterId(id), Some(reg_bits(is, &name, 32))))
            }
            NameHit::Ambiguous => {
                let is = self.ensure_instruction_set(py)?;
                match table_for(is) {
                    Some(table) => match table.iter().find(|(n, _, _)| *n == name) {
                        Some((n, id, base)) => {
                            Ok((WireRegisterId(*id), Some(reg_bits(is, n, *base))))
                        }
                        None => Err(unknown_register(&name)),
                    },
                    None => self.resolve_role_name(is, &name),
                }
            }
            NameHit::None => Err(unknown_register(&name)),
        }
    }

    /// Role-name fallback for architectures without a static table
    /// (AArch64 ids from `aarch64.rs`, Xtensa ids from
    /// `architecture/xtensa/registers.rs`).
    fn resolve_role_name(
        &self,
        is: WireInstructionSet,
        name: &str,
    ) -> PyResult<(WireRegisterId, Option<u32>)> {
        match (is, name) {
            // Xtensa: pc is a special register, sp = a1, return address = a0.
            (WireInstructionSet::Xtensa, "pc") => Ok((WireRegisterId(0xFF00), Some(32))),
            (WireInstructionSet::Xtensa, "sp") => Ok((WireRegisterId(0x0001), Some(32))),
            (WireInstructionSet::Xtensa, "lr") | (WireInstructionSet::Xtensa, "ra") => {
                Ok((WireRegisterId(0x0000), Some(32)))
            }
            // AArch64: pc = 32, sp = 31, fp = x29, lr = x30; all 64-bit.
            (WireInstructionSet::A64, "pc") => Ok((WireRegisterId(32), Some(64))),
            (WireInstructionSet::A64, "sp") => Ok((WireRegisterId(31), Some(64))),
            (WireInstructionSet::A64, "fp") => Ok((WireRegisterId(29), Some(64))),
            (WireInstructionSet::A64, "lr") => Ok((WireRegisterId(30), Some(64))),
            _ => Err(unknown_register(name)),
        }
    }

    /// Read one register as u128.
    fn read_reg_u128(&self, py: Python<'_>, id: WireRegisterId) -> PyResult<u128> {
        let value = self.rpc(py, "read core register", |c| async move {
            c.read_core_reg(id).await
        })?;
        Ok(wire_value_to_u128(value))
    }

    /// Read one register as u64 (addresses and the shortcut getters).
    fn read_reg_u64(&self, py: Python<'_>, id: WireRegisterId) -> PyResult<u64> {
        let value = self.read_reg_u128(py, id)?;
        u64::try_from(value).map_err(|_| {
            pyo3::exceptions::PyOverflowError::new_err(format!(
                "register value {value:#x} does not fit in 64 bits"
            ))
        })
    }
}

/// Re-apply the hardware breakpoints tracked for `core` on the server.
///
/// The flash path calls this after boot: a reset can clear the debug unit
/// while the session (and our tracked set) stays alive. GIL-free; safe to
/// call from inside a `py.detach` block.
pub(crate) fn reapply_breakpoints(conn: &Arc<Connection>, core: u32) -> Result<(), BridgeError> {
    let addresses = conn.breakpoints.lock().get(&core).cloned().unwrap_or_default();
    if addresses.is_empty() {
        return Ok(());
    }
    let session = conn.session().clone();
    let results = Bridge::global()?.wait(
        DEFAULT_TIMEOUT,
        "reapply breakpoints",
        async move { session.core(core as usize).set_hw_breakpoints(addresses).await },
    )?;
    for result in results.map_err(|e| BridgeError::from_client(e, Phase::Call))? {
        result.map_err(|e| {
            BridgeError::Rpc(format!(
                "failed to re-apply a hardware breakpoint on core {core}: {e}"
            ))
        })?;
    }
    Ok(())
}

#[pymethods]
impl CoreHandle {
    // ── Control ─────────────────────────────────────────────────────

    fn halt(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<u64> {
        // The bridge deadline must cover the server-side halt wait.
        let deadline = Duration::from_millis(timeout_ms) + BRIDGE_SLACK;
        let info = self.rpc_with_deadline(py, deadline, "halt core", |c| async move {
            c.halt(Duration::from_millis(timeout_ms)).await
        })?;
        Ok(info.pc)
    }

    fn run(&self, py: Python<'_>) -> PyResult<()> {
        self.rpc(py, "run core", |c| async move { c.run().await })
    }

    fn reset(&self, py: Python<'_>) -> PyResult<()> {
        self.rpc(py, "reset core", |c| async move { c.reset().await })
    }
    fn reset_and_halt(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<u64> {
        let deadline = Duration::from_millis(timeout_ms) + BRIDGE_SLACK;
        let info = self.rpc_with_deadline(py, deadline, "reset and halt core", |c| async move {
            c.reset_and_halt(Duration::from_millis(timeout_ms)).await
        })?;
        Ok(info.pc)
    }

    /// Single-instruction step; returns the new program counter.
    fn step(&self, py: Python<'_>) -> PyResult<u64> {
        self.step_by(py, WireSteppingMode::StepInstruction)
    }

    /// Statement-level step over calls (server-side; needs debug info
    /// loaded via `Session.load_debug_info`); returns the new PC.
    fn step_over(&self, py: Python<'_>) -> PyResult<u64> {
        self.step_by(py, WireSteppingMode::OverStatement)
    }

    /// Statement-level step into calls (server-side; needs debug info
    /// loaded via `Session.load_debug_info`); returns the new PC.
    fn step_into(&self, py: Python<'_>) -> PyResult<u64> {
        self.step_by(py, WireSteppingMode::IntoStatement)
    }

    /// Step out of the current frame (server-side; needs debug info loaded
    /// via `Session.load_debug_info`); returns the new PC.
    fn step_out(&self, py: Python<'_>) -> PyResult<u64> {
        self.step_by(py, WireSteppingMode::OutOfStatement)
    }

    /// Block until the core reports Halted, polling every 50 ms. The whole
    /// loop runs as one future on the bridge runtime, with `timeout_ms` as
    /// the overall deadline.
    fn wait_for_core_halted(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<()> {
        let deadline = Duration::from_millis(timeout_ms);
        let conn = self.conn.clone();
        let core = self.core;
        py.detach(move || {
            let bridge = Bridge::global()?;
            bridge
                .wait(
                    deadline,
                    "wait for core halted",
                    async move {
                        let core_if = conn.session().core(core as usize);
                        loop {
                            match core_if.status().await {
                                Ok(WireCoreStatus::Halted(_)) => return Ok(()),
                                Ok(_) => tokio::time::sleep(HALT_POLL_INTERVAL).await,
                                Err(e) => return Err(e),
                            }
                        }
                    },
                )?
                .map_err(|e| BridgeError::from_client(e, Phase::Call))
        })
        .map_err(to_bridge_err)
    }

    fn core_halted(&self, py: Python<'_>) -> PyResult<bool> {
        Ok(matches!(self.raw_status(py)?, WireCoreStatus::Halted(_)))
    }

    fn status(&self, py: Python<'_>) -> PyResult<CoreStatusProxy> {
        Ok(wire_status_to_py(self.raw_status(py)?))
    }

    // ── Single-word memory ──────────────────────────────────────────

    fn read_word_8(&self, py: Python<'_>, addr: u64) -> PyResult<u8> {
        let mut buf = self.rpc(py, "read memory 8", |c| async move {
            c.read_memory_8(addr, 1).await
        })?;
        buf.pop().ok_or_else(|| empty_read("read_word_8"))
    }

    fn read_word_16(&self, py: Python<'_>, addr: u64) -> PyResult<u16> {
        let mut buf = self.rpc(py, "read memory 16", |c| async move {
            c.read_memory_16(addr, 1).await
        })?;
        buf.pop().ok_or_else(|| empty_read("read_word_16"))
    }

    fn read_word_32(&self, py: Python<'_>, addr: u64) -> PyResult<u32> {
        let mut buf = self.rpc(py, "read memory 32", |c| async move {
            c.read_memory_32(addr, 1).await
        })?;
        buf.pop().ok_or_else(|| empty_read("read_word_32"))
    }

    fn read_word_64(&self, py: Python<'_>, addr: u64) -> PyResult<u64> {
        let mut buf = self.rpc(py, "read memory 64", |c| async move {
            c.read_memory_64(addr, 1).await
        })?;
        buf.pop().ok_or_else(|| empty_read("read_word_64"))
    }

    fn write_word_8(&self, py: Python<'_>, addr: u64, val: u8) -> PyResult<()> {
        self.rpc(py, "write memory 8", |c| async move {
            c.write_memory_8(addr, vec![val]).await
        })
    }

    fn write_word_16(&self, py: Python<'_>, addr: u64, val: u16) -> PyResult<()> {
        self.rpc(py, "write memory 16", |c| async move {
            c.write_memory_16(addr, vec![val]).await
        })
    }

    fn write_word_32(&self, py: Python<'_>, addr: u64, val: u32) -> PyResult<()> {
        self.rpc(py, "write memory 32", |c| async move {
            c.write_memory_32(addr, vec![val]).await
        })
    }

    fn write_word_64(&self, py: Python<'_>, addr: u64, val: u64) -> PyResult<()> {
        self.rpc(py, "write memory 64", |c| async move {
            c.write_memory_64(addr, vec![val]).await
        })
    }

    // ── Block memory ────────────────────────────────────────────────

    fn read_8(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u8>> {
        self.rpc(py, "read memory 8", |c| async move {
            c.read_memory_8(addr, count).await
        })
    }

    fn read_16(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u16>> {
        self.rpc(py, "read memory 16", |c| async move {
            c.read_memory_16(addr, count).await
        })
    }

    fn read_32(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u32>> {
        self.rpc(py, "read memory 32", |c| async move {
            c.read_memory_32(addr, count).await
        })
    }

    fn read_64(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u64>> {
        self.rpc(py, "read memory 64", |c| async move {
            c.read_memory_64(addr, count).await
        })
    }

    fn write_8(&self, py: Python<'_>, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.rpc(py, "write memory 8", |c| async move {
            c.write_memory_8(addr, data).await
        })
    }

    fn write_16(&self, py: Python<'_>, addr: u64, data: Vec<u16>) -> PyResult<()> {
        self.rpc(py, "write memory 16", |c| async move {
            c.write_memory_16(addr, data).await
        })
    }

    fn write_32(&self, py: Python<'_>, addr: u64, data: Vec<u32>) -> PyResult<()> {
        self.rpc(py, "write memory 32", |c| async move {
            c.write_memory_32(addr, data).await
        })
    }

    fn write_64(&self, py: Python<'_>, addr: u64, data: Vec<u64>) -> PyResult<()> {
        self.rpc(py, "write memory 64", |c| async move {
            c.write_memory_64(addr, data).await
        })
    }

    // ── Generic byte memory ────────────────────────────────────────

    /// Bulk byte read; returns as many bytes as are readable from `addr`,
    /// stopping at the first unreadable region (may be fewer than `count`).
    fn read(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u8>> {
        self.rpc(py, "read memory", |c| async move {
            c.read_bytes(addr, count).await
        })
    }

    fn write(&self, py: Python<'_>, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.rpc(py, "write memory", |c| async move {
            c.write_memory_8(addr, data).await
        })
    }

    // ── Register access ────────────────────────────────────────────

    /// Read a core register by name or numeric ID.
    ///
    /// Names are case-insensitive and cover both the Cortex-M and the
    /// RISC-V register files with their ABI aliases (`pc`, `sp`, `lr`/`ra`,
    /// `fp`, `x1`, `s0`...); integers are raw register IDs.
    fn read_core_reg(&self, py: Python<'_>, name_or_id: &Bound<'_, PyAny>) -> PyResult<u128> {
        let (id, _) = self.resolve_register(py, name_or_id)?;
        self.read_reg_u128(py, id)
    }

    /// Write a core register by name or numeric ID.
    ///
    /// The value is validated against the register's width; `OverflowError`
    /// is raised when it does not fit.
    fn write_core_reg(
        &self,
        py: Python<'_>,
        name_or_id: &Bound<'_, PyAny>,
        value: u128,
    ) -> PyResult<()> {
        let (id, known_bits) = self.resolve_register(py, name_or_id)?;
        if let Some(bits) = known_bits {
            let max: u128 = if bits >= 128 {
                u128::MAX
            } else {
                (1u128 << bits) - 1
            };
            if value > max {
                return Err(pyo3::exceptions::PyOverflowError::new_err(format!(
                    "register value {value:#x} does not fit in a {bits}-bit register"
                )));
            }
        }
        let wire_value = u128_to_wire_value(value);
        self.rpc(py, "write core register", |c| async move {
            c.write_core_reg(id, wire_value).await
        })
    }

    // ── Named register shortcuts ──────────────────────────────────

    fn read_pc(&self, py: Python<'_>) -> PyResult<u64> {
        let id = self.program_counter_id(py)?;
        self.read_reg_u64(py, id)
    }

    fn read_sp(&self, py: Python<'_>) -> PyResult<u64> {
        let id = self.role_register_id(py, "sp")?;
        self.read_reg_u64(py, id)
    }

    fn read_fp(&self, py: Python<'_>) -> PyResult<u64> {
        let id = self.role_register_id(py, "fp")?;
        self.read_reg_u64(py, id)
    }

    fn read_return_address(&self, py: Python<'_>) -> PyResult<u64> {
        let id = self.return_address_id(py)?;
        self.read_reg_u64(py, id)
    }

    // ── Breakpoints ────────────────────────────────────────────────

    /// Set one hardware breakpoint; the facade tracks it so it survives a
    /// reflash (the flash path re-applies the tracked set after boot).
    fn set_hw_breakpoint(&self, py: Python<'_>, addr: u64) -> PyResult<()> {
        let results = self.set_hw_breakpoints(py, vec![addr])?;
        match results.into_iter().next() {
            Some((_, None)) => Ok(()),
            Some((_, Some(msg))) => Err(py_err_with_kind(msg, "Rpc")),
            None => Ok(()),
        }
    }

    /// Set several hardware breakpoints at once. Returns one
    /// `(address, error)` pair per address, with `error` set to the
    /// Rpc-classified failure message; failed addresses are not tracked.
    fn set_hw_breakpoints(
        &self,
        py: Python<'_>,
        addresses: Vec<u64>,
    ) -> PyResult<Vec<(u64, Option<String>)>> {
        let results = self.rpc(py, "set hw breakpoints", |c| {
            let addresses = addresses.clone();
            async move { c.set_hw_breakpoints(addresses).await }
        })?;
        let mut tracked = self.conn.breakpoints.lock();
        let entry = tracked.entry(self.core).or_default();
        let mut out = Vec::with_capacity(addresses.len());
        for (addr, result) in addresses.into_iter().zip(results) {
            match result {
                Ok(()) => {
                    if !entry.contains(&addr) {
                        entry.push(addr);
                    }
                    out.push((addr, None));
                }
                Err(e) => out.push((
                    addr,
                    Some(format!(
                        "failed to set hardware breakpoint at {addr:#x}: {e}"
                    )),
                )),
            }
        }
        Ok(out)
    }

    /// Clear one hardware breakpoint.
    fn clear_hw_breakpoint(&self, py: Python<'_>, addr: u64) -> PyResult<()> {
        self.conn
            .breakpoints
            .lock()
            .entry(self.core)
            .and_modify(|entry| entry.retain(|a| *a != addr));
        self.rpc(py, "clear hw breakpoints", |c| async move {
            c.clear_hw_breakpoints(vec![addr]).await
        })
    }

    /// Clear several hardware breakpoints at once.
    fn clear_hw_breakpoints(&self, py: Python<'_>, addresses: Vec<u64>) -> PyResult<()> {
        {
            let mut tracked = self.conn.breakpoints.lock();
            let entry = tracked.entry(self.core).or_default();
            entry.retain(|a| !addresses.contains(a));
        }
        self.rpc(py, "clear hw breakpoints", |c| async move {
            c.clear_hw_breakpoints(addresses).await
        })
    }

    /// Clear every hardware breakpoint this facade set on the core.
    fn clear_all_hw_breakpoints(&self, py: Python<'_>) -> PyResult<()> {
        let addresses = self
            .conn
            .breakpoints
            .lock()
            .remove(&self.core)
            .unwrap_or_default();
        if addresses.is_empty() {
            return Ok(());
        }
        self.rpc(py, "clear hw breakpoints", |c| async move {
            c.clear_hw_breakpoints(addresses).await
        })
    }

    fn enable_vector_catch(
        &self,
        py: Python<'_>,
        condition: VectorCatchConditionProxy,
    ) -> PyResult<()> {
        let condition = py_to_wire_vector_catch(condition);
        self.rpc(py, "enable vector catch", |c| async move {
            c.enable_vector_catch(condition).await
        })
    }

    fn disable_vector_catch(
        &self,
        _py: Python<'_>,
        _condition: VectorCatchConditionProxy,
    ) -> PyResult<()> {
        // The RPC surface only carries `core/enable_vector_catch`; there is
        // no disable endpoint, and the facade refuses to fake one.
        Err(py_err_with_kind(
            "the probe-rs RPC server has no vector-catch disable endpoint; \
             the catch clears on core reset".into(),
            "Rpc",
        ))
    }

    // ── Disassembly (RPC-only surface) ─────────────────────────────

    /// Disassemble `count` instructions starting at `address`. Returns a
    /// list of dicts with `address, instruction, instruction_bytes, line,
    /// column, location`; the DWARF-derived fields need
    /// `Session.load_debug_info` first.
    fn disassemble(&self, py: Python<'_>, address: u64, count: i64) -> PyResult<Vec<Py<PyAny>>> {
        let instructions = self.rpc_session(py, "disassemble", |s| async move {
            s.disassemble(self.core, address, 0, 0, count).await
        })?;
        let mut out = Vec::with_capacity(instructions.len());
        for i in instructions {
            let row = PyDict::new(py);
            row.set_item("address", i.address)?;
            row.set_item("instruction", i.instruction)?;
            row.set_item("instruction_bytes", i.instruction_bytes)?;
            row.set_item("line", i.line)?;
            row.set_item("column", i.column)?;
            row.set_item(
                "location",
                match i.location {
                    Some(loc) => {
                        let d = PyDict::new(py);
                        d.set_item("name", loc.name)?;
                        d.set_item("path", loc.path)?;
                        d.into_any().unbind()
                    }
                    None => py.None(),
                },
            )?;
            out.push(row.into_any().unbind());
        }
        Ok(out)
    }

    // ── Meta ────────────────────────────────────────────────────────

    fn id(&self) -> usize {
        self.core as usize
    }

    fn architecture(&self, py: Python<'_>) -> PyResult<ArchitectureProxy> {
        Ok(self.core_type_local(py)?.architecture()).map(|a| match a {
            probe_rs_target::Architecture::Arm => ArchitectureProxy::Arm,
            probe_rs_target::Architecture::Riscv => ArchitectureProxy::Riscv,
            probe_rs_target::Architecture::Xtensa => ArchitectureProxy::Xtensa,
        })
    }

    fn core_type(&self, py: Python<'_>) -> PyResult<CoreTypeProxy> {
        Ok(core_type_to_py(self.core_type_local(py)?))
    }

    fn instruction_set(&self, py: Python<'_>) -> PyResult<InstructionSetProxy> {
        let is = self.ensure_instruction_set(py)?;
        Ok(instruction_set_to_py(wire_instruction_set_to_local(is)))
    }

    fn fpu_support(&self, py: Python<'_>) -> PyResult<bool> {
        Ok(self.core_metadata(py)?.fpu_support)
    }

    fn floating_point_register_count(&self, py: Python<'_>) -> PyResult<usize> {
        self.core_metadata(py)?
            .floating_point_register_count
            .map(|c| c as usize)
            .ok_or_else(|| {
                py_err_with_kind("this core has no floating-point unit".into(), "Rpc")
            })
    }

    fn is_64_bit(&self, py: Python<'_>) -> PyResult<bool> {
        Ok(instruction_set_is_64_bit(self.ensure_instruction_set(py)?))
    }

    /// Byte order is not transported over the RPC surface; the facade
    /// refuses to guess on your behalf.
    fn endianness(&self, _py: Python<'_>) -> PyResult<EndianProxy> {
        Err(py_err_with_kind(
            "endianness is not exposed by the probe-rs RPC server".into(),
            "Rpc",
        ))
    }

    /// Snapshot of the registers this facade knows for the core's
    /// architecture (the wire carries no name table; Cortex-M and RISC-V
    /// files are built in). Floating-point registers are listed only when
    /// the core has an FPU.
    fn registers(&self, py: Python<'_>) -> PyResult<Vec<CoreRegisterInfo>> {
        let meta = self.core_metadata(py)?;
        let is = meta.instruction_set;
        let _ = self.instruction_set.set(is);
        let Some(table) = table_for(is) else {
            return Err(py_err_with_kind(
                format!(
                    "register names for {is:?} cores are not built into pyprobe-rs; \
                     address registers by numeric ID instead"
                ),
                "Register",
            ));
        };
        Ok(table
            .iter()
            .filter(|(name, _, _)| {
                // Keep FP registers only for cores with an FPU: on RISC-V
                // every FP register name starts with 'f' (but 'fp' is the
                // integer frame pointer, x8); on Cortex-M it is fpscr plus
                // s0..s31 ('sp' is the only other s-name).
                let fp = if is_riscv(is) {
                    name.starts_with('f') && *name != "fp"
                } else {
                    *name == "fpscr" || (name.starts_with('s') && *name != "sp")
                };
                !fp || meta.fpu_support
            })
            .map(|(name, id, base)| CoreRegisterInfo {
                name: name.to_string(),
                id: *id,
                size_in_bits: reg_bits(is, name, *base),
            })
            .collect())
    }

    /// Memory regions of the attached target as (kind, start, end) tuples,
    /// served from the session's target metadata.
    fn memory_regions(&self, py: Python<'_>) -> PyResult<Vec<(String, u64, u64)>> {
        let meta = self.rpc_session(py, "target metadata", |s| async move {
            s.target_metadata().await
        })?;
        Ok(meta
            .memory_map
            .iter()
            .map(|region| {
                // The wire re-declares the target memory map with tuple
                // ranges (probe_rs_rpc::chip::MemoryRegion).
                let (kind, start, end) = match region {
                    probe_rs_rpc::chip::MemoryRegion::Nvm(r) => ("nvm", r.range.0, r.range.1),
                    probe_rs_rpc::chip::MemoryRegion::Ram(r) => ("ram", r.range.0, r.range.1),
                    probe_rs_rpc::chip::MemoryRegion::Generic(r) => {
                        ("generic", r.range.0, r.range.1)
                    }
                };
                (kind.to_string(), start, end)
            })
            .collect())
    }

    fn __repr__(&self) -> String {
        format!("CoreHandle(core={})", self.core)
    }
}

/// Role-id helpers that need the core's architecture.
impl CoreHandle {

    /// Status as the raw wire value.
    fn raw_status(&self, py: Python<'_>) -> PyResult<WireCoreStatus> {
        self.rpc(py, "core status", |c| async move { c.status().await })
    }

    /// Run one `debug_step` and return the new PC; server warnings (e.g.
    /// statement stepping without debug info) go to stderr.
    fn step_by(&self, py: Python<'_>, mode: WireSteppingMode) -> PyResult<u64> {
        let core = self.core;
        let response =
            self.rpc_session(py, "step core", |s| async move { s.debug_step(core, mode).await })?;
        if let Some(warning) = response.warning {
            eprintln!("pyprobe-rs: step warning: {warning}");
        }
        Ok(response.program_counter)
    }

    /// Core type via the session target metadata.
    fn core_type_local(&self, py: Python<'_>) -> PyResult<probe_rs::CoreType> {
        let meta = self.rpc_session(py, "target metadata", |s| async move {
            s.target_metadata().await
        })?;
        let wire_type = meta
            .cores
            .iter()
            .find(|c| c.index == self.core)
            .map(|c| c.core_type)
            .ok_or_else(|| {
                py_err_with_kind(
                    format!("core {} is not part of the attached session", self.core),
                    "Rpc",
                )
            })?;
        Ok(wire_core_type_to_local(wire_type))
    }

    /// Register id of the program counter for this core's architecture.
    fn program_counter_id(&self, py: Python<'_>) -> PyResult<WireRegisterId> {
        let is = self.ensure_instruction_set(py)?;
        let id = match is {
            // cortex_m.rs: PC = 15.
            WireInstructionSet::Thumb2 | WireInstructionSet::A32 => 15,
            // aarch64.rs: PC has its own id slot on AArch64.
            WireInstructionSet::A64 => 32,
            // riscv/registers.rs: the DPC/pc debug register.
            WireInstructionSet::RV32
            | WireInstructionSet::RV32C
            | WireInstructionSet::RV64
            | WireInstructionSet::RV64C => 0x7b1,
            // xtensa/registers.rs: pc is a special register.
            WireInstructionSet::Xtensa => 0xFF00,
        };
        Ok(WireRegisterId(id))
    }

    /// Register id of the return-address register for this architecture.
    fn return_address_id(&self, py: Python<'_>) -> PyResult<WireRegisterId> {
        let is = self.ensure_instruction_set(py)?;
        let id = match is {
            WireInstructionSet::Thumb2 | WireInstructionSet::A32 => 14,
            WireInstructionSet::A64 => 30,
            WireInstructionSet::RV32
            | WireInstructionSet::RV32C
            | WireInstructionSet::RV64
            | WireInstructionSet::RV64C => 0x1001,
            WireInstructionSet::Xtensa => 0x0000,
        };
        Ok(WireRegisterId(id))
    }

    /// Register id of a role name (`sp`, `fp`) for this architecture.
    fn role_register_id(&self, py: Python<'_>, role: &str) -> PyResult<WireRegisterId> {
        let is = self.ensure_instruction_set(py)?;
        let (sp, fp): (u16, Option<u16>) = match is {
            WireInstructionSet::Thumb2 | WireInstructionSet::A32 => (13, Some(7)),
            WireInstructionSet::A64 => (31, Some(29)),
            WireInstructionSet::RV32
            | WireInstructionSet::RV32C
            | WireInstructionSet::RV64
            | WireInstructionSet::RV64C => (0x1002, Some(0x1008)),
            // Xtensa has no frame-pointer role; sp is a1.
            WireInstructionSet::Xtensa => (0x0001, None),
        };
        match (role, fp) {
            ("sp", _) => Ok(WireRegisterId(sp)),
            ("fp", Some(fp)) => Ok(WireRegisterId(fp)),
            _ => Err(py_err_with_kind(
                format!("register {role} does not exist on this core"),
                "Register",
            )),
        }
    }
}

/// Error for a word read the server answered with no elements.
fn empty_read(op: &str) -> PyErr {
    py_err_with_kind(format!("server returned no data for {op}"), "Rpc")
}
