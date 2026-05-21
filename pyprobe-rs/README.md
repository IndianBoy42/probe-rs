# pyprobe-rs

Python bindings for the [probe-rs](https://github.com/probe-rs/probe-rs) embedded debugging toolkit.

## Installation

```bash
pip install maturin
maturin develop --manifest-path pyprobe-rs/Cargo.toml --features espressif
```

## Usage

```python
import pyprobe_rs as pr

# List probes
probes = pr.list_probes()
if probes:
    probe = probes[0].open()
    sess = probe.attach("nrf52", pr.Permissions())
    core = sess.core(0)
    core.halt(1000)
    core.write_word_32(0x2000_0000, 0xDEADBEEF)
    val = core.read_word_32(0x2000_0000)
    print(f"Read: {val:#010x}")
    core.run()
```

## License

Same as probe-rs (MIT/Apache-2.0).
