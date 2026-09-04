# pyprobe-rs

Python bindings for the [probe-rs](https://github.com/probe-rs/probe-rs) embedded
debugging toolkit, for dynamic scripting sessions with hardware in the loop:
flashing, halting, memory/register access, RTT, SWO, and core dumps — all from
a REPL or notebook.

> Full API reference with Rust-API mapping: [USAGE.md](USAGE.md)

## Install

Requires [maturin](https://www.maturin.rs/):

```bash
pip install maturin
maturin develop --release           # inside pyprobe-rs/, into the active venv
# or: maturin build --release && pip install target/wheels/pyprobe_rs-*.whl
```

## Quick start

```python
import pyprobe_rs as pr

# pr.list_probes()  ==  probe_rs::probe::Lister::new().list_all()
probes = pr.list_probes()
probe = probes[0].open()                       # probe_rs::DebugProbeInfo::open()

# probe.attach(target, perms)  ==  probe_rs::Probe::attach(target, permissions)
sess = probe.attach("nrf52840_xxaa", pr.Permissions())

# sess.core(0)  ==  probe_rs::Session::core(0)  (lending handle, see USAGE.md)
core = sess.core(0)

core.halt(1000)                                # Core::halt → returns PC
core.write_word_32(0x2000_0000, 0xDEADBEEF)    # MemoryInterface::write_word_32
val = core.read_word_32(0x2000_0000)           # MemoryInterface::read_word_32
print(f"RAM[0x2000_0000] = {val:#010x}")

pc = core.read_pc()                            # CoreInterface::read_core_reg(PC)
core.run()                                     # Core::run
```

Flash a firmware image:

```python
pr.download_file(sess, "firmware.hex", "hex")  # probe_rs::flashing::download_file_with_options
```

Stream RTT logs:

```python
rtt = sess.attach_rtt(0)                       # probe_rs::rtt::Rtt::attach
up = rtt.up_channel(0)
print(up.read_str())                           # non-blocking read of pending data
```

Every failure raises `pr.ProbeRsError` with a `.kind` attribute
(`"Timeout"`, `"Rtt"`, `"Flash"`, `"Probe"`, …).

## Testing without hardware

```python
probe = pr.Probe.from_fake(pr.FakeProbe.with_mocked_core())
sess = probe.attach("Cortex-M4", pr.Permissions())
```

See [USAGE.md](USAGE.md) for the complete guide: configuration, registers,
breakpoints, flashing options and progress callbacks, RTT channel modes, SWO,
core dumps, and error handling.

## License

Same as probe-rs (MIT OR Apache-2.0).
