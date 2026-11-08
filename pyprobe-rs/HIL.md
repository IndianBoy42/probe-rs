# HIL testing with pyprobe-rs — field notes

Operational companion to [USAGE.md](USAGE.md). USAGE.md is the API reference;
this document is what multi-week hardware-in-the-loop campaigns taught about
*running* pyprobe-rs against a real board: session discipline, probe/bus
failure modes, RTT as a test transport, halt budgets, and the forensic
techniques that answer "is it the target or the toolchain" without guessing.

Everything here was learned on real rigs (STM32H7-class Cortex-M7 targets,
CMSIS-DAP probes, host-side Python driving gates for hours). The failure modes
are reproducible; the recipes are what actually shortened campaigns.

## 1. Session discipline (the lifecycle ladder)

The RPC design — a managed `probe-rs serve` owns the probe, the Python kernel
never does — removes most "abandoned cell holds my hardware" failure modes,
but not all. Three ways to lose the probe, all observed in a single campaign:

1. **Attach per logical section, close at the end of it.** A session object
   closed inside every notebook cell makes the next cell's `core` calls run
   against a closed session; a session never closed leaks the lease *and*
   the advisory lock until the kernel dies. One attach per coherent gate
   sequence, `sess.close()` when the section is done, fresh attach for the
   next.
2. **After a reflash, the pre-flash session is dead but still holds the
   lease.** Close it, then attach again. `close()` also releases the server
   lease, which is what frees `probe-rs download` for the next CLI flash.
3. **"The probe is claimed by another session on this server" while your
   kernel still holds a session object** is usually *your own* previous
   context (an orphaned kernel or a pre-compaction session), not another
   user. Read the holder hint (pid + cwd come with the error), verify with
   `pgrep`/`/proc/<pid>/cwd`, then close or kill *that* process — never
   blind-`pkill` a debug server.
   A claim can outlive the client process that created it: killing the
   claimant is not guaranteed to clear it. When the managed server is wedged
   (see §5), `kill -TERM` the serve pid directly — the next attach respawns
   it.

Long-lived kernels keep already-imported modules across context compaction,
so a kernel that predates an edit to your own tooling runs the *old* code.
When a tool's diagnostics look wrong after a compaction, reload its modules
(`importlib.reload`) or tie-break with a fresh subprocess before trusting
either output.

## 2. Mixing pyprobe-rs with the probe-rs CLI

- While any pyprobe-rs session exists, the managed `serve` daemon holds the
  USB device: `probe-rs download` / `probe-rs reset` from a shell fail with
  "The selected USB device could not be opened". Flash through the session
  instead — `sess.flash(elf)` does resolve → build → flash → boot in one
  call and re-applies tracked hardware breakpoints.
- `probe-rs download` can leave the core halted (DHCSR `C_HALT=1`) and a
  following `probe-rs reset` does not always run it. If the board answers
  nothing after a CLI flash: write DHCSR `0xA05F0001` (DBGKEY | C_DEBUGEN,
  no C_HALT) or run `timeout 40 probe-rs run --chip <chip> <elf>` — the
  target keeps running after the timeout kills the client.
- `probe-rs reset` right after an upload can fail with an `errno 16
  "interface busy"`-style USB race while the board is in fact running. Verify
  with a CLI ping or `core.status()` before retrying the reset.
- A `probe-rs` CLI invocation can spawn its own server whose RPC schema does
  not match the pyprobe-rs client's; the client then reports "the probe-rs
  server uses a different RPC schema than this client" until the mismatched
  serve process is killed and re-spawned. When mixing CLI and bindings,
  prefer driving a whole gate from one attached session.

## 3. RTT as a test transport

RTT is not just a log pipe — with a firmware CLI behind the down channel it
becomes a command/response transport you can gate on:

- **Command/response over RTT**: write the newline-terminated line to the
  down buffer, bump its `WrOff`, then poll the up buffer for the reply. The
  RTT control block is non-cached on typical firmwares (DTCM placement); no
  D-cache maintenance needed for the rings themselves.
- **Drain before the window you assert on.** The up ring is small (1 KiB is
  common) and holds *backlog*: a capture taken after an event shows stale
  lines unless you drained first. `NO_BLOCK_TRIM` channels drop the **newest**
  data when full — a full ring during your measurement window silently loses
  the evidence you wanted.
- **Wraparound looks like a reboot.** A decreasing `WrOff` plus boot-banner
  text is usually the ring wrapping while you re-read from a stale `RdOff`.
  Confirm real resets with a reset-cause register (e.g. RCC RSR on STM32)
  and a persistent crash store, never with `WrOff` alone.
- **Console replies are bounded** (~1 KiB per command on a 1 KiB ring). A
  longer reply is cut mid-line with no marker. Page (narrow verbs, skip
  cursors) or summarize on the target; never read a cut line as data.
- **The down ring proves input consumption.** The target maintains `RdOff`
  on the down descriptor; if you wrote commands and `RdOff` never follows
  `WrOff`, the firmware never read the line — the transport is fine, the
  reader is not. This one descriptor read separates "CLI task wedged" from
  "link broken".
