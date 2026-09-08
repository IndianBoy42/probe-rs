pyprobe-rs now runs as a facade over the probe-rs RPC protocol. Opening a session transparently starts (or reuses) a private `probe-rs serve` process, and every operation — core control, memory, registers, breakpoints, flashing, RTT and DWARF-based inspection — goes through it. Sessions become long-lived and restartable: repeated opens are idempotent, a stale Python cell can no longer keep the probe claimed (`close()` releases the probe lease immediately, and the manager heals a busy server it spawned itself), RTT can stream in the background while other code runs, and the same script can drive a remote probe by pointing `open()` at a `serve` host instead of spawning one.

Reset semantics are debugger-grade: `reset()` always leaves the core
running (the Cortex-M debug halt request survives a system reset), the
tracked hardware-breakpoint set is re-applied after reset, reset-and-halt
and flash, `close()` releases the probe lease immediately, a busy
self-spawned server heals automatically on the next attach, and
`shutdown_server(force=True)` reclaims an adopted server whose pidfile
verifies as `probe-rs serve`. Variable dictionaries now carry the DAP
`memory_reference`.
