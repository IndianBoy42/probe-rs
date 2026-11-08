# pyprobe-rs

Python bindings for the [probe-rs](https://github.com/probe-rs/probe-rs) embedded
debugging toolkit, built for long-lived scripting sessions with hardware in the
loop: flashing, halting, memory/register access, RTT streaming, DWARF-based
debugging, and core dumps — from a REPL, notebook, or AI agent.

All operations run over RPC against a managed `probe-rs serve` process, so the
Python kernel never holds the probe: stale cells, kernel restarts, and leaked
handles cannot block your hardware.

> Full API reference with Rust-API mapping: [USAGE.md](USAGE.md)

## Install

Requires [maturin](https://www.maturin.rs/):

```bash
pip install maturin
maturin develop --release           # inside pyprobe-rs/, into the active venv
# or: maturin build --release && pip install target/wheels/pyprobe_rs-*.whl
```

The `probe-rs` CLI (the RPC server) is resolved automatically: `PYPROBE_RS_BIN`
env var → this workspace's `target/{debug,release}/probe-rs` → `PATH`.

## Quick start

```python
import pyprobe_rs as pr

# Attach through the managed probe-rs server (started automatically on a
# private unix socket; repeated pr.open() / Session.auto_attach() converge).
sess = pr.Session.auto_attach("nrf52840_xxaa")

core = sess.core(0)
core.halt(1000)                                # → PC
core.write_word_32(0x2000_0000, 0xDEADBEEF)    # MemoryInterface::write_word_32
print(f"RAM = {core.read_word_32(0x2000_0000):#010x}")
pc = core.read_pc()                            # CoreInterface::read_core_reg(PC)
core.run()
```

## The reflash loop

One call does resolve → build → flash → boot → hardware-breakpoint re-apply:

```python
sess.flash("target/thumbv7em-none-eabihf/release/app")   # run=True, verify=False
```

RTT keeps streaming while your code does other things (background poller on the
RPC runtime, 1 MiB drop-oldest ring per channel):

```python
rtt = sess.attach_rtt(0)
rtt.start_streaming(callback=lambda ch, data: print(data.decode(errors="replace"), end=""))
# ... other cells run; nothing is lost ...
chunks = rtt.up_channel(0).pop(4096)     # drain what arrived meanwhile
rtt.stop_streaming()
```

## Debugging without DAP/GDB

The RPC surface carries full server-side DWARF support — the agent-grade
replacement for a heavyweight debug adapter:

```python
sess.load_debug_info("target/.../app")            # publish ELF DWARF to the server
sess.stack_trace(0)                               # frames with function/pc/file:line
scopes = sess.scopes(0, frame_id=0)
vars = sess.variables(scopes[0]["variables_reference"])
sess.evaluate(0, "&config.clock_rate")            # DWARF expression evaluation
sess.resolve_source_breakpoints("src/main.rs", [12, 40])
core.disassemble(0x1000, 32)
```

## Long-lived kernels and stale cells

The probe is owned by the spawned `probe-rs serve` process, never by the
Python kernel. A kernel restart or an abandoned cell cannot hold the probe:
idle sessions release their lease on disconnect, and a server we spawned
self-heals (restart + reattach) if a previous kernel's session is still
lingering. Lifecycle handles:

```python
pr.sessions()          # live sessions of this process
pr.close_all()         # close them all (later calls fail fast)
pr.server_logs()       # ring buffer of the spawned server's output
pr.shutdown_server()   # stop the server we spawned (refused for foreign ones)
```

## Remote probes

The same API drives a probe on another machine — `open()` with a host binds to
an existing server instead of spawning one:

```python
pr.open(host="ws://rpi.local:3000", token="...")
# or: pr.open(host="ssh://user@rpi.local")
sess = pr.Session.auto_attach("nRF52840_xxAA")     # unchanged
```

(Prefer `ssh://` or trusted networks: upstream `wss://` does not verify TLS
certificates yet.)
Caveat: in current probe-rs dev builds the websocket transport's schema
handshake is unreliable upstream — the same binary pair fails its own
compatibility check over `ws://` while `socket://` works; remote hosts need
an upstream fix. pyprobe-rs surfaces it as a clean `ServerVersion` error
instead of hanging.

## Asyncio
Every blocking call releases the GIL. `pr.aio(obj)` (or `obj.aio` where
available) mirrors any facade method as an awaitable for background-style
workflows:

```python
await pr.aio(sess).flash("app.elf")
```

## Errors

Every failure raises `pr.ProbeRsError` with a `.kind` attribute and a message
that states the one-line fix: `"ProbeBusy"`, `"ServerGone"`, `"Connection"`,
`"ServerVersion"`, `"Timeout"`, `"Flash"`, `"Rtt"`, `"Register"`, …

## Testing

Sessions are RPC-managed and the protocol has no fake-probe attach (probe-rs
removed dry runs), so `Session.auto_attach(..., dry_run=True)` raises
`ProbeRsError` with `kind == "NotImplemented"`.

`uv run pytest tests/` runs the hardware-free API tests and skips the session
tests. Session and hardware tests read `PYPROBE_RS_PY_HIL_CONFIG` (a DUT TOML
in the smoke-tester schema):

```bash
PYPROBE_RS_PY_HIL_CONFIG=/path/to/dut.toml uv run pytest tests/ -m hil
```

See [USAGE.md](USAGE.md) for the complete guide: configuration, registers,
breakpoints, flashing options and progress callbacks, RTT streaming, the DWARF
debug surface, core dumps, and error handling. For running long-lived
hardware-in-the-loop campaigns — session discipline, probe/bus failure modes,
RTT as a command transport, halt budgets, and target forensics — read
[HIL.md](HIL.md).

## License

Same as probe-rs (MIT OR Apache-2.0).
