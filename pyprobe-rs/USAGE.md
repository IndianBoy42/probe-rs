# pyprobe-rs — full usage guide

Python bindings for the [probe-rs](https://github.com/probe-rs/probe-rs) embedded
debugging toolkit. This document is the complete reference; each section notes the
underlying mechanism being used — a probe-rs RPC endpoint on the managed
`probe-rs serve` process, a direct local probe-rs call, or a pure-Python helper.

- [Installation](#installation)
- [Execution model](#execution-model)
- [Server lifecycle](#server-lifecycle)
- [Probes](#probes)
- [Sessions and configuration](#sessions-and-configuration)
- [Core control](#core-control)
- [Memory access](#memory-access)
- [Core registers](#core-registers)
- [Breakpoints and vector catch](#breakpoints-and-vector-catch)
- [Core information](#core-information)
- [Debugging (DWARF, no DAP/GDB)](#debugging-dwarf-no-dapgdb)
- [Targets and the registry](#targets-and-the-registry)
- [Flashing](#flashing)
- [RTT](#rtt)
- [SWO / SWV tracing](#swo--swv-tracing)
- [Core dumps](#core-dumps)
- [Error handling](#error-handling)
- [Enums](#enums)
- [Testing without hardware](#testing-without-hardware)
- [Asyncio](#asyncio)
- [Python ↔ RPC API mapping](#python--rpc-api-mapping)

## Installation

Builds against the probe-rs workspace this crate lives in.

```bash
pip install maturin
cd pyprobe-rs
maturin develop --release      # into the active virtualenv
```

The `espressif` chip-support plugin is enabled by default
(`maturin develop --features espressif` is implied).

The installed package carries a matching `probe-rs` server binary (the RPC
server) embedded in the extension — no system install is needed. See
[Server lifecycle](#server-lifecycle) for the resolution order and the
`PYPROBE_RS_BIN` override.

## Execution model

pyprobe-rs is a thin RPC facade: every operation is a request to a managed
`probe-rs serve` process, and the Python kernel never holds the probe. Four
rules explain every surprising behavior:

1. **The server owns the hardware.** Probes are claimed, sessions are attached,
   and memory/RTT/flash work all happen inside `probe-rs serve`. Stale cells,
   kernel restarts, and leaked handles cannot block your hardware: a kernel
   that disappears just drops its connection, and the server releases the
   probe lease of an idle session.
2. **The GIL is released on every call.** Each blocking call (halt, memory
   access, flash, RTT poll, attach) runs on the bridge's RPC runtime with the
   interpreter detached, so a long flash no longer freezes interactive Python
   loops or Jupyter heartbeats. Progress and RTT callbacks are invoked from
   the RPC runtime back into Python; their exceptions are printed to stderr
   and never abort the operation.
3. **Sessions close explicitly, leases release automatically.** A session
   holds a probe lease on the server. `Session.close()` (idempotent, also
   available via the context manager) marks the session closed so later calls
   fail fast; `pr.close_all()` does this for every session of the process.
   The server releases the probe once the transport's last handle is dropped
   — so close your session, or let it go out of scope, to free the probe for
   the next client. Handles obtained from a session (`sess.core(i)`,
   `sess.attach_rtt(...)`, `rtt.up_channel(n)`) are plain
   (connection, index) pairs with no hardware state; they can be held and
   shared freely, and they keep the transport alive until collected.
4. **Convergence and self-heal.** `pr.open()` and `Session.auto_attach()` are
   idempotent — repeated calls converge on "server running, schema
   compatible" instead of erroring. If the probe is busy on a server that
   pyprobe-rs spawned and no live connection exists, the attach retries once
   after restarting that server (clearing a stale session left behind by a
   previous kernel). Foreign servers (spawned by another client, or a remote
   host) are never restarted; their `ProbeBusy` error names the fix.

`FakeProbe` is still consumed once (`into_probe()` / `Probe.from_fake()`),
but the resulting attach is now a **server-side dry run**: the server swaps in
its own fake probe and claims no device. The direct form is
`Session.auto_attach(chip, dry_run=True)` (see
[Testing without hardware](#testing-without-hardware)).

## Server lifecycle

`pr.open()` converges the module to "a compatible server is reachable":

- **No host configured (default):** spawn (or adopt) a private local
  `probe-rs serve` on a user-owned unix socket. The runtime dir
  `<$XDG_RUNTIME_DIR|$TMPDIR|/tmp>/pyprobe-rs-<uid>` holds a generated
  `.probe-rs.toml`, the `serve.sock` socket (permissions `0o660`), and a
  pidfile. The server runs with `HOME` pinned to that directory, so your own
  `~/.probe-rs.toml [server]` table cannot leak into the instance. A socket
  that is already connectable is adopted, whether it is ours from a previous
  kernel or another client's.
- **Host configured (`pr.open(host=..., token=...)`):** verify a remote
  server instead of spawning one. All later attaches go to that host until
  `pr.open()` is called again without arguments.

The binary is resolved in this order: the `PYPROBE_RS_BIN` environment
variable (ignored if it does not point at a file) → the server binary
embedded in the package (a stripped workspace build, materialized once into
the per-user runtime directory) → this workspace's
`target/{server,release,debug}/probe-rs`. `PATH` is deliberately never
consulted: the RPC protocol is version-unstable by design, so a foreign
binary can pass a schema handshake yet behave differently. A stale binary
from an older build is caught by the RPC schema handshake; on a server
pyprobe-rs spawned, it is restarted once to pick up the current one.

Lifecycle handles:

```python
pr.sessions()          # describe the live sessions this process holds
pr.close_all()         # close them all (later calls on them fail fast)
pr.server_logs()       # ring buffer (last ~200 lines) of our spawned server
pr.shutdown_server()   # stop the server we spawned; refused for adopted
                       # servers and in remote mode
```

Transports:

| Transport | How | Notes |
|---|---|---|
| private unix socket (default) | auto-spawned/adopted `probe-rs serve` | the managed path; per-user runtime dir |
| `socket:///path/to/serve.sock` | adopt a unix-socket server elsewhere on this machine | e.g. a server started by hand or by another user |
| `ws://host:port`, `wss://host:port` | remote `probe-rs serve` with a TCP listener | token via `pr.open(host=..., token=...)`; upstream `wss://` does not verify TLS certificates yet — prefer `ssh://` or trusted networks |
| `ssh://[user@]host[:port]` | reach a remote server over an SSH tunnel | same API afterwards |

Server side, a TCP listener is configured in the server's config file:

```toml
[server]
address = "127.0.0.1"
port = 3000

[[server.users]]        # optional: require a token
name = "pyprobe"
token = "..."
```

Remote status (verified against this workspace's dev build, 2026-09-09):
remote transports are architecturally supported, but the upstream websocket
transport's schema handshake is currently unreliable — `probe-rs` itself
fails its own compatibility check against its own `serve` over `ws://` and
then hangs. pyprobe-rs surfaces that as a clean `ServerVersion` error (our
deadlines prevent the upstream hang). Working paths today: the local
`socket://` transport, and remote servers reached as adopted socket servers
(`ssh://` or `socket://`). Re-check upstream `probe-rs` before relying on
`ws://`.

Spawning requires Unix; the managed-server path is Unix-only end to end.

Stale-cell healing, summarized: a `ProbeBusy` on our own idle spawned server
or a schema mismatch on it triggers exactly one restart + retry; a dead
child's socket is cleaned up and respawned on the next convergence; a
readiness failure (socket never opens within ~10 s) raises a `Connection`
error and suggests `pr.server_logs()`.

## Probes

Probe enumeration and the probe-level operations (`vid`, speed, protocol,
reset lines) use the [probe_rs::probe](https://docs.rs/probe-rs/latest/probe_rs/probe/)
API directly; attaching routes through the server.

```python
import pyprobe_rs as pr

probes = pr.list_probes()                  # -> list[ProbeInfo]
for p in probes:
    print(p.vid, p.pid, p.serial, p.identifier, p.probe_type, p.inaccessible)

probe = probes[0].open()                   # local handle, or a server-side selector
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

`pr.list_probes()` uses the server's view when a server is active (remote
mode configured, or a server is already running): those entries carry
`probe_type` (e.g. `"cmsisdap"`, `"jlink"`) and `inaccessible` markers, and
`ProbeInfo.open()` returns a `Probe` bound to that listing — it has no local
handle, so probe-level calls (`set_speed`, `get_name`, …) raise, but
`attach()` and `attach_under_reset()` route through the server and select it.
With no server active, probes are listed locally and `open()` returns a real
local handle; those entries always report `probe_type == ""` and
`inaccessible == False`. Listing probes never starts a server.

`get_name()` returns the debug-formatted descriptor of the underlying probe —
a long USB dump on CMSIS-DAP probes. Use `ProbeInfo.identifier` for a short
name. Probes without voltage sensing return `None` from `get_target_voltage()`.

Attach (through the server, preferring this probe):

```python
sess = probe.attach("nrf52840_xxaa", pr.Permissions())
sess = probe.attach_under_reset("nrf52840_xxaa", pr.Permissions().allow_erase_all())
```

`attach()` releases the local USB handle first so the server can claim the
device; the `Probe` object itself stays usable — if attach fails, just call
`attach()` again (the probe's selector is remembered). Probe-level methods
(`set_speed`, `target_reset`, …) raise `ValueError: Probe consumed` once the
local handle was released. A `set_speed()`/`select_protocol()` value applied
**before** `attach()` is forwarded into the attach request, so the server
opens the probe with the same settings.

Errors are classified (see [Error handling](#error-handling)): `ProbeBusy`
(the probe is claimed by another session on the server — close that session
or pass `wait_for_probe=` to `Session.auto_attach()`), `Probe` (no
accessible probe found), `TargetAttachFailed` (the target refused attach;
retrying with `connect_under_reset=True` may succeed).

## Sessions and configuration

Sessions are RPC sessions on the managed server.

```python
sess = pr.Session.auto_attach("nrf52840_xxaa")   # picks the first accessible probe

cfg = pr.SessionConfig(
    speed=1000,                       # kHz
    protocol=pr.WireProtocol.Swd,
    permissions=pr.Permissions(),
)
sess = pr.Session.auto_attach("nrf52840_xxaa", cfg, wait_for_probe=30)
# wait_for_probe: seconds to queue for the probe lease instead of failing
# immediately while another client holds it.

perms = pr.Permissions()
perms.allow_erase_all()               # needed for chip erase / unlock operations

with pr.Session.auto_attach("nrf52840_xxaa") as sess:
    ...                               # closes the session on exit

sess.close()                          # idempotent; later calls fail fast
```
`close()` (explicit, via `with`, or via `pr.close_all()`) makes every later
call on the session and its handles raise immediately with kind `Connection`
instead of hitting a dead server, and releases the probe lease right away —
even while `CoreHandle`/RTT objects are still referenced. `close()` itself
never blocks, so it is safe to call from RTT/flash progress callbacks; when
an operation is in flight, the lease releases as soon as that operation ends
(its own deadline bounds the wait).

```python
sess.list_cores()                     # -> [(0, CoreType.Armv7em), ...]
sess.target()                         # -> Target (see below)
sess.architecture()                   # -> Architecture.Arm / Riscv / Xtensa

sess.resume_all_cores()
sess.clear_all_hw_breakpoints()       # across all cores

core = sess.core(0)                   # IndexError beyond the last core
```

`Target` (read-only, resolved from the local builtin registry by the
session's target name):

```python
t = sess.target()
t.name                # str
t.architecture        # Architecture enum
t.cores               # [(core_name: str, CoreType)]
t.flash_algorithms    # [str]
t.memory_map          # [("ram"|"nvm"|"generic", start: int, end: int)]
```

## Core control

`CoreHandle` is a (connection, core index) pair: every call is one RPC with
the GIL released, and handles can be shared across threads/cells freely.

```python
core = sess.core(0)

pc = core.halt(1000)                  # halt with timeout (ms) -> PC after halt
core.run()                            # resume
core.reset()                          # system reset, keep running
pc = core.reset_and_halt(1000)        # reset and halt -> PC
pc = core.step()                      # single instruction -> PC
pc = core.step_over()                 # statement-level step (needs debug info)
pc = core.step_into()                 # statement-level step (needs debug info)
pc = core.step_out()                  # step out of the current frame (needs debug info)

core.wait_for_core_halted(2000)       # polls `core/status` every 50 ms; raises on timeout
core.core_halted()                    # -> bool

st = core.status()                    # -> CoreStatus (see Enums)
st.state              # CoreState.Running / Halted / LockedUp / Sleeping / Unknown
st.is_halted, st.is_running
st.halt_reason        # HaltReason | None
    # .kind  -> HaltKind.Breakpoint / Watchpoint / Step / Request / External /
    #            Exception / Multiple / Unknown
    # .detail -> str (e.g. "hardware", "software", "semihosting: ...")
```

Two facade-side guarantees on top of the raw endpoints: `reset()` resumes
the core even when the reset was issued while halted (the Cortex-M debug
halt request survives a system reset; the server leaves the core stopped in
that case), and the tracked hardware-breakpoint set is re-applied after
`reset()`, `reset_and_halt()`, and `flash()` — a system reset clears the
FP comparators, and the set is the debugger's intent. Clear it explicitly
with `clear_hw_breakpoint`/`clear_all_hw_breakpoints` when you want it gone.

`step_over` / `step_into` / `step_out` are server-side, DWARF-based
statement stepping — load debug info first (see
[Debugging](#debugging-dwarf-no-dapgdb)); without it the server answers with
a warning (printed to stderr) and the PC after the step.

Run/halt transitions on a freely running target can sporadically raise an
ARM-specific error on some probes (seen with CMSIS-DAP V2). The reliable pattern
is `halt()` → do the work → `run()`, polling `core_halted()` in a loop rather
than relying on one blocking `wait_for_core_halted()` call.

## Memory access

Addresses are raw bus addresses; every access is one RPC request.

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

data = core.read(0x2000_0000, 128)     # bulk byte read -> bytes
core.write(0x2000_0000, b"\x00" * 16)  # byte write
```

`core.read(addr, count)` returns **as many bytes as are readable** from
`addr`, stopping at the first unreadable region — the result may be shorter
than `count`.

Width matters for speed: byte and 16-bit transactions move a fraction of the
data per probe round-trip (and now also per RPC round-trip). Dumping 128 KiB
of RAM took 1.7 s with `read_32` vs 6.7 s with `read_8` (Cortex-M7,
CMSIS-DAP V2). Prefer the widest width your alignment allows, and batch with
the block methods instead of `read_word_*` loops.

## Core registers

Registers are addressed by name (case-insensitive) or numeric id. The RPC
wire carries bare register ids with no name table, so name → id resolution
uses the same probe-rs register file the server reads from (selected from the
core's instruction-set and FPU metadata): Cortex-M, RISC-V (32/64), AArch32,
AArch64 and Xtensa all resolve, together with each architecture's ABI aliases.

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

- Unknown names raise `pr.ProbeRsError` with kind `Register`; the message
  lists all known names.
- Writes are validated against the register's width: a value that does not
  fit raises `OverflowError` (nothing is silently truncated).

Convenience accessors (return `int`):

```python
core.read_pc()
core.read_sp()
core.read_fp()
core.read_return_address()
```

Enumerate registers (a snapshot of the probe-rs register file for the core's
architecture; floating-point registers are listed only when the core has an
FPU):

```python
for reg in core.registers():           # -> [CoreRegisterInfo]
    print(reg.name, reg.id, reg.size_in_bits)
```

Names are lowercased, and the arch-neutral `pc`/`sp`/`lr`/`fp` aliases are
appended when the architecture names those registers differently (Cortex-M
lists both `r15` and `pc`, for example).

## Breakpoints and vector catch

Hardware breakpoints set through pyprobe-rs are **tracked by the facade** and
re-applied automatically after a `sess.flash(...)` — a reset clears the debug
unit, but the session (and the tracked set) stays alive.

```python
core.set_hw_breakpoint(0x0800_1234)
core.set_hw_breakpoints([0x0800_1200, 0x0800_1234])
# -> [(address, error)] per address; error is None on success, and failed
#    addresses are not tracked. There is no unit-selection API.
core.clear_hw_breakpoint(0x0800_1234)
core.clear_hw_breakpoints([0x0800_1200, 0x0800_1234])
core.clear_all_hw_breakpoints()        # everything this facade set on the core
sess.clear_all_hw_breakpoints()        # ... across all cores

core.enable_vector_catch(pr.VectorCatchCondition.HardFault)
```

Gaps on the RPC surface:

- `disable_vector_catch()` raises: the server has no vector-catch disable
  endpoint; the catch clears on core reset.
- `available_breakpoint_units()` and `set_hw_breakpoint_unit()` are gone —
  the RPC protocol exposes neither.

## Core information

Metadata comes from the server's target/core metadata endpoints.

```python
core.id()                       # -> int, core index within the session
core.architecture()             # Architecture
core.core_type()                # CoreType
core.instruction_set()          # InstructionSet
core.fpu_support()              # bool
core.is_64_bit()                # bool
core.floating_point_register_count()  # int; raises when the core has no FPU
core.memory_regions()           # [(kind, start, end)] — same shape as Target.memory_map
```

`core.endianness()` raises: byte order is not transported over the RPC wire,
and the facade refuses to guess on your behalf.

## Debugging (DWARF, no DAP/GDB)

The RPC surface carries full server-side DWARF support: stack traces, scopes,
variables, expression evaluation, source-breakpoint resolution, SVD peripheral
names, and statement-level stepping — no debug adapter, no GDB server.

```python
sess.load_debug_info("target/thumbv7em-none-eabihf/release/app")

# 1. Where is execution, in source terms?
for f in sess.stack_trace(0):                     # core=0, limit=20 frames
    print(f["function"], hex(f["pc"]), f["file"], f["line"],
          "(inlined)" if f["inlined"] else "")
# frames are dicts: function, pc, inlined, file, line, column, id

# 2. Inspect state at the top frame.
scopes = sess.scopes(0, frame=0)      # core, frame index
# -> [{"name": "Local", "variables_reference": 4, "line": 12, "column": 8}, ...]
local = sess.variables(scopes[0]["variables_reference"])
# -> [{"name", "value", "type", "variables_reference"}]
# expand a struct/pointer by passing its variables_reference back in:
member = sess.variables(local[0]["variables_reference"])

# 3. Evaluate a DWARF expression against the loaded debug info.
v = sess.evaluate("&config.clock_rate")           # core=0, frame=None
# -> {"result", "type", "variables_reference", "memory_reference"}

# 4. Peripheral register names from a CMSIS-SVD file (uploaded to the server).
sess.load_svd("nrf52840.svd")                     # core=0

# 5. Plan source breakpoints, then set them as hardware breakpoints.
bps = sess.resolve_source_breakpoints("src/main.rs", [12, 40])
# -> [{"address", "file", "line"}], or {"address": None, "error": str}
for bp in bps:
    if bp.get("address") is not None:
        sess.core(0).set_hw_breakpoint(bp["address"])

# 6. Step in source terms (server-side DWARF).
pc = sess.core(0).step_over()                     # also step_into / step_out
# core.step() remains single-instruction.

# 7. Disassemble; the DWARF-derived fields need load_debug_info first.
for ins in sess.core(0).disassemble(0x1000, 32):
    print(hex(ins["address"]), ins["instruction"],
          ins["location"] and ins["location"]["name"])
# instruction dicts: address, instruction, instruction_bytes, line, column,
#                    location ({name, path} | None)
```

Contract notes:

- `load_debug_info(path)` publishes the ELF to the server once; on a remote
  server the file travels the upload path automatically. Re-run it after every
  reflash with the new image.
- A reflash changes code and invalidates server-side frame/variable handles:
  call `sess.clear_debug_state(0)` (per core) after re-attaching the new
  image's debug info.
- `resolve_source_breakpoints` only *maps* lines to addresses — setting the
  breakpoint is still `core.set_hw_breakpoint(...)` (step 5 above).
- `stack_trace` requires the core to be halted; it returns the frames of the
  requested core flattened into one list (inlined frames included, marked
  `inlined=True`).

## Targets and the registry

These are pure local registry calls (no RPC):

```python
names = pr.list_targets()                       # all builtin chip names
target = pr.get_target_by_name("nrf52840_xxaa") # convenience lookup

reg = pr.Registry.from_builtin_families()
reg.families()                                  # -> ["nRF52 Family", ...]
reg.search_chips("nRF52840")                    # -> [str]
reg.get_target_by_name("nrf52840_xxaa")         # -> Target
```

## Flashing

The iteration primitive is `sess.flash(path, *, run=True, verify=False,
progress=None)` — one call does resolve → build → (verify) → flash → boot →
re-apply tracked hardware breakpoints:

```python
sess.flash("target/thumbv7em-none-eabihf/release/app")   # run=True, verify=False
sess.flash("fw.elf", verify=True)     # pre-verify + read-back after programming
sess.flash("fw.bin", run=False)       # boot but leave all cores halted
```

- The image format is resolved server-side from the target metadata, so any
  supported format works here: `elf`, `hex` / `ihex` / `intelhex`, `bin` /
  `binary`, `uf2`, `idf` / `esp-idf` / `espidf` (case-insensitive).
- `verify=False` (the default) is the fast iteration path: no pre-check, no
  read-back. `verify=True` does both halves: the image is pre-checked against
  flash (an unchanged image **skips programming entirely**), and a program
  that runs is read back afterwards.
- `run=True` resumes all cores after boot; `run=False` leaves them halted.
- Tracked hardware breakpoints are re-applied after boot (a reset clears the
  debug unit while the session stays alive). A failed re-apply is logged to
  stderr, not raised — the flash itself succeeded.

Full control (Rust `flashing::DownloadOptions` semantics) via
`pr.download_file`:

```python
opts = pr.DownloadOptions()        # defaults: verify=True, keep_unwritten_bytes=True;
                                   # everything else False, ram_chunk_size=None
opts.do_chip_erase = True          # erase the whole chip, not just used sectors
opts.skip_erase = True             # no erase pass at all (pre-erased targets)
opts.preverify = True              # skip programming when the image already matches flash
opts.disable_double_buffering = True
opts.ram_chunk_size = 4096         # None = upstream default chunking

pr.download_file(sess, "fw.elf", "elf", opts)
```

Two options changed meaning under the RPC facade:

- `opts.dry_run` is no longer a per-download switch — the server derives
  dry-run behavior from the *session* (attach with
  `Session.auto_attach(..., dry_run=True)`). Setting it now raises
  `ValueError` instead of being silently ignored on a destructive
  operation. Dry-run sessions also skip the boot step (the mocked core
  cannot run a boot sequence).
- `opts.preverify` is implemented client-side: the image is verified first
  and the write is skipped when it already matches flash.

Formats and raw-image positioning (rejected for all formats except `bin`):

```python
pr.download_file(sess, "fw.bin", "bin", base_address=0x1000_0000)
pr.download_file(sess, "fw.bin", "bin", base_address=0x1000_0000, skip=512)
```

`download_file` always boots the image afterwards (cores resume) and
re-applies tracked breakpoints.

Progress callback — receives `ProgressEvent` objects:

```python
def on_progress(ev):
    # ev.kind: FlashLayoutReady | AddProgressBar | Started | Progress |
    #           Finished | Failed | DiagnosticMessage
    # ev.operation: str ("Fill" | "Erase" | "Program" | "Verify" | "Ram",
    #                    or the diagnostic message text)
    # ev.size: int, ev.total: int | None, ev.time_s: float (always 0 — the
    #          wire carries no timestamps)
    print(ev.kind, ev.operation, ev.size)

sess.flash("fw.hex", progress=on_progress)
pr.download_file(sess, "fw.hex", "hex", progress=on_progress)
```

Callback contract:

- events are forwarded to a dedicated callback thread (never the flash
  future's poll context), so the callback **may** call `Session`/
  `CoreHandle`/RTT APIs — halting, reading memory, or writing RTT down
  channels from inside the callback all work;
- it cannot abort the flash (the RPC progress stream is one-way);
- exceptions it raises are printed to stderr, never propagated or swallowed
  silently;
- during interpreter shutdown callbacks are dropped instead of touching a
  finalizing interpreter.

Staged writes (server-side `FlashLoader`):

```python
loader = pr.build_loader(sess, "fw.bin", "bin", base_address=0x1000_0000)
loader.add_data(0x1000_2000, b"\xde\xad\xbe\xef")   # patches, staged at commit
loader.commit(sess)                                  # flash + boot (cores resume)
loader.commit(sess, opts, progress=on_progress)
```

The image is staged server-side by `build_loader`; `add_data` patches
accumulate client-side and are staged on the server at `commit`.

Chip erase without an image (requires `Permissions.allow_erase_all()`):

```python
sess.erase_all()                       # vendor erase-all sequence when the
                                       # target provides one (the server
                                       # decides), otherwise a full chip erase
sess.sequence_erase_all()              # alias of erase_all()
```

`sess.has_sequence_erase_all()` is gone — whether a vendor sequence exists is
a server-side decision now.

## RTT

RTT runs as a server-side RTT client inside `probe-rs serve`; every
read/write is one RPC request with the GIL released.

```python
rtt = sess.attach_rtt(0)               # scan RAM for the RTT control block
rtt = sess.attach_rtt(0, ptr=0x2000_0100)   # or attach at a known control block
rtt = sess.attach_rtt(0, modes={0: pr.ChannelMode.BlockIfFull})  # attach-time up-channel modes

rtt.up_channels()                      # -> [ChannelInfo(number, name)]
rtt.down_channels()                    # ChannelInfo: attach-time snapshot

up = rtt.up_channel(0)
up.name()                              # channel name (from the snapshot)
data = up.read()                       # direct poll; up to 4096 bytes requested
text = up.read_str()                   # lossy UTF-8 of the same read

rtt.start_streaming()                  # background poller, 20 ms tick
rtt.start_streaming(0.01, callback=lambda ch, data: print(data.decode(errors="replace"), end=""))
# ... other cells run; nothing is lost (up to 1 MiB per channel) ...
chunks = rtt.up_channel(0).pop(4096)        # drain what arrived meanwhile
data = rtt.up_channel(0).pop(1024, 0.5)     # wait up to 0.5 s for data
rtt.stop_streaming()                        # rings keep their buffered data

down = rtt.down_channel(0)
n = down.write(b"hi\n")                # single non-blocking attempt; check n
n = down.write_str("hi\n")             # same, from str
```

Streaming semantics:

- `start_streaming(interval_secs=0.02, callback=None)` runs a poller task on
  the bridge runtime: one poll request per tick covering all up channels,
  pushing bytes into per-channel **1 MiB drop-oldest ring buffers**, which
  `up.pop()` drains. It is idempotent while running; `interval_secs` must be
  positive. The poller exits on `stop_streaming()`, when the `Rtt` object is
  dropped, or when the connection dies — a crashed client can never leave a
  polling loop stranded on the server.
- **Loss semantics:** the ring is a lossy tap by design. If your consumer is
  slower than the target, the *oldest* bytes are dropped once 1 MiB
  accumulates; the poller never blocks on a slow consumer. `pop()` returns up
  to `max_bytes` of the oldest buffered bytes; with `timeout_secs > 0` it
  waits (GIL released throughout) — an empty result means the ring had
  nothing within the deadline. `pop()` works even if streaming never started
  (the ring always exists; it just stays empty).
- `callback`, when given, is called as `callback(channel_number, bytes)` per
  chunk on a dedicated callback thread (never the poller's runtime
  context), so it **may call back into pyprobe_rs** — writing a down
  channel, halting, or flashing from inside the callback all work. Its
  exceptions are printed to stderr and never stop the stream. After
  `stop_streaming()`/`close()` no *new* callbacks fire; a chunk already in
  flight when you stopped may still be delivered.
- while a flash/reset owns the session, the poller skips its ticks (the
  target is not readable mid-operation anyway); bytes produced during that
  window follow the target's own RTT buffer mode (lost under `SkipIfFull`,
  buffered under `BlockIfFull`) and the ring catches up at the next tick
  after the operation ends. Skipping instead of waiting is what keeps the
  poller from stalling the session's wire;
- `up.read()` is a direct RPC poll: the server consumes the target's ring
  buffer, so **everything pending is returned even beyond `count`** —
  truncating would lose the remainder (already consumed server-side). It
  races with a running background poller: use **one consumer per channel**.
- Attaching before the target initialized RTT is fine: the control-block
  attach happens server-side and is retried by every later poll. The channel
  *list* is still an attach-time snapshot — if the target re-initializes RTT
  with a different channel layout, call `sess.attach_rtt(...)` again to
  rescan. (On multi-core targets the `core_index` argument is advisory: the
  default RAM scan covers core 0, and a `ptr` pins the core owning it.)

Gaps on the RPC surface (all raise `pr.ProbeRsError`):

- `up.set_mode()`: the wire has no live mode endpoint. Set the mode at attach
  time with `modes={channel_number: pr.ChannelMode.BlockIfFull}`; channels
  without an entry keep the target's own buffering.
- `up.mode()`: returns the mode configured at attach time; raises for a
  channel left at the target's own mode (the wire cannot read it live).
- `rtt.ptr()`: returns the address only when `attach_rtt(ptr=...)` pinned it;
  the wire does not report the address found by a RAM scan.
- `up.buffer_size()` / `down.buffer_size()` / `ChannelInfo.buffer_size`:
  the wire does not report target-side buffer sizes.

`down.write()` makes a single non-blocking attempt (the server's retry window
is disabled): depending on the channel's mode, a full target buffer yields `0`
or a partial write — check the returned count.

A target can host **several RTT control blocks** (bootloader + app, or a
transport library instantiating its own), and the scan returns the first match
in RAM — not necessarily the block your console uses. The magic also survives
warm resets (SEGGER's init-once idiom), so a stale block still looks live.
Wrong-block symptoms: down-channel writes get no reaction and up-channel data
never advances while the target is clearly logging elsewhere. Fingerprint the
live console by its channel names — an abandoned block decays into garbage
names (fragments of unrelated strings) instead of e.g. `"Terminal"` — or
attach explicitly with `sess.attach_rtt(0, ptr=...)`.

## SWO / SWV tracing

Not available. `setup_tracing`, `disable_swv`, `add_swv_data_trace`,
`remove_swv_data_trace`, `read_swo`, and `read_trace_data` all raise
`pr.ProbeRsError` with kind `NotImplemented`:

> SWO/SWV tracing is not available over the RPC interface; it needs a direct
> probe session (planned once probe-rs adds tracing endpoints).

The probe-rs RPC protocol has no tracing endpoints, and the direct-probe code
path was removed with the RPC cutover — the facade refuses to fake one. The
`TraceSink` enum still exists for API compatibility.

## Core dumps

The dump is taken server-side (`core/dump`) and reconstructed into a local
probe-rs `CoreDump`; `store`/`load` use the ELF coredump file format.

```python
dump = pr.CoreDump.capture(sess, 0, [
    (0x2000_0000, 0x2001_0000),    # (start, end) — inclusive-exclusive RAM ranges
])                                  # halted core: registers + memory
dump.store("crash.pcd")

dump = pr.CoreDump.load("crash.pcd")
dump.registers                     # {register_id: value} (ints, full width)
dump.data                          # [(start_address, bytes), ...]
```

## Error handling

Every failure raises `pr.ProbeRsError` (a subclass of `Exception`) with a
`.kind` attribute for programmatic branching, and a message that states the
one-line fix:

```python
try:
    rtt = sess.attach_rtt(0)
except pr.ProbeRsError as e:
    print(e.kind)      # "Rtt"
    print(e)           # diagnostic + the one-line fix
```

Bridge and lifecycle kinds:

| kind | meaning | fix named in the message |
|---|---|---|
| `Connection` | server unreachable, or this session was closed | `pr.open()` to start/verify the server (or fix the host URL); re-attach after a close |
| `ServerVersion` | server speaks a different RPC schema | use the matching probe-rs version for client and server (`PYPROBE_RS_BIN`, or update the server) |
| `ServerGone` | the connection was lost mid-call (server crash/panic) | `pr.open()` and attach again — restarting the server releases its probe |
| `ProbeBusy` | the probe is claimed by another session on this server | close that session (`pr.close_all()` closes ours, `pr.shutdown_server()` restarts one we spawned), or queue with `wait_for_probe=` |
| `TargetAttachFailed` | the target refused attach (may hint `connect_under_reset`) | retry with `connect_under_reset=True` where suggested |
| `Probe` | no accessible probe found, or the probe could not be opened | connect one and check `pr.list_probes()` |
| `Timeout` | a call exceeded its bridge deadline | pass a larger timeout or check that the target responds |
| `Rpc` | a server-reported failure not otherwise classified | per-message |
| `NotImplemented` | documented RPC gap (SWO/SWV, RTT mode/ptr/buffer size, vector-catch disable) | per-message |

Kinds forwarded from probe-rs error variants: `Timeout`, `Probe`, `Arm`,
`Riscv`, `Xtensa`, `ChipNotFound`, `MissingPermissions` (operation needs
`Permissions.allow_erase_all()`), `Core`, `Register`, `Memory`, `Breakpoint`,
`Rtt`. Flashing and dump failures surface as their error type names:
`FlashError`, `FileDownloadError`, `CoreDumpError`, `ArmError`,
`DebugProbeError`, …

Server-side panics (e.g. the mocked core's unsupported 16-bit access) drop
the connection: you see `ServerGone` with a fix message, and a fresh attach
recovers.

Python-side argument mistakes still raise builtin exceptions
(`ValueError`, `TypeError`, `IndexError`, `OverflowError`).

## Unexpected target resets and power cycles

MCUs reset when you don't ask them to (watchdogs, brownouts, reset buttons,
reflashes from another tool). Sessions are built to survive that:

- **RTT keeps streaming.** The server re-attaches the RTT control block on
  every poll, so a firmware re-initialization heals on its own. The
  background poller surfaces what is happening instead of going silent: it
  warns when a channel starts (or stops) failing, and after ~500 ms of
  silence it probes the channel list — warning once if a subscribed channel
  number no longer exists (the target rebooted into a different build), in
  which case re-attach via `sess.attach_rtt()` to pick up the new set.
- **Breakpoints survive resets.** Hardware breakpoints are re-applied after
  `sess.flash()`, `core.reset()`, and `core.reset_and_halt()` (a
  SYSRESETREQ clears the FP comparators; the debug domain keeps them).
- **Dead links self-heal once.** If the target loses power or the probe is
  replugged mid-session ("No target responded", USB errors), the next
  operation rebuilds the transport, re-attaches, restores DWARF debug info
  and tracked breakpoints, and retries the operation — once per session.
  You'll see `pyprobe-rs: probe link lost during <op>; reattached and
  retried` on stderr. Later dead-link failures surface unchanged: a
  persistently broken rig must not be masked. The failure comes back as
  `kind == "Probe"`; recovery from there is `sess.close()` + a fresh
  `Session.auto_attach(...)`.
- **State changes are visible, not fatal.** A reset while halted leaves the
  core running: `core.status()` reports it, `step()` raises `kind == "Core"`
  ("has to be halted"), and `wait_for_core_halted(timeout_ms=…)` is
  deadline-bounded. A power reset can also drop the debug domain
  (`kind == "Core"`, "is not enabled") — that one is not auto-recoverable;
  close and re-attach (with `connect_under_reset=True` if the target locks
  up).

Error-string classification for server-side failures is best-effort (the RPC
wire carries no structured error kind): `Timeout`, `Probe`, `Rtt`, `Core`,
`Memory` kinds are matched from the probe-rs error text, everything else
stays `Rpc`.

## Enums

All enums live at module level and support `==` comparison:

| Python | Values | Source |
|---|---|---|
| `WireProtocol` | `Swd`, `Jtag` | `probe_rs::probe::WireProtocol` |
| `Architecture` | `Arm`, `Riscv`, `Xtensa` | `probe_rs_target::Architecture` |
| `CoreType` | `Armv6m`, `Armv7a`, `Armv7r`, `Armv7m`, `Armv7em`, `Armv8a`, `Armv8m`, `Riscv`, `Riscv64`, `Xtensa` | `probe_rs_target::CoreType` |
| `InstructionSet` | `Thumb2`, `A32`, `A64`, `RV32`, `RV32C`, `RV64`, `RV64C`, `Xtensa` | `probe_rs_target::InstructionSet` |
| `Endian` | `Little`, `Big` | `probe_rs_target::Endian` (unused: `core.endianness()` raises) |
| `ChannelMode` | `NoBlockSkip`, `NoBlockTrim`, `BlockIfFull` | `rtt::ChannelMode` (attach-time only) |
| `VectorCatchCondition` | `HardFault`, `CoreReset`, `SecureFault`, `All`, `Svc`, `Hlt` | `VectorCatchCondition` (enable only) |
| `CoreState` | `Running`, `Halted`, `LockedUp`, `Sleeping`, `Unknown` | `CoreStatus` |
| `HaltKind` | `Breakpoint`, `Watchpoint`, `Step`, `Request`, `External`, `Exception`, `Multiple`, `Unknown` | `HaltReason` |
| `TraceSink` | `Swo`, `Tpiu`, `TraceMemory` | accepted by `setup_tracing`, which then raises |

## Testing without hardware

Two equivalent dry-run paths — the server swaps in its own fake probe and
claims no device or probe lease:

```python
# explicit FakeProbe (consumes the FakeProbe):
fp = pr.FakeProbe.with_mocked_core()            # or pr.FakeProbe.new()
sess = pr.Probe.from_fake(fp).attach("Cortex-M4", pr.Permissions())

# direct dry-run attach:
sess = pr.Session.auto_attach("Cortex-M4", dry_run=True)

core = sess.core(0)
core.halt(1000)
core.write_word_32(0x2000_0000, 0xDEADBEEF)
```

What the mock covers: control flow, 8/32/64-bit memory access, registers,
error paths. What it does not: flash or RTT contents (there is no RTT control
block, so `attach_rtt` fails cleanly), and **16-bit accesses panic inside the
mock** — the server drops the connection and you get `ServerGone`; a fresh
attach recovers. `reset`/`reset_and_halt` support is state-dependent on the
mock; failures are clean classified errors, never dead connections.

Two pytest suites cover the no-hardware surface:

- `tests/test_smoke.py` — binding smoke: targets, registry, dry-run attach,
  memory/register round trips, options, coredump round trip.
- `tests/test_rpc_lifecycle.py` — spawns a **real `probe-rs serve`** and
  drives dry-run sessions through the lifecycle: idempotent `open()`,
  `close()`/`close_all()` fail-fast, `ServerGone` recovery, RTT and debug
  surface shapes, and the asyncio wrapper.

```bash
uv run pytest tests/        # no hardware needed; uses the embedded server when present
```

```bash
# hardware required; every wait is deadline-bounded
PYPROBE_RS_PY_HIL_CONFIG=/path/to/dut.toml uv run pytest tests/test_hil.py -m hil

# add the soak loop (minutes; long — run it on its own)
PYPROBE_RS_PY_HIL_CONFIG=/path/to/dut.toml PYPROBE_RS_PY_SOAK_MIN=30 \
  uv run pytest tests/test_hil.py -m hil -k soak
```

### Flash wear on the DUT

The default suite programs the same test image repeatedly, and
`Session.flash` skips erase/program entirely when the image already matches
the flash content (unchanged-image shortcut), so repeat runs cost the DUT
~zero erase cycles. `test_flash_wall_time_wear` is the one deliberate
exception: it disables the shortcut to measure verify on/off wall time, and
every run erases the sector(s) the image touches. It is skipped unless you
ask for it:

```bash
PYPROBE_RS_PY_FLASH_WEAR=1 PYPROBE_RS_PY_HIL_CONFIG=/path/to/dut.toml \
  uv run pytest tests/test_hil.py -m "hil and wear"
```

Wear accounting for an STM32H7-class part (128 KiB sectors, 10 K cycles
rated): one fresh-image suite ≈ 1 sector erase; each `wear` invocation ≈ 3;
the soak's periodic reflashes hit the shortcut and cost none.

## Asyncio

Every blocking call releases the GIL, so any facade object can be mirrored as
an asyncio view: `pr.aio(obj)` returns a wrapper whose method calls become
awaitables running in the default executor (the underlying call keeps the
event loop free). `obj.aio` works as a property on `Session`, `CoreHandle`,
`Rtt`, `UpChannel`, and `DownChannel`; non-callable attributes pass through.

```python
await pr.aio(sess).flash("app.elf")
a, b = await asyncio.gather(
    sess.core(0).aio.read_word_32(0x2000_0000),
    sess.core(0).aio.read_word_32(0x2000_0004),
)
```

## Python ↔ RPC API mapping

Most facade calls are one RPC endpoint on the managed `probe-rs serve`.
Endpoints marked *direct* are local probe-rs calls that bypass the server;
*client-side* items live in the pyprobe-rs process.

| Python | probe-rs RPC endpoint / source |
|---|---|
| `pr.open()` / managed server | spawns or adopts `probe-rs serve` on a private unix socket; schema handshake at connect |
| `pr.sessions()` / `pr.close_all()` | client-side session registry (no RPC) |
| `pr.shutdown_server()` / `pr.server_logs()` | client-side process management (no RPC) |
| `pr.list_probes()` | `probe/list` (server view) or direct `Lister::list_all()` when no server is active |
| `pr.Lister().open(sel)` / `ProbeInfo.open()` | direct `Lister::open(DebugProbeSelector)` / `DebugProbeInfo::open()` |
| `Probe.attach()` / `.attach_under_reset()` | `probe/attach` (prefers that probe's selector; local USB handle released first) |
| `pr.Session.auto_attach(..., dry_run=True)` | `probe/attach` with `dry_run` (server swaps in a fake probe) |
| `pr.Session.auto_attach(..., wait_for_probe=s)` | `probe/attach` with `wait_for_probe` (probe lease queue) |
| `sess.core(i)` / `sess.list_cores()` | `target/metadata` (attach-time metadata; index checked client-side) |
| `sess.target()` / `sess.architecture()` | direct local `config::Registry` lookup by metadata target name |
| `sess.resume_all_cores()` | `cores/resume` (all cores) |
| `core.halt(ms)` / `.run()` / `.status()` | `core/halt` / `core/run` / `core/status` |
| `core.reset()` / `.reset_and_halt(ms)` | `reset` / `reset_and_halt` |
| `core.step()` / `.step_over()` / `.step_into()` / `.step_out()` | `core/step` (stepping mode; statement modes are server-side DWARF) |
| `core.wait_for_core_halted(ms)` / `.core_halted()` | `core/status` polled in a client-side 50 ms loop |
| `core.read_word_8/16/32/64(...)` | `memory/read8` … `memory/read64` (count 1) |
| `core.write_word_8/16/32/64(...)` | `memory/write8` … `memory/write64` |
| `core.read_8/16/32/64`, `.write_8/16/32/64` | same `memory/read*` / `memory/write*` endpoints with count/data |
| `core.read(addr, n)` | `memory/read_bytes` (stops at the first unreadable region) |
| `core.write(addr, data)` | `memory/write8` |
| `core.read_core_reg` / `.write_core_reg` | `core/read_registers` / `core/write_reg` (ids from built-in name tables) |
| `core.read_pc()` / `.read_sp()` / `.read_fp()` / `.read_return_address()` | `core/read_registers` with architecture-resolved role ids |
| `core.registers()` | built-in Cortex-M/RISC-V name tables (the wire carries no register names) |
| `core.set_hw_breakpoint()` / `.set_hw_breakpoints()` | `core/set_hw_bps` (set tracked client-side for reflash re-apply) |
| `core.clear_hw_breakpoint()` / `.clear_hw_breakpoints()` / `.clear_all_hw_breakpoints()` | `core/clear_hw_bps` (tracked set, client-side filtered) |
| `sess.clear_all_hw_breakpoints()` | `core/clear_hw_bps` per core, from the tracked set |
| `core.enable_vector_catch()` | `core/enable_vc` (no disable endpoint) |
| `core.id()` / `.architecture()` / `.core_type()` / `.fpu_support()` / `.is_64_bit()` / `.instruction_set()` / `.floating_point_register_count()` | `core/metadata` / `target/metadata` (+ built-in instruction-set tables) |
| `core.memory_regions()` | `target/metadata` (wire memory map) |
| `core.disassemble(addr, count)` | `core/disassemble` |
| `sess.load_debug_info(path)` | `debug_state/load_debug_info` (upload resolved via the client's upload cache) |
| `sess.stack_trace(core, limit)` | `stack_trace/rich` |
| `sess.scopes(core, frame)` | `stack_trace/scopes` |
| `sess.variables(ref, core, filter)` | `stack_trace/variables` |
| `sess.evaluate(expr, core, frame)` | `stack_trace/evaluate` |
| `sess.load_svd(path, core)` | `debug_state/load_svd` |
| `sess.resolve_source_breakpoints(file, lines)` | `debug_state/resolve_source_breakpoints` |
| `sess.clear_debug_state(core)` | `debug_state/clear_core` |
| `sess.attach_rtt(core, ptr)` | `create_rtt` (scan region) + `rtt/channels` (snapshot) |
| `up.read()` | `rtt/poll_up` (single-channel direct poll) |
| `up.pop(max_bytes, timeout_secs)` | local 1 MiB drop-oldest ring fed by the background poller on `rtt/poll_up` |
| `Rtt.start_streaming()` / `.stop_streaming()` | background poll task on the bridge runtime (dies with the connection) |
| `down.write()` / `.write_str()` | `rtt/down` (single non-blocking attempt) |
| `Rtt` drop / `sess.close()` | the server releases RTT clients together with the session/connection (no `rtt/clean_up` call is made; forced channel modes are not restored on drop) |
| `sess.flash(path, run, verify, progress)` | `flash/build` (+ `flash/verify` pre-check when `verify=True`) + `flash/flash` + `flash/boot`, then re-applied `core/set_hw_bps` |
| `pr.download_file(sess, …)` | `flash/build` / `flash/verify` / `flash/flash` / `flash/boot` |
| `pr.build_loader(sess, …)` / `loader.add_data()` / `.commit()` | `flash/build` / `flash/load_region` / `flash/flash` + `flash/boot` |
| `sess.erase_all()` / `.sequence_erase_all()` | `flash/erase_all` (server applies the vendor sequence when one exists) |
| `pr.ProgressEvent` | `flash/progress` stream topic (one-way; no timestamps) |
| `pr.CoreDump.capture(sess, core, ranges)` | `core/dump` (reconstructed into a local `CoreDump`) |
| `pr.CoreDump.load()` / `.store()` | direct local ELF-coredump file I/O |
| `pr.list_targets()` / `pr.get_target_by_name()` / `pr.Registry` | direct local `config::Registry` |
| `pr.SessionConfig` / `pr.Permissions` | fields of the `probe/attach` request |
| `pr.FakeProbe` / `Probe.from_fake()` | `probe/attach` with `dry_run` |
| `pr.aio(obj)` / `obj.aio` | pure-Python executor wrapper (no RPC) |
| `pr.ProbeRsError` | bridge error taxonomy + server `RpcError` strings (as `.kind`) |
