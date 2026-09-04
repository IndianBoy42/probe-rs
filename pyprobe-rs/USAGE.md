# pyprobe-rs — full usage guide

Python bindings for the [probe-rs](https://github.com/probe-rs/probe-rs) embedded
debugging toolkit. This document is the complete reference; each section notes the
underlying probe-rs Rust API being exposed.

- [Installation](#installation)
- [Execution model](#execution-model)
- [Probes](#probes)
- [Sessions and configuration](#sessions-and-configuration)
- [Core control](#core-control)
- [Memory access](#memory-access)
- [Core registers](#core-registers)
- [Breakpoints and vector catch](#breakpoints-and-vector-catch)
- [Core information](#core-information)
- [Targets and the registry](#targets-and-the-registry)
- [Flashing](#flashing)
- [RTT](#rtt)
- [SWO / SWV tracing](#swo--swv-tracing)
- [Core dumps](#core-dumps)
- [Error handling](#error-handling)
- [Enums](#enums)
- [Testing without hardware](#testing-without-hardware)
- [Python ↔ Rust API mapping](#python--rust-api-mapping)

## Installation

Builds against the probe-rs workspace this crate lives in.

```bash
pip install maturin
cd pyprobe-rs
maturin develop --release      # into the active virtualenv
```

The `espressif` chip-support plugin is enabled by default
(`maturin develop --features espressif` is implied).

## Execution model

Three rules explain every surprising behavior:

1. **Consuming calls.** `Probe.attach()` / `Probe.attach_under_reset()` consume the
   probe (Rust move semantics) — **including when the call fails**, so a retry
   needs a freshly opened `Probe` (pattern in [Probes](#probes)). Using a consumed
   probe raises `ValueError: Probe consumed`. Same for `FakeProbe` once converted
   with `Probe.from_fake()` / `FakeProbe.into_probe()`.
2. **Lending handles.** `sess.core(i)`, `sess.attach_rtt(...)`, `rtt.up_channel(n)`
   return handles that re-borrow the underlying session on every call. Handles keep
   the session alive; there is no lifetime juggling on the Python side.
3. **GIL blocking.** Operations hold the Python GIL for their full duration
   (probe-rs's `FlashProgress` handler is not `Send`, so the work cannot be moved
   under `allow_threads`). A long flash freezes the interpreter — and Jupyter
   heartbeats — until it finishes. Keep flashes out of hot loops or run them in a
   worker process/thread (the GIL is released for other threads only *between*
   probe operations, not during them).
4. **Release by drop.** A `Session` owns the probe; there is no explicit close.
   The probe is released when the `Session` is dropped — deterministic in CPython
   on the last reference (`sess = None`, then `gc.collect()` if other references
   may linger). `Probe.detach()` only applies to a `Probe` that has not been
   attached.

## Probes

Rust API: [`probe_rs::probe`](https://docs.rs/probe-rs/latest/probe_rs/probe/) —
`Lister`, `DebugProbeInfo`, `DebugProbeSelector`, `Probe`.

```python
import pyprobe_rs as pr

probes = pr.list_probes()                  # -> list[ProbeInfo]; == Lister::new().list_all()
for p in probes:
    print(p.vid, p.pid, p.serial, p.identifier)

probe = probes[0].open()                   # == DebugProbeInfo::open()
# or by selector:
probe = pr.Lister().open("1366:1015")                      # == Lister::open(selector)
sel = pr.DebugProbeSelector(0x1366, 0x1015, "SERIAL")       # str(sel) == "1366:1015:SERIAL"
probe = pr.Lister().open(sel)

probe.set_speed(1000)                      # kHz; returns the applied speed
probe.select_protocol(pr.WireProtocol.Swd)
print(probe.get_name(), probe.speed_khz(), probe.get_target_voltage())
print(probe.protocol())                    # WireProtocol.Swd or None
print(probe.has_arm_debug_interface(), probe.has_riscv_interface(), probe.has_xtensa_interface())

probe.target_reset()                       # pulse nRESET
probe.target_reset_assert()                # hold target in reset ...
probe.target_reset_deassert()              # ... release it
probe.detach()
```

`get_name()` returns the debug-formatted descriptor of the underlying probe — a
long USB dump on CMSIS-DAP probes. Use `ProbeInfo.identifier` for a short name.
Probes without voltage sensing return `None` from `get_target_voltage()`.

Attach (consuming):

```python
sess = probe.attach("nrf52840_xxaa", pr.Permissions())
sess = probe.attach_under_reset("nrf52840_xxaa", pr.Permissions().allow_erase_all())
```

`attach()` consumes `probe` even when it fails — re-open before retrying:

```python
try:
    sess = probe.attach(chip, pr.Permissions())
except pr.ProbeRsError:
    probe = pr.list_probes()[0].open()   # the old `probe` is consumed
    sess = probe.attach_under_reset(chip, pr.Permissions())
```

## Sessions and configuration

Rust API: `probe_rs::Session`, `probe_rs::SessionConfig`, `probe_rs::Permissions`.

```python
sess = pr.Session.auto_attach("nrf52840_xxaa")   # picks first matching probe

cfg = pr.SessionConfig(
    speed=1000,                       # kHz
    protocol=pr.WireProtocol.Swd,
    permissions=pr.Permissions(),
)
sess = pr.Session.auto_attach("nrf52840_xxaa", cfg)

perms = pr.Permissions()
perms.allow_erase_all()               # needed for chip erase / unlock operations

sess.list_cores()                     # -> [(0, CoreType.Armv7em), ...]
sess.target()                         # -> Target (see below)
sess.architecture()                   # -> Architecture.Arm / Riscv / Xtensa

sess.resume_all_cores()
sess.clear_all_hw_breakpoints()       # across all cores
```

`Target` (read-only, Rust `probe_rs_target::Target`):

```python
t = sess.target()
t.name                # str
t.architecture        # Architecture enum
t.cores               # [(core_name: str, CoreType)]
t.flash_algorithms    # [str]
t.memory_map          # [("ram"|"nvm"|"generic", start: int, end: int)]
```

## Core control

Rust API: `probe_rs::Core` / `CoreInterface`. A `CoreHandle` is obtained from the
session and re-borrows it on every call.

```python
core = sess.core(0)

pc = core.halt(1000)                  # halt with timeout (ms) -> PC after halt
core.run()                            # resume
core.reset()                          # system reset, keep running
pc = core.reset_and_halt(1000)        # reset and halt -> PC
pc = core.step()                      # single instruction -> PC

core.wait_for_core_halted(2000)       # raises ProbeRsError on timeout
core.core_halted()                    # -> bool

st = core.status()                    # -> CoreStatus (see Enums)
st.state              # CoreState.Running / Halted / LockedUp / Sleeping / Unknown
st.is_halted, st.is_running
st.halt_reason        # HaltReason | None
    # .kind  -> HaltKind.Breakpoint / Watchpoint / Step / Request / External /
    #            Exception / Multiple / Unknown
    # .detail -> str (e.g. "hardware", "software", "semihosting: ...")
```

Run/halt transitions on a freely running target can sporadically raise an
ARM-specific error on some probes (seen with CMSIS-DAP V2). The reliable pattern
is `halt()` → do the work → `run()`, polling `core_halted()` in a loop rather
than relying on one blocking `wait_for_core_halted()` call.

## Memory access

Rust API: `probe_rs::MemoryInterface`. Addresses are raw bus addresses.

```python
core.read_word_8(0x2000_0000)          # -> int (u8)
core.read_word_16(0x2000_0000)         # -> int
core.read_word_32(0x2000_0000)         # -> int
core.read_word_64(0x2000_0000)         # -> int

core.write_word_8(0x2000_0000, 0xFF)
core.write_word_32(0x2000_0000, 0xDEADBEEF)

core.read_8(0x2000_0000, 64)           # -> list[int]
core.read_16(0x2000_0000, 32)          # -> list[int]
core.read_32(0x2000_0000, 16)          # -> list[int]
core.read_64(0x2000_0000, 8)           # -> list[int]

core.write_8(0x2000_0000, bytes(64))   # accepts any bytes-like list[int]
core.write_16(0x2000_0000, [0x1234, 0xF00D])
core.write_32(0x2000_0000, [0xDEAD, 0xBEEF])
core.write_64(0x2000_0000, [0x1122334455667788])

data = core.read(0x2000_0000, 128)     # generic byte read -> bytes
core.write(0x2000_0000, b"\x00" * 16)  # generic byte write
```

Width matters for speed: byte and 16-bit transactions move a fraction of the data
per probe round-trip. Dumping 128 KiB of RAM took 1.7 s with `read_32` vs 6.7 s
with `read_8` (Cortex-M7, CMSIS-DAP V2). Prefer the widest width your alignment
allows, and batch with the block methods instead of `read_word_*` loops.

## Core registers

Rust API: `CoreInterface::read_core_reg` / `write_core_reg`, `CoreRegisters`.

```python
core.read_core_reg("PC")               # by name (case-insensitive)
core.read_core_reg("r0")               # canonical names: R0..R15, MSP, PSP, XPSR, ...
core.read_core_reg(0)                  # or numeric register id
core.write_core_reg("R0", 0x2000_1000)

# architecture-aware aliases, resolved via the core itself:
core.read_core_reg("pc")               # PC (R15 on ARM, `pc` on RISC-V)
core.read_core_reg("sp")               # stack pointer
core.read_core_reg("lr")               # link/return address register
core.read_core_reg("fp")               # frame pointer
```

- Unknown names raise `KeyError` whose message lists all valid names.
- Writes are validated against the register's width: a value that does not fit
  raises `OverflowError` (nothing is silently truncated).

Convenience accessors (return `int`):

```python
core.read_pc()
core.read_sp()
core.read_fp()
core.read_return_address()
```

Enumerate registers:

```python
for reg in core.registers():           # -> [CoreRegisterInfo]
    print(reg.name, reg.id, reg.size_in_bits)
```

## Breakpoints and vector catch

Rust API: `CoreInterface` breakpoint / `VectorCatchCondition` methods.

```python
core.available_breakpoint_units()      # -> int (FPB comparators)
core.set_hw_breakpoint(0x0800_1234)
core.set_hw_breakpoint_unit(1, 0x0800_1234)   # specific comparator unit
core.clear_hw_breakpoint(0x0800_1234)
core.clear_all_hw_breakpoints()

core.enable_vector_catch(pr.VectorCatchCondition.HardFault)
core.disable_vector_catch(pr.VectorCatchCondition.HardFault)
```

## Core information

Rust API: `CoreInterface` metadata methods.

```python
core.id()                       # -> int, core index within the session
core.architecture()             # Architecture
core.core_type()                # CoreType
core.instruction_set()          # InstructionSet
core.endianness()               # Endian
core.fpu_support()              # bool
core.is_64_bit()                # bool
core.floating_point_register_count()  # int
core.memory_regions()           # [(kind, start, end)] — same shape as Target.memory_map
```

## Targets and the registry

Rust API: `probe_rs::config::Registry`, `config::Target`.

```python
names = pr.list_targets()                       # all builtin chip names
target = pr.get_target_by_name("nrf52840_xxaa") # convenience lookup

reg = pr.Registry.from_builtin_families()
reg.families()                                  # -> ["nRF52 Family", ...]
reg.search_chips("nRF52840")                    # -> [str]
reg.get_target_by_name("nrf52840_xxaa")         # -> Target
```

## Flashing

Rust API: [`probe_rs::flashing`](https://docs.rs/probe-rs/latest/probe_rs/flashing/)
— `download_file_with_options`, `build_loader`, `FlashLoader`, `DownloadOptions`,
`FlashProgress`.

```python
opts = pr.DownloadOptions()        # defaults: verify=True, keep_unwritten_bytes=True;
                                   # everything else False, ram_chunk_size=None
opts.dry_run = True                # everything except the actual write
opts.do_chip_erase = True          # erase the whole chip, not just used sectors
opts.skip_erase = True             # no erase pass at all (pre-erased targets)
opts.preverify = True              # verify contents before programming
opts.disable_double_buffering = True
opts.ram_chunk_size = 4096         # None = upstream default chunking

pr.download_file(sess, "fw.elf", "elf", opts)
```

Flashing leaves the core halted. Boot the new image afterwards with
`sess.core(0).reset()` (system reset and run) or `core.run()`.

Formats: `elf`, `hex` / `ihex` / `intelhex`, `bin` / `binary`, `uf2`.
Raw `bin` images have no address information — position them explicitly:

```python
pr.download_file(sess, "fw.bin", "bin", base_address=0x1000_0000)
pr.download_file(sess, "fw.bin", "bin", base_address=0x1000_0000, skip=512)
# base_address/skip are rejected for all other formats
```

Progress callback — receives `ProgressEvent` objects synchronously:

```python
def on_progress(ev):
    # ev.kind: FlashLayoutReady | AddProgressBar | Started | Progress |
    #           Finished | Failed | DiagnosticMessage
    # ev.operation: str, ev.size: int, ev.total: int | None, ev.time_s: float
    print(ev.kind, ev.operation, ev.size)

pr.download_file(sess, "fw.hex", "hex", progress=on_progress)
```

Callback contract:
- runs while the session is mutably borrowed — it **must not** call any
  `Session`/`CoreHandle`/RTT API;
- cannot abort the flash (upstream handler API is infallible);
- exceptions it raises are printed to stderr, not propagated or swallowed silently.

Staged writes (Rust `flashing::FlashLoader`):

```python
loader = pr.build_loader(sess, "fw.bin", "bin", base_address=0x1000_0000)
loader.add_data(0x1000_2000, b"\xde\xad\xbe\xef")   # override/append data
loader.commit(sess)                                  # erase + program + verify
```

Chip erase without an image:

```python
sess.erase_all()                       # vendor erase-all sequence if available,
                                       # otherwise flashing-based erase-all
sess.has_sequence_erase_all()          # vendor sequence available?
sess.sequence_erase_all()              # vendor sequence only
```

## RTT

Rust API: [`probe_rs::rtt`](https://docs.rs/probe-rs/latest/probe_rs/rtt/) — `Rtt`,
`RttUpChannel`, `RttDownChannel`, `ChannelMode`.

```python
rtt = sess.attach_rtt(0)               # scans RAM for the RTT control block
rtt = sess.attach_rtt(0, ptr)          # or attach at a known control-block address

rtt.up_channels()                      # -> [ChannelInfo(number, name, buffer_size)]
rtt.down_channels()
rtt.ptr()                              # control block address

up = rtt.up_channel(0)
data = up.read()                       # non-blocking; up to 4096 bytes
data = up.read(1024)
text = up.read_str()                   # UTF-8 (lossy) of the same read
up.mode()                              # -> ChannelMode
up.set_mode(pr.ChannelMode.BlockIfFull)

down = rtt.down_channel(0)
n = down.write(b"hi\n")                # returns bytes actually written
n = down.write_str("hi\n")             # same, from str
```

Caveats:
- channel lists are a snapshot taken at attach time; if the target re-initializes
  RTT, call `sess.attach_rtt(...)` again to rescan;
- `write`/`write_str` may write **partially** depending on the channel mode
  (`NoBlockSkip`/`NoBlockTrim` drop data when the target isn't reading) — check the
  returned count.
- channel handles stay valid across core halt/run cycles (each call re-borrows
  the session); re-attach only when the control block itself changes;
- a target can host **several RTT control blocks** (bootloader + app, or a
  transport library instantiating its own), and scan mode returns the first match
  in RAM — not necessarily the block your console uses. The magic also survives
  warm resets (SEGGER's init-once idiom), so a stale block still looks live.
  Wrong-block symptoms: down-channel writes get no reaction and up-channel data
  never advances while the target is clearly logging elsewhere. Fingerprint the
  live console by its channel names — an abandoned block decays into garbage
  names (fragments of unrelated strings) instead of e.g. `"Terminal"` — or attach
  explicitly with `sess.attach_rtt(0, ptr)`.

## SWO / SWV tracing

Rust API: `Session::setup_tracing`, `Session::swo_reader`, ARM SWV data trace units.

```python
sess.setup_tracing(0, pr.TraceSink.Swo)       # Swo | Tpiu | TraceMemory
sess.add_swv_data_trace(0, 0x2000_0000)       # ITM port 0 watches this address
...
raw = sess.read_trace_data()                   # -> bytes (same as read_swo())
sess.remove_swv_data_trace(0)
sess.disable_swv(0)
```

`read_trace_data` returns whatever the OS buffer currently holds; poll it from your
own loop. Decoding ITM/SWO framing is not provided.

## Core dumps

Rust API: `probe_rs::CoreDump` (`dump_core`, `load`, `store`).

```python
dump = pr.CoreDump.capture(sess, 0, [
    (0x2000_0000, 0x2001_0000),    # (start, end) RAM regions to snapshot
])                                  # halted core: registers + memory
dump.store("crash.pcd")

dump = pr.CoreDump.load("crash.pcd")
dump.registers                     # {register_id: value} (ints, full width)
dump.data                          # [(start_address, bytes), ...]
```

## Error handling

Every probe-rs failure raises `pr.ProbeRsError` (a subclass of `Exception`) with a
`.kind` attribute for programmatic branching:

```python
try:
    rtt = sess.attach_rtt(0)
except pr.ProbeRsError as e:
    print(e.kind)      # "Rtt"
    print(e)           # full probe-rs diagnostic
```

| kind | source |
|---|---|
| `Timeout` | `probe_rs::Error::Timeout` |
| `Probe` | probe access/transfer errors |
| `Arm`, `Riscv`, `Xtensa` | architecture-specific errors |
| `ChipNotFound` | unknown target name |
| `MissingPermissions` | operation needs `Permissions.allow_erase_all()` |
| `Core`, `Register`, `Memory`, `Breakpoint` | `probe_rs::Error` subcategories |
| `Rtt` | `rtt::Error` |
| `FileDownloadError`, `FlashError` | flashing |
| `CoreDumpError` | core dumps |
| `ArmError`, `DebugProbeError`, … | short type name of the underlying error |

Python-side argument mistakes still raise builtin exceptions
(`ValueError`, `TypeError`, `KeyError`, `IndexError`, `OverflowError`).

## Enums

All enums live at module level and support `==` comparison:

| Python | Values | Rust |
|---|---|---|
| `WireProtocol` | `Swd`, `Jtag` | `probe_rs::probe::WireProtocol` |
| `Architecture` | `Arm`, `Riscv`, `Xtensa` | `probe_rs_target::Architecture` |
| `CoreType` | `Armv6m`, `Armv7a`, `Armv7r`, `Armv7m`, `Armv7em`, `Armv8a`, `Armv8m`, `Riscv`, `Riscv64`, `Xtensa` | `probe_rs_target::CoreType` |
| `InstructionSet` | `Thumb2`, `A32`, `A64`, `RV32`, `RV32C`, `RV64`, `RV64C`, `Xtensa` | `probe_rs_target::InstructionSet` |
| `Endian` | `Little`, `Big` | `probe_rs_target::Endian` |
| `ChannelMode` | `NoBlockSkip`, `NoBlockTrim`, `BlockIfFull` | `rtt::ChannelMode` |
| `VectorCatchCondition` | `HardFault`, `CoreReset`, `SecureFault`, `All`, `Svc`, `Hlt` | `VectorCatchCondition` |
| `CoreState` | `Running`, `Halted`, `LockedUp`, `Sleeping`, `Unknown` | `CoreStatus` |
| `HaltKind` | `Breakpoint`, `Watchpoint`, `Step`, `Request`, `External`, `Exception`, `Multiple`, `Unknown` | `HaltReason` |
| `TraceSink` | `Swo`, `Tpiu`, `TraceMemory` | `arm::component::TraceSink` |

## Testing without hardware

Rust API: `probe_rs::probe::fake_probe::FakeProbe` with a mocked Cortex-M core.

```python
fp = pr.FakeProbe.with_mocked_core()
sess = pr.Probe.from_fake(fp).attach("Cortex-M4", pr.Permissions())
core = sess.core(0)
core.halt(1000)
core.write_word_32(0x2000_0000, 0xDEADBEEF)
```

The mock exercises the full binding surface (control flow, memory, registers,
error paths) but does not model flash or RTT contents. Note: `setup_tracing` on
the fake aborts the process (mock DAP limitation) — don't call it here.

## Python ↔ Rust API mapping

| Python | probe-rs Rust |
|---|---|
| `pr.list_probes()` | `probe_rs::probe::Lister::new().list_all()` |
| `pr.Lister().open(sel)` | `Lister::open(DebugProbeSelector)` |
| `ProbeInfo.open()` | `DebugProbeInfo::open()` |
| `Probe.attach()` / `.attach_under_reset()` | `Probe::attach` / `attach_under_reset` |
| `Probe.set_speed()` / `.select_protocol()` | `Probe::set_speed` / `select_protocol` |
| `Probe.target_reset*()` | `Probe::target_reset*` |
| `pr.Session.auto_attach()` | `probe_rs::Session::auto_attach` |
| `sess.core(i)` | `Session::core(i)` |
| `core.halt/run/reset/step` | `Core::halt/run/reset/step` |
| `core.read_word_32(...)` etc. | `MemoryInterface::read_word_32` etc. |
| `core.read_8/16/32/64`, `core.write_8/16/32/64`, `core.read`, `core.write` | `MemoryInterface` block methods |
| `core.read_core_reg / write_core_reg` | `CoreInterface::read_core_reg / write_core_reg` |
| `core.read_pc()` | `CoreInterface::program_counter()` + `read_core_reg` |
| `core.set_hw_breakpoint()` | `CoreInterface::set_hw_breakpoint` |
| `core.enable_vector_catch()` | `CoreInterface::enable_vector_catch` |
| `sess.setup_tracing(i, TraceSink)` | `Session::setup_tracing(i, TraceSink)` |
| `sess.add_swv_data_trace()` | `Session::add_swv_data_trace` |
| `sess.read_trace_data()` | `Session::swo_reader().read_to_end()` |
| `sess.erase_all()` | `Session::sequence_erase_all` / `flashing::erase_all` |
| `sess.attach_rtt(i, ptr)` | `rtt::Rtt::attach` / `attach_at` |
| `up.read()` / `up.set_mode()` | `RttUpChannel::read` / `RttChannel::set_mode` |
| `down.write()` | `RttDownChannel::write` |
| `pr.download_file()` | `flashing::download_file_with_options` |
| `pr.build_loader()` | `flashing::build_loader` |
| `FlashLoader.add_data() / .commit()` | `flashing::FlashLoader::add_data / commit` |
| `pr.DownloadOptions` | `flashing::DownloadOptions` |
| `pr.ProgressEvent` | `flashing::ProgressEvent` |
| `pr.CoreDump.capture()` | `CoreDump::dump_core` |
| `pr.CoreDump.load() / .store()` | `CoreDump::load` / `store` |
| `pr.Registry` | `config::Registry` |
| `pr.SessionConfig` / `pr.Permissions` | `probe_rs::SessionConfig` / `Permissions` |
| `pr.FakeProbe` | `probe::fake_probe::FakeProbe` |
| `pr.ProbeRsError` | `probe_rs::Error` and friends (as `.kind`) |
