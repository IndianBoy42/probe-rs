//! CoreHandle: an RPC-backed handle to one core of an attached session.
//!
//! Every method issues one RPC to the managed `probe-rs serve` process with
//! the GIL released, so long-running hardware operations never block the
//! interpreter. The wire carries no register-name table, so name→id
//! resolution is served from the static tables in this file.

use probe_rs_rpc::core_ops::{
    WireBreakpointCause, WireCoreStatus, WireCoreType, WireHaltReason, WireInstructionSet,
    WireRegisterId, WireRegisterValue, WireSteppingMode, WireVectorCatchCondition, WireXlen,
};
use probe_rs_rpc_client::{ClientError, CoreInterface, SessionInterface};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use probe_rs::{CoreRegister, CoreRegisters, RegisterRole};

use crate::error::{BridgeError, Phase, py_err_with_kind, to_bridge_err};
use crate::rpc::{Bridge, Connection, DEFAULT_TIMEOUT, call_with_recovery, rpc_call};
use crate::types::{
    ArchitectureProxy, CoreStateProxy, CoreStatusProxy, CoreTypeProxy, EndianProxy, HaltKindProxy,
    HaltReasonProxy, InstructionSetProxy, VectorCatchConditionProxy, core_type_to_py,
    instruction_set_to_py,
};

/// Interval between status polls in `wait_for_core_halted`.
const HALT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Headroom added on top of a server-side wait when sizing the bridge
/// deadline (the RPC round-trip itself must not eat into the wait).
const BRIDGE_SLACK: Duration = Duration::from_secs(5);

// ── Register catalog ───────────────────────────────────────────────
//
// The RPC wire carries bare `WireRegisterId(u16)` values with no name or
// width metadata, so name resolution happens here. Rather than mirror each
// architecture's register file, we reuse the probe-rs files the server maps
// ids from (`CoreRegisters::for_core_type`): names, ids and widths stay in
// lockstep with the server, and every architecture probe-rs knows is
// covered rather than only the ones this facade once hard-coded.

/// Every name a register answers to: the architecture-defined primary name
/// plus role aliases (`Other`, `Argument`, `Return`).
fn register_names(r: &CoreRegister) -> impl Iterator<Item = &'static str> {
    r.roles.iter().filter_map(|role| match role {
        RegisterRole::Core(n)
        | RegisterRole::Argument(n)
        | RegisterRole::Return(n)
        | RegisterRole::Other(n) => Some(*n),
        _ => None,
    })
}

/// The register file matching a wire instruction set. `for_core_type` picks
/// the FPU and 64-bit variants from the live metadata, so the returned file
/// is the one the server reads from.
fn registers_for(
    instruction_set: WireInstructionSet,
    fpu_support: bool,
    floating_point_register_count: Option<u64>,
) -> &'static CoreRegisters {
    let core_type = match instruction_set {
        // The Cortex-M families share one file; the FPU flag selects the
        // variant. A32 uses the AArch32 file on both v7-A/R and 32-bit v8-A.
        WireInstructionSet::Thumb2 => probe_rs::CoreType::Armv7m,
        WireInstructionSet::A32 => probe_rs::CoreType::Armv7a,
        WireInstructionSet::A64 => probe_rs::CoreType::Armv8a,
        WireInstructionSet::Riscv {
            xlen: WireXlen::X32,
            ..
        } => probe_rs::CoreType::Riscv,
        WireInstructionSet::Riscv {
            xlen: WireXlen::X64,
            ..
        } => probe_rs::CoreType::Riscv64,
        WireInstructionSet::Xtensa => probe_rs::CoreType::Xtensa,
    };
    CoreRegisters::for_core_type(
        core_type,
        fpu_support,
        floating_point_register_count.map(|c| c as usize),
    )
}

/// True for instruction sets whose registers are 64 bits wide.
fn instruction_set_is_64_bit(instruction_set: WireInstructionSet) -> bool {
    matches!(
        instruction_set,
        WireInstructionSet::A64
            | WireInstructionSet::Riscv {
                xlen: WireXlen::X64,
                ..
            }
    )
}