- **Debug-domain RTT attaches can go deaf on multi-CB firmware.** If the
  running image can contain more than one `SEGGER RTT` control block (warm
  resets across rebuilds), a symbol-anchored attach can land on a dead block
  while the target is healthy. Fingerprint the block (up[0] name) and fall
  back to a RAM scan for the `SEGGER RTT` magic; keep the flashed image and
  the ELF you resolve symbols from in sync.
- **After a reflash, cached RTT addresses are stale** — the ELF moved
  `_SEGGER_RTT`, and reads at the old address fault with "a region outside
  of the AP address space was accessed". Close and re-attach; treat that
  error text as "stale session state", not a bus fault.
- **`Rtt.ptr()` is not exposed over the RPC wire** (0.32): when you need
  control-block geometry (max channel counts, descriptor layout) you must
  resolve the CB address yourself — pass the already-resolved pointer to
  `attach_rtt` and read the geometry via raw memory reads. An RPC-exposed
  pointer accessor would remove this workaround.
- **A fast-draining host can overrun the writer.** After a drain burst a
  1 KiB ring showed `used=1023 free=1` with `RdOff` one past `WrOff`, and
  later captures carried `0xA5` filler mid-line: the reader had wrapped past
  the writer into unwritten buffer. Drain continuously at a sustainable rate
  instead of in fast bursts.

## 4. Halt budgets and watchdogs

- **The target watchdog is not frozen by debug by default.** Every core halt
  eats into the feed window; a halt longer than the window (single-digit
  seconds on a typical IWDG) resets the board mid-read, and your evidence
  collection then reads junk. Either keep halts short (single-word reads are
  millisecond-scale), or set the debug freeze register yourself for the
  capture and clear it afterwards (STM32H7: `DBGMCU_APB4FZ1` bit for
  `DBG_IWDG1_STOP`) — if you leave it set the target never watchdog-resets.
- **Long halts can starve more than the watchdog.** Software budget/health
  watchdogs observe task activity, not CPU state; a multi-second halt can
  make a healthy subsystem unload itself on resume. Run a state check after
  any big dump.
- **Bulk memory reads: prefer word-wide access.** `read_32` measured ~4×
  faster than `read_8` on the same bytes over the same probe; a 128 KiB RAM
  snapshot takes ~1.7 s vs ~6.7 s. Halting once for one big read beats many
  small halts.
- **Boot-log capture**: `core.reset()` followed by continuous ring polling
  from offset 0 (a fresh RTT init zeroes both ring offsets) captures the
  banner; a reset-then-capture-later pattern discards it. If the core is
  left halted while you set up, the watchdog eats the window.

## 5. Probe and bus failure modes (and what they actually mean)

| Symptom | What it is | Recovery |
|---|---|---|
| Every access dies with `Failed to read register DRW … FAULT response`, recurring across re-attach, while the target is demonstrably alive | The managed `serve` process wedged (observed after hours of stepping + flash cycles) | `kill -TERM` the serve pid; the next attach respawns it. All sessions close; re-attach. |
| The same DRW FAULT *only* on one specific flash address, neighbors read fine | An ECC-inconsistent flashword: a flash program interrupted mid-word is unreadable by **any** master, including the debug AP. The error carries no address, so localize by probing neighbors. | Target-side: a guarded-read fault path that tolerates the word. Not a toolchain fault. |
| `WAIT` responses / DAP transfer errors while the *target* runs a flash operation (self-erase, on-target format, log commit) | On-target flash programming busy-stalls the AHB-AP: `attach_rtt`/`command`/reads die mid-operation. The command you sent before the stall may **still have been delivered** — check its effect on the target before resending (double-firing a destructive verb is worse than the wait) | Wait out the operation (an erase is tens of seconds), then read *state* via raw memory reads instead of re-running the CLI verb; re-attach after the flash is idle. |
| Attach right after `sess.flash()` fails similarly | The post-flash boot window — the core is mid-reset while the session wants the bus | Retry after a few seconds |
| Probe device number drifts in `lsusb`, `/dev/ttyACM*` indexes shift, in-flight calls die with USB errors | Probe USB re-enumerating itself (flaky cable/hub); the *board* keeps running and its own USB CDC usually re-enumerates too — find the new port by probing each candidate | Re-attach a fresh session; a full re-flash cleanly re-inits the board's USB when its CDC never comes back |
| A script that *prints nothing* under `timeout` hangs | Piped stdout is block-buffered; the prints died in the buffer | Rerun with `python3 -u`; if still silent, `strace -f -e trace=openat,read,connect` — one observed cause is probe enumeration blocking on a flaky USB *hub* sysfs read, cured by moving the probe to another port |
| `Could not determine a suitable packet size for this probe` on open | The probe is not answering `DAP_Info` — faulty probe hardware | Physical replug, then replace the probe; no software fix |

