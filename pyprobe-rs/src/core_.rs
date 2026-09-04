//! CoreHandle: the lending-pattern solution.

use crate::error::to_py_err;
use crate::session::Session;
use crate::types::*;
use pyo3::prelude::*;
use probe_rs::MemoryInterface;
use probe_rs::{Core, CoreInterface, RegisterId, RegisterValue};
use probe_rs_target::MemoryRegion;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

/// Handle to a core within a session. Re-borrows on each call.
#[pyclass(unsendable)]
pub struct CoreHandle {
    session: Py<Session>,
    core_index: usize,
    register_map: RefCell<Option<HashMap<String, (RegisterId, u32)>>>,
}

impl CoreHandle {
    pub fn new(session: Py<Session>, core_index: usize) -> Self {
        Self {
            session,
            core_index,
            register_map: RefCell::new(None),
        }
    }

    fn with_core<R>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&mut Core<'_>) -> PyResult<R>,
    ) -> PyResult<R> {
        let mut sess = self.session.borrow_mut(py);
        let sess_ref = &mut sess.inner;
        let mut core = sess_ref.core(self.core_index).map_err(to_py_err)?;
        f(&mut core)
    }
}

/// Info about a single register.
#[pyclass]
#[derive(Clone)]
pub struct CoreRegisterInfo {
    #[pyo3(get)]
    pub name: String,
    #[pyo3(get)]
    pub id: u16,
    #[pyo3(get)]
    pub size_in_bits: u32,
}

fn register_value_to_u128(v: RegisterValue) -> u128 {
    match v {
        RegisterValue::U32(x) => x as u128,
        RegisterValue::U64(x) => x as u128,
        RegisterValue::U128(x) => x,
    }
}

/// Build a RegisterValue validated against the register's width in bits.
fn register_value_from_u128(value: u128, bits: u32) -> PyResult<RegisterValue> {
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
    Ok(match bits {
        0..=32 => RegisterValue::U32(value as u32),
        33..=64 => RegisterValue::U64(value as u64),
        _ => RegisterValue::U128(value),
    })
}

/// Width of the register with the given ID; 32 for unknown IDs.
fn register_bits_by_id(core: &Core<'_>, id: RegisterId) -> u32 {
    core.registers()
        .all_registers()
        .find(|r| r.id().0 == id.0)
        .map(|r| r.size_in_bits() as u32)
        .unwrap_or(32)
}

#[pymethods]
impl CoreHandle {
    // ── Control ─────────────────────────────────────────────────────