/// Arch-neutral aliases the facade accepts beside a register's own names,
/// matching the old in-process binding (`pc`/`sp`/`lr`/`fp`).
const ROLE_ALIASES: &[(&str, RegisterRole)] = &[
    ("pc", RegisterRole::ProgramCounter),
    ("sp", RegisterRole::StackPointer),
    ("lr", RegisterRole::ReturnAddress),
    ("fp", RegisterRole::FramePointer),
];

/// The register playing `role`, if the file defines one.
fn role_register(
    regs: &'static CoreRegisters,
    role: RegisterRole,
) -> Option<&'static CoreRegister> {
    regs.all_registers().find(|r| r.register_has_role(role))
}

fn role_register_missing(role: &str) -> PyErr {
    py_err_with_kind(format!("this core has no {role} register"), "Register")
}

fn unknown_register(name: &str, regs: &CoreRegisters) -> PyErr {
    let mut known: Vec<String> = regs
        .all_registers()
        .flat_map(register_names)
        .map(|n| n.to_lowercase())
        .collect();
    known.sort_unstable();
    known.dedup();
    py_err_with_kind(
        format!("unknown register: {name} (known: {})", known.join(", ")),
        "Register",
    )
}

// ── Wire → local enum helpers ──────────────────────────────────────

pub(crate) fn wire_instruction_set_to_local(
    w: WireInstructionSet,
) -> probe_rs_target::InstructionSet {
    use probe_rs_target::InstructionSet as I;
    match w {
        WireInstructionSet::Thumb2 => I::Thumb2,
        WireInstructionSet::A32 => I::A32,
        WireInstructionSet::A64 => I::A64,
        WireInstructionSet::Riscv { xlen, c, f } => I::Riscv {
            xlen: match xlen {
                WireXlen::X32 => probe_rs_target::Xlen::X32,
                WireXlen::X64 => probe_rs_target::Xlen::X64,
            },
            c,
            f,
        },
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
    /// Instruction set of this core, fetched once on first need. Selects the
    /// probe-rs register file and backs the `is_64_bit` shortcut.
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

    /// Run `f` over this core's `CoreInterface` on the bridge runtime with
    /// the GIL released. The returned future owns the interface, so the
    /// borrow ends inside `py.detach`.
    fn rpc<T, F>(
        &self,
        py: Python<'_>,
        op: &str,
        f: impl Fn(CoreInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        self.rpc_with_deadline(py, DEFAULT_TIMEOUT, op, f)
    }

    /// [`Self::rpc`] with an explicit bridge deadline; used when the
    /// server-side wait can exceed the default 30 s.
    ///
    /// The interface is re-derived on every attempt so a dead-link recovery
    /// (`call_with_recovery`) retries against the rebuilt session.
    fn rpc_with_deadline<T, F>(
        &self,
        py: Python<'_>,
        deadline: Duration,
        op: &str,
        f: impl Fn(CoreInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        let conn = self.conn.clone();
        let core = self.core;
        py.detach(move || {
            call_with_recovery(&conn, deadline, op, || {
                let core_if = conn.session()?.core(core as usize);
                rpc_call(deadline, op, f(core_if))
            })
        })
        .map_err(to_bridge_err)
    }

    /// Re-apply this core's tracked hardware breakpoints (after ops that
    /// clear the FP comparators server-side: reset, reset_and_halt, flash).
    fn reapply_tracked_breakpoints(&self, py: Python<'_>) -> PyResult<()> {
        let conn = self.conn.clone();
        let core = self.core;
        py.detach(|| reapply_breakpoints(&conn, core))
            .map_err(to_bridge_err)
    }

    /// Same as [`Self::rpc`] for calls that address the session rather than
    /// a single core (target metadata, stepping, disassembly).
    fn rpc_session<T, F>(
        &self,
        py: Python<'_>,
        op: &str,
        f: impl Fn(SessionInterface) -> F + Send,
    ) -> PyResult<T>
    where
        F: Future<Output = Result<T, ClientError>>,
        T: Send,
    {
        let conn = self.conn.clone();
        py.detach(move || {
            call_with_recovery(&conn, DEFAULT_TIMEOUT, op, || {
                let session = conn.session()?.clone();
                rpc_call(DEFAULT_TIMEOUT, op, f(session))
            })
        })
        .map_err(to_bridge_err)
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

    /// The core's register file, from live metadata (one RPC unless the
    /// instruction set was already cached). Names, ids and widths come
    /// straight from probe-rs, so every architecture the server supports
    /// resolves — not only the ones this facade once hard-coded.
    fn registers_for_core(&self, py: Python<'_>) -> PyResult<&'static CoreRegisters> {
        let meta = self.core_metadata(py)?;
        let _ = self.instruction_set.set(meta.instruction_set);
        Ok(registers_for(
            meta.instruction_set,
            meta.fpu_support,
            meta.floating_point_register_count,
        ))
    }

    /// Resolve a register name or raw id to a wire register id. Names are
    /// matched case-insensitively against the core's register file (primary
    /// names and role aliases), so `r0`/`R0`, `xpsr`, `pc` and the ABI names
    /// all work. Integers pass through untouched.
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
        let name = name_or_id.extract::<String>()?;
        let regs = self.registers_for_core(py)?;
        if let Some(reg) = regs
            .all_registers()
            .find(|r| register_names(r).any(|n| n.eq_ignore_ascii_case(&name)))
        {
            return Ok((WireRegisterId(reg.id().0), Some(reg.size_in_bits() as u32)));
        }
        for (alias, role) in ROLE_ALIASES {
            if alias.eq_ignore_ascii_case(&name)
                && let Some(reg) = role_register(regs, *role)
            {
                return Ok((WireRegisterId(reg.id().0), Some(reg.size_in_bits() as u32)));
            }
        }
        Err(unknown_register(&name, regs))
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
    let addresses = conn
        .breakpoints
        .lock()
        .get(&core)
        .cloned()
        .unwrap_or_default();
    if addresses.is_empty() {
        return Ok(());
    }
    let session = conn.session()?.clone();
    let results = Bridge::global()?.wait(DEFAULT_TIMEOUT, "reapply breakpoints", async move {
        session
            .core(core as usize)
            .set_hw_breakpoints(addresses)
            .await
    })?;
    for result in results.map_err(|e| BridgeError::from_client(e, Phase::Call))? {
        result.map_err(|e| {
            BridgeError::Rpc(
                format!("failed to re-apply a hardware breakpoint on core {core}: {e}"),
                "Breakpoint",
            )
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
        self.rpc(py, "reset core", |c| async move { c.reset().await })?;
        // armv7m `reset()` issues SYSRESETREQ without clearing DHCSR.C_HALT,
        // which lives in the debug domain and survives the system reset — so a
        // core halted before the reset (e.g. by `step()`) boots halted again.
        // The API contract is "reset, then execute": resume when the core
        // reports halted or sleeping (running is the common case; this costs
        // one extra status round-trip).
        self.rpc(py, "resume after reset", |c| async move {
            // The server caches `Unknown` right after a reset, so anything
            // that is not demonstrably Running gets an explicit resume;
            // running an (unhalted) sleeping core is harmless, and this
            // covers the cached-status case without a second round-trip.
            match c.status().await? {
                WireCoreStatus::Running => Ok(()),
                _ => c.run().await,
            }
        })?;
        self.reapply_tracked_breakpoints(py)
    }
    fn reset_and_halt(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<u64> {
        let deadline = Duration::from_millis(timeout_ms) + BRIDGE_SLACK;
        let info = self.rpc_with_deadline(py, deadline, "reset and halt core", |c| async move {
            c.reset_and_halt(Duration::from_millis(timeout_ms)).await
        })?;
        // A system reset clears the FP comparators; the tracked set is the
        // debugger's intent and must survive run-control like it survives
        // flashing.
        self.reapply_tracked_breakpoints(py)?;
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
            call_with_recovery(&conn, deadline, "wait for core halted", || {
                let session = conn.session()?.clone();
                Bridge::global()?
                    .wait(deadline, "wait for core halted", async move {
                        let core_if = session.core(core as usize);
                        loop {
                            match core_if.status().await {
                                Ok(WireCoreStatus::Halted(_)) => return Ok(()),
                                Ok(_) => tokio::time::sleep(HALT_POLL_INTERVAL).await,
                                Err(e) => return Err(e),
                            }
                        }
                    })?
                    .map_err(|e| BridgeError::from_client(e, Phase::Call))
            })
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
        self.rpc(py, "write memory 8", |c| {
            let data = data.clone();
            async move { c.write_memory_8(addr, data).await }
        })
    }

    fn write_16(&self, py: Python<'_>, addr: u64, data: Vec<u16>) -> PyResult<()> {
        self.rpc(py, "write memory 16", |c| {
            let data = data.clone();
            async move { c.write_memory_16(addr, data).await }
        })
    }

    fn write_32(&self, py: Python<'_>, addr: u64, data: Vec<u32>) -> PyResult<()> {
        self.rpc(py, "write memory 32", |c| {
            let data = data.clone();
            async move { c.write_memory_32(addr, data).await }
        })
    }

    fn write_64(&self, py: Python<'_>, addr: u64, data: Vec<u64>) -> PyResult<()> {
        self.rpc(py, "write memory 64", |c| {
            let data = data.clone();
            async move { c.write_memory_64(addr, data).await }
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
        self.rpc(py, "write memory", |c| {
            let data = data.clone();
            async move { c.write_memory_8(addr, data).await }
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
        let regs = self.registers_for_core(py)?;
        let reg = role_register(regs, RegisterRole::ProgramCounter)
            .ok_or_else(|| role_register_missing("program counter"))?;
        self.read_reg_u64(py, WireRegisterId(reg.id().0))
    }

    fn read_sp(&self, py: Python<'_>) -> PyResult<u64> {
        let regs = self.registers_for_core(py)?;
        let reg = role_register(regs, RegisterRole::StackPointer)
            .ok_or_else(|| role_register_missing("stack pointer"))?;
        self.read_reg_u64(py, WireRegisterId(reg.id().0))
    }

    fn read_fp(&self, py: Python<'_>) -> PyResult<u64> {
        let regs = self.registers_for_core(py)?;
        let reg = role_register(regs, RegisterRole::FramePointer)
            .ok_or_else(|| role_register_missing("frame pointer"))?;
        self.read_reg_u64(py, WireRegisterId(reg.id().0))
    }

    fn read_return_address(&self, py: Python<'_>) -> PyResult<u64> {
        let regs = self.registers_for_core(py)?;
        let reg = role_register(regs, RegisterRole::ReturnAddress)
            .ok_or_else(|| role_register_missing("return address"))?;
        self.read_reg_u64(py, WireRegisterId(reg.id().0))
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
        self.rpc(py, "clear hw breakpoints", |c| {
            let addresses = addresses.clone();
            async move { c.clear_hw_breakpoints(addresses).await }
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
        self.rpc(py, "clear hw breakpoints", |c| {
            let addresses = addresses.clone();
            async move { c.clear_hw_breakpoints(addresses).await }
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
             the catch clears on core reset"
                .into(),
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
            .ok_or_else(|| py_err_with_kind("this core has no floating-point unit".into(), "Rpc"))
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

    /// Snapshot of every register probe-rs defines for this core's
    /// architecture. Names are lowercased, and the arch-neutral
    /// `pc`/`sp`/`lr`/`fp` aliases are appended when the file exposes the
    /// register under another name.
    fn registers(&self, py: Python<'_>) -> PyResult<Vec<CoreRegisterInfo>> {
        let regs = self.registers_for_core(py)?;
        let mut out: Vec<CoreRegisterInfo> = regs
            .all_registers()
            .map(|r| CoreRegisterInfo {
                name: r.name().to_lowercase(),
                id: r.id().0,
                size_in_bits: r.size_in_bits() as u32,
            })
            .collect();
        let mut seen: HashSet<String> = out.iter().map(|r| r.name.clone()).collect();
        for (alias, role) in ROLE_ALIASES {
            if let Some(reg) = role_register(regs, *role)
                && seen.insert((*alias).to_string())
            {
                out.push(CoreRegisterInfo {
                    name: (*alias).to_string(),
                    id: reg.id().0,
                    size_in_bits: reg.size_in_bits() as u32,
                });
            }
        }
        Ok(out)
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
        let response = self.rpc_session(py, "step core", |s| async move {
            s.debug_step(core, mode).await
        })?;
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
}

/// Error for a word read the server answered with no elements.
fn empty_read(op: &str) -> PyErr {
    py_err_with_kind(format!("server returned no data for {op}"), "Rpc")
}