## 6. Forensics: SWD ground truth beats console text

When the console lies or is dead, memory and registers do not:

- **Is the target alive?** Read the scheduler tick (`uwTick`-equivalent), DWT
  `CYCCNT` (a cycle counter that only advances on a running core), and PC.
  Ring text can be stale; a frozen crash-store can predate the current boot.
- **Is the CPU in a fault?** PC inside the fault handler, `IPSR` (exception
  number, `xPSR & 0x1FF`) = HardFault, and CFSR/HFSR/BFAR/MMFAR tell you
  which. A fault handler that faults *again* is its own bug class: bound any
  fault-path stack walk by *mapped memory*, not by one region.
- **Healthy fault path vs re-faulting handler — the timing discriminator.**
  A handler that writes its report and calls reset answers a ping again in
  well under a second; one that re-faults waits out the watchdog window.
  Measure ping-recovery time; it separates the two without a debugger.
- **Reset cause before the next boot.** Reset-cause registers (e.g. RCC RSR
  IWDG/PIN flags) latch and are cleared early by firmware — read them while
  the core sits at the reset vector, not after boot.
- **A crash-report retention region is the highest-value forensic asset.**
  Persistent (backup-domain / no-reset-section) report slots with PC/LR/SP/
  fault registers survive resets and pay for themselves every campaign.
  Extract with one bulk read and symbolize host-side (`addr2line`).
- **Hardware breakpoints on trap instructions catch misuse at the source.**
  A `udf`-style trap has few sites; enumerate them, break on one, and the
  PC/LR/r0 at the halt name the offender before the fault cascade buries it.
  (Re-resolve symbol addresses after every reflash — a stale address
  silently never hits.)
- **Register numbers, not names, for raw reads.** `core.read_core_reg(13/14/15/16)`
  = SP/LR/PC/xPSR on Cortex-M — useful when the DWARF surface isn't loaded.
- **The debugger being attached can itself change target behavior.** Firmware
  `bkpt`/`__builtin_trap`-style statements halt the core *only when
  C_DEBUGEN is set* (a probe attached) and otherwise escalate to a fault or
  reset. A "hang" seen only during probe sessions can be exactly this —
  before blaming the firmware, reproduce with the probe detached (watch the
  board's own USB serial presence, or a second target-side liveness signal).
  The inverse trap exists too: a watchdog reset storm produces bogus PC
  samples (mid-reset-handler addresses) that look like crashes but are the
  reset cycle itself.
- **Not every special register is in the register table.** `primask` and
  friends may be missing (`unknown register`); route around it with direct
  memory reads of the SCB/NVIC/SysTick registers — `ICSR` in particular
  gives `VECTACTIVE`, which names the stuck interrupt without any register
  table at all. Immediately after a target watchdog reset, AP/DP reads can
  transiently fail with `Failed to read register DRW at 0xd0c` /
  "timeout during an operation" — retry with a fresh session before
  treating it as a dead probe.

## 7. Test design that survived campaigns

- **Gates with numbers, evidence in the artifact.** Every gate carries a
  numeric threshold and its raw evidence line embedded in the result JSON —
  offline RCA should never need the board again. A gate that "felt green"
  is not a gate.
- **A/B on one board, one session.** Flash the baseline image, record, flash
  the change, record again — same board, same card, same temperature. The
  alternative (compare against last week's numbers) is noise.
- **Report throughput *and* tail latency, never conflate them.** A change
  can improve throughput while making p90 slightly worse; both numbers in
  the result, the conclusion drawn from both.
- **Record honest FAILs.** A perf gate that missed its target is recorded as
  a fail with the measured number — the follow-up observation (e.g. "2 KiB
  batches write ~2× the sectors") is exactly the kind of finding a padded
  pass would have buried.
- **Deadline-bounded everything.** No wait in the harness waits forever; a
  no-response is a classified outcome, not a hang.
- **Verify expected values offline before board time.** A test fixture whose
  expected constant drifted from the generated schema fails on hardware in
  a way that wastes a session; a throwaway host-side check against the exact
  generated headers is cheap insurance.
- **Design the instrument into the fixture.** Wake/queue coalescing is only
  observable with a burst producer and a consumer that prints
  count-per-wake; a paced producer can never show it. Prefer monotonic
  counters in module output over line presence: a counter jump = lost
  events; a continuing counter = resume.
- **Crash-count as a cheap invariant.** Note the persistent crash-report
  count at session start; it must not move across your gates. (A reflash
  clears the store — read the count *before* flashing if you want the
  previous session's reports.)
- **Fault injection is compiled out of production.** "What happens when the
  card is pulled" is injected at the choke point the real failure passes
  through, returning what the hardware returns, inside a build flag that
  folds to `constexpr false` in production images. Never a runtime switch a
  stray command could arm.
- **Probe-driven reset ≈ power cut for RAM state.** For durability gates, a
  debug reset loses volatile state exactly like a power cut; inject torn
  writes and outages in firmware rather than unplugging things mid-run.