    fn halt(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<u64> {
        self.with_core(py, |core| {
            let info = core.halt(Duration::from_millis(timeout_ms)).map_err(to_py_err)?;
            Ok(info.pc)
        })
    }

    fn run(&self, py: Python<'_>) -> PyResult<()> {
        self.with_core(py, |core| core.run().map_err(to_py_err))
    }

    fn reset(&self, py: Python<'_>) -> PyResult<()> {
        self.with_core(py, |core| core.reset().map_err(to_py_err))
    }

    fn reset_and_halt(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<u64> {
        self.with_core(py, |core| {
            let info = core
                .reset_and_halt(Duration::from_millis(timeout_ms))
                .map_err(to_py_err)?;
            Ok(info.pc)
        })
    }

    fn step(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_core(py, |core| {
            let info = core.step().map_err(to_py_err)?;
            Ok(info.pc)
        })
    }

    fn wait_for_core_halted(&self, py: Python<'_>, timeout_ms: u64) -> PyResult<()> {
        self.with_core(py, |core| {
            core.wait_for_core_halted(Duration::from_millis(timeout_ms))
                .map_err(to_py_err)
        })
    }

    fn core_halted(&self, py: Python<'_>) -> PyResult<bool> {
        self.with_core(py, |core| core.core_halted().map_err(to_py_err))
    }

    fn status(&self, py: Python<'_>) -> PyResult<CoreStatusProxy> {
        self.with_core(py, |core| {
            Ok(core_status_to_py(core.status().map_err(to_py_err)?))
        })
    }

    // ── Single-word memory ──────────────────────────────────────────

    fn read_word_8(&self, py: Python<'_>, addr: u64) -> PyResult<u8> {
        self.with_core(py, |core| core.read_word_8(addr).map_err(to_py_err))
    }
    fn read_word_16(&self, py: Python<'_>, addr: u64) -> PyResult<u16> {
        self.with_core(py, |core| core.read_word_16(addr).map_err(to_py_err))
    }
    fn read_word_32(&self, py: Python<'_>, addr: u64) -> PyResult<u32> {
        self.with_core(py, |core| core.read_word_32(addr).map_err(to_py_err))
    }
    fn read_word_64(&self, py: Python<'_>, addr: u64) -> PyResult<u64> {
        self.with_core(py, |core| core.read_word_64(addr).map_err(to_py_err))
    }

    fn write_word_8(&self, py: Python<'_>, addr: u64, val: u8) -> PyResult<()> {
        self.with_core(py, |core| core.write_word_8(addr, val).map_err(to_py_err))
    }
    fn write_word_16(&self, py: Python<'_>, addr: u64, val: u16) -> PyResult<()> {
        self.with_core(py, |core| core.write_word_16(addr, val).map_err(to_py_err))
    }
    fn write_word_32(&self, py: Python<'_>, addr: u64, val: u32) -> PyResult<()> {
        self.with_core(py, |core| core.write_word_32(addr, val).map_err(to_py_err))
    }
    fn write_word_64(&self, py: Python<'_>, addr: u64, val: u64) -> PyResult<()> {
        self.with_core(py, |core| core.write_word_64(addr, val).map_err(to_py_err))
    }

    // ── Block memory ────────────────────────────────────────────────

    fn read_8(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u8>> {
        self.with_core(py, |core| {
            let mut buf = vec![0u8; count];
            core.read_8(addr, &mut buf).map_err(to_py_err)?;
            Ok(buf)
        })
    }

    fn read_16(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u16>> {
        self.with_core(py, |core| {
            let mut buf = vec![0u16; count];
            core.read_16(addr, &mut buf).map_err(to_py_err)?;
            Ok(buf)
        })
    }

    fn read_32(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u32>> {
        self.with_core(py, |core| {
            let mut buf = vec![0u32; count];
            core.read_32(addr, &mut buf).map_err(to_py_err)?;
            Ok(buf)
        })
    }

    fn read_64(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u64>> {
        self.with_core(py, |core| {
            let mut buf = vec![0u64; count];
            core.read_64(addr, &mut buf).map_err(to_py_err)?;
            Ok(buf)
        })
    }

    fn write_8(&self, py: Python<'_>, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.with_core(py, |core| core.write_8(addr, &data).map_err(to_py_err))
    }
    fn write_16(&self, py: Python<'_>, addr: u64, data: Vec<u16>) -> PyResult<()> {
        self.with_core(py, |core| core.write_16(addr, &data).map_err(to_py_err))
    }
    fn write_32(&self, py: Python<'_>, addr: u64, data: Vec<u32>) -> PyResult<()> {
        self.with_core(py, |core| core.write_32(addr, &data).map_err(to_py_err))
    }
    fn write_64(&self, py: Python<'_>, addr: u64, data: Vec<u64>) -> PyResult<()> {
        self.with_core(py, |core| core.write_64(addr, &data).map_err(to_py_err))
    }

    // ── Generic byte memory ────────────────────────────────────────

    fn read(&self, py: Python<'_>, addr: u64, count: usize) -> PyResult<Vec<u8>> {
        self.with_core(py, |core| {
            let mut buf = vec![0u8; count];
            core.read(addr, &mut buf).map_err(to_py_err)?;
            Ok(buf)
        })
    }

    fn write(&self, py: Python<'_>, addr: u64, data: Vec<u8>) -> PyResult<()> {
        self.with_core(py, |core| core.write(addr, &data).map_err(to_py_err))
    }

    // ── Register access ────────────────────────────────────────────

    /// Read a core register by name or numeric ID.
    ///
    /// Names are case-insensitive; common aliases (`pc`, `sp`, `lr`, `fp`)
    /// resolve through the core's architecture descriptions.
    fn read_core_reg(&self, py: Python<'_>, name_or_id: &Bound<'_, PyAny>) -> PyResult<u128> {
        let (reg_id, _) = self.resolve_register(py, name_or_id)?;
        self.with_core(py, |core| {
            let value = core.read_core_reg(reg_id).map_err(to_py_err)?;
            Ok(register_value_to_u128(value))
        })
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
        let (reg_id, known_bits) = self.resolve_register(py, name_or_id)?;
        self.with_core(py, |core| {
            let bits = known_bits.unwrap_or_else(|| register_bits_by_id(core, reg_id));
            let rv = register_value_from_u128(value, bits)?;
            core.write_core_reg(reg_id, rv).map_err(to_py_err)
        })
    }

    // ── Named register shortcuts ──────────────────────────────────

    fn read_pc(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_core(py, |core| {
            let pc = core.program_counter();
            core.read_core_reg::<u64>(pc.id()).map_err(to_py_err)
        })
    }

    fn read_sp(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_core(py, |core| {
            let sp = core.stack_pointer();
            core.read_core_reg::<u64>(sp.id()).map_err(to_py_err)
        })
    }

    fn read_fp(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_core(py, |core| {
            let fp = core.frame_pointer();
            core.read_core_reg::<u64>(fp.id()).map_err(to_py_err)
        })
    }

    fn read_return_address(&self, py: Python<'_>) -> PyResult<u64> {
        self.with_core(py, |core| {
            let ra = core.return_address();
            core.read_core_reg::<u64>(ra.id()).map_err(to_py_err)
        })
    }

    // ── Breakpoints ────────────────────────────────────────────────

    fn available_breakpoint_units(&self, py: Python<'_>) -> PyResult<u32> {
        self.with_core(py, |core| {
            core.available_breakpoint_units().map_err(to_py_err)
        })
    }

    fn set_hw_breakpoint(&self, py: Python<'_>, addr: u64) -> PyResult<()> {
        self.with_core(py, |core| core.set_hw_breakpoint(addr).map_err(to_py_err))
    }

    fn set_hw_breakpoint_unit(&self, py: Python<'_>, unit: usize, addr: u64) -> PyResult<()> {
        self.with_core(py, |core| {
            core.set_hw_breakpoint_unit(unit, addr).map_err(to_py_err)
        })
    }

    fn clear_hw_breakpoint(&self, py: Python<'_>, addr: u64) -> PyResult<()> {
        self.with_core(py, |core| core.clear_hw_breakpoint(addr).map_err(to_py_err))
    }

    fn clear_all_hw_breakpoints(&self, py: Python<'_>) -> PyResult<()> {
        self.with_core(py, |core| core.clear_all_hw_breakpoints().map_err(to_py_err))
    }

    fn enable_vector_catch(
        &self,
        py: Python<'_>,
        condition: VectorCatchConditionProxy,
    ) -> PyResult<()> {
        self.with_core(py, |core| {
            core.enable_vector_catch(py_to_vector_catch(condition))
                .map_err(to_py_err)
        })
    }

    fn disable_vector_catch(
        &self,
        py: Python<'_>,
        condition: VectorCatchConditionProxy,
    ) -> PyResult<()> {
        self.with_core(py, |core| {
            core.disable_vector_catch(py_to_vector_catch(condition))
                .map_err(to_py_err)
        })
    }

    // ── Meta ────────────────────────────────────────────────────────

    fn id(&self, py: Python<'_>) -> PyResult<usize> {
        self.with_core(py, |core| Ok(core.id()))
    }

    fn architecture(&self, py: Python<'_>) -> PyResult<ArchitectureProxy> {
        self.with_core(py, |core| Ok(architecture_to_py(core.architecture())))
    }

    fn core_type(&self, py: Python<'_>) -> PyResult<CoreTypeProxy> {
        self.with_core(py, |core| Ok(core_type_to_py(core.core_type())))
    }

    fn instruction_set(&self, py: Python<'_>) -> PyResult<InstructionSetProxy> {
        self.with_core(py, |core| {
            Ok(instruction_set_to_py(
                core.instruction_set().map_err(to_py_err)?,
            ))
        })
    }

    fn fpu_support(&self, py: Python<'_>) -> PyResult<bool> {
        self.with_core(py, |core| core.fpu_support().map_err(to_py_err))
    }

    fn floating_point_register_count(&self, py: Python<'_>) -> PyResult<usize> {
        self.with_core(py, |core| {
            core.floating_point_register_count().map_err(to_py_err)
        })
    }

    fn is_64_bit(&self, py: Python<'_>) -> PyResult<bool> {
        self.with_core(py, |core| Ok(core.is_64_bit()))
    }

    fn endianness(&self, py: Python<'_>) -> PyResult<EndianProxy> {
        self.with_core(py, |core| {
            Ok(endian_to_py(core.endianness().map_err(to_py_err)?))
        })
    }

    /// Snapshot of all registers on this core.
    fn registers(&self, py: Python<'_>) -> PyResult<Vec<CoreRegisterInfo>> {
        self.with_core(py, |core| {
            Ok(core
                .registers()
                .all_registers()
                .map(|r| CoreRegisterInfo {
                    name: r.name().to_string(),
                    id: r.id().0,
                    size_in_bits: r.size_in_bits() as u32,
                })
                .collect())
        })
    }

    /// Memory regions for this core's target.
    fn memory_regions(&self, py: Python<'_>) -> Vec<(String, u64, u64)> {
        let sess = self.session.borrow(py);
        let mut result = Vec::new();
        let target = sess.inner.target();
        for region in &target.memory_map {
            let (kind, start, end) = match region {
                MemoryRegion::Nvm(r) => ("nvm", r.range.start, r.range.end),
                MemoryRegion::Ram(r) => ("ram", r.range.start, r.range.end),
                MemoryRegion::Generic(r) => ("generic", r.range.start, r.range.end),
            };
            result.push((kind.to_string(), start, end));
        }
        result
    }

    fn __repr__(&self) -> String {
        format!("CoreHandle(index={})", self.core_index)
    }
}

impl CoreHandle {
    /// Resolve a register by name (case-insensitive, with common aliases),
    /// returning its ID and cached width; numeric IDs carry no cached width.
    fn resolve_register(
        &self,
        py: Python<'_>,
        name_or_id: &Bound<'_, PyAny>,
    ) -> PyResult<(RegisterId, Option<u32>)> {
        if let Ok(id) = name_or_id.extract::<u16>() {
            return Ok((RegisterId(id), None));
        }
        if let Ok(id) = name_or_id.extract::<i64>() {
            return Ok((RegisterId(id as u16), None));
        }
        let name = name_or_id.extract::<String>()?;
        self.ensure_register_map(py)?;
        let map = self.register_map.borrow();
        let map = map.as_ref().unwrap();
        map
            .get(&name.to_lowercase())
            .map(|(id, bits)| (*id, Some(*bits)))
            .ok_or_else(|| {
                let mut known: Vec<&str> = map.keys().map(String::as_str).collect();
                known.sort_unstable();
                pyo3::exceptions::PyKeyError::new_err(format!(
                    "unknown register: {name} (known: {})",
                    known.join(", ")
                ))
            })
    }

    /// Build the name → (id, width) cache on first register access. Keys are
    /// lowercase canonical names; arch-neutral aliases (pc/sp/lr/fp) resolve
    /// through probe-rs's core descriptions and never shadow canonical names.
    fn ensure_register_map(&self, py: Python<'_>) -> PyResult<()> {
        let mut slot = self.register_map.borrow_mut();
        if slot.is_some() {
            return Ok(());
        }
        let mut sess = self.session.borrow_mut(py);
        let sess_ref = &mut sess.inner;
        let core = sess_ref.core(self.core_index).map_err(to_py_err)?;
        let mut map: HashMap<String, (RegisterId, u32)> = core
            .registers()
            .all_registers()
            .map(|r| (r.name().to_lowercase(), (r.id(), r.size_in_bits() as u32)))
            .collect();
        for (alias, reg) in [
            ("pc", core.program_counter()),
            ("sp", core.stack_pointer()),
            ("lr", core.return_address()),
            ("fp", core.frame_pointer()),
        ] {
            map.entry(alias.to_string())
                .or_insert((reg.id(), reg.size_in_bits() as u32));
        }
        *slot = Some(map);
        Ok(())
    }
}
