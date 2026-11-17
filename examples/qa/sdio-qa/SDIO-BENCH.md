# SDIO P4 host / C6 slave: throughput benchmark

Separate from the H0-H7 qualification scenarios, which it does not replay. It measures the
useful throughput and the latency of the existing explicit cycle (arm, transfer, completion,
hand back, re-arm) between the ESP32-P4 host and the ESP32-C6 slave. **The production drivers
are not modified to improve a figure**: `build.py` refuses to build if a production path differs
from the qualified base, and the benchmark only calls the public API. Nothing here qualifies the
driver, SLC1, descriptor chains or automatic re-arming. **Hardware trials not executed in this
revision.**

## What the code allows (read from the sources of this revision)

### P4 host: `esp-hal` SDMMC, slot 1 through the GPIO matrix

| Item | Hardware or API | What the code says |
| --- | --- | --- |
| Bus width | API | `BusWidth::Bit1` and `Bit4` (4 needs the four data pins connected; the QA wiring has them: GPIO14..17). `Bit8` is slot 0 only. |
| Frequency | API | `set_bus_low_level(width, hz)` accepts `1..=40_000_000` Hz. |
| Real clock | API | `card = module / (2 * div)`, module = 160 MHz PLL / `module_div` (default 2) = **80 MHz**, so only 40, 20, 13.3, 10, 8, 6.7, 5.7, 5 MHz... exist: a request of 25 MHz gives **20 MHz**. The plan uses 400 kHz, 5, 10, 20 and 40 MHz, which are exact, and the report always prints the real clock. |
| Sampling phase | Hardware, API | Set (`DelayPhase`, default 0 degrees) only above 25 MHz, selectable in `SlotConfig`. **Not tuned here**: 40 MHz runs with the default phase. |
| Transfer size | API | The IDMAC ring (4 descriptors of 4096 bytes) is refilled during the transfer, so the length is not bounded by the ring. `block_size` is `u16`, `block_count` `u32`. |
| Buffers | API | `DmaAlignedMut::new` needs address and size multiples of the 64-byte cache line: every size of the plan is one. |
| CMD53 | SD spec, API | Byte mode counts on 9 bits: the plan uses it up to 256 bytes. Above, block mode with 512-byte blocks, whose size is written to function 1 `0x110/0x111` and read back. No auto-stop (the driver only adds one for CMD18/CMD25). |
| Mode | API | Blocking calls: the P4 does not overlap computation and transfer. |

### C6 slave: `esp-hal` `sdio_slave`

| Item | Hardware or API | What the code says |
| --- | --- | --- |
| Pads | Hardware | Fixed IO MUX: CMD GPIO18, CLK GPIO19, DATA0..3 GPIO20..23; **all six are required** before `enable_bus()`. The four data pads exist in the API, which has no width option. |
| Bus width, speed, block size | Hardware | **Not configured by the driver**: they are the reset values of the CCCR/FBR registers, programmed by the host. The repository does not state the maximum SDIO frequency or the supported widths of the C6 slave. The benchmark therefore **reads** CCCR `0x00..0x14` and FBR1 `0x100..0x111` at the qualified setting and logs them (`bench_caps`); it does not assume them. |
| Transfer size | API | One descriptor: `arm_receive` takes 1 to 4095 words (16380 bytes), `arm_transmit` a length up to the buffer (and `0x3ffc` bytes). The largest packet of the plan, 4096 bytes, is inside the limit. |
| Direction | API | One direction armed at a time, explicit re-arming; credits are cumulative modulo 4096 and the published length modulo 2^20. |
| Block-mode transfers | Hardware | **Not documented by the driver.** The host reads or writes at `0x1F800 - length`; whether the C6 accepts multi-block transfers into its single descriptor is what the 1024 and 4096-byte configurations will show. An error there is a result, not proof of a driver defect. |

Not measured and not claimed: 8-bit, UHS, DDR, packets above 4096 bytes, SLC1, descriptor chains,
interrupts to the host, an asynchronous or interrupt-driven P4.

## Method

**Plans** (`SDIO_BENCH_PLAN`, scenario `bench-<plan>`): `smoke` (the qualified configuration, 64
bytes, one 1 s window: a dry run, not a result), `qualified` (400 kHz, 1 bit, sizes 64, 256, 1024,
4096), `w1` (1 bit, 5, 10, 20, 40 MHz, every size), `w4` (4 bits, 400 kHz to 40 MHz, every size),
`full` (the three in this order, 40 configurations). A configuration is a width, a requested
frequency and a packet size.

**Fresh session per configuration.** The P4 resets the C6 (GPIO54), enumerates it at the qualified
400 kHz / 1 bit setting (80 initialization clocks, CMD0, CMD5, CMD3, CMD7), then reads the
capabilities (first session only), writes the function 1 block size (block mode), the bus width
(CCCR `0x07`, read back) and the high-speed bit (CCCR `0x13`, only above 25 MHz and only if the
card advertises it), and finally programs its own width and clock. A card that does not offer what a
configuration needs is reported `UNSUPPORTED` (not an error of the transfer).

**In-band control.** The C6 knows nothing in advance. Before each phase the P4 sends a 64-byte
control packet (direction, packet length, count, phase identifier, checksum) through the same
explicit cycle; after the phase the C6 sends a 64-byte summary (packets and bytes validated,
content errors, first bad byte and sequence number). Neither packet is inside a timed window.

**Phases.** For each configuration and each direction (P4 to C6, C6 to P4, round trip): a warm-up
of 8 packets, whose packet time sizes the windows; three windows of about five seconds; then,
after all configurations, the best error-free one (highest mean round-trip goodput) is run again
for thirty seconds in each direction (the C6 is reset again). Windows hold a fixed number of
packets, computed from the warm-up, because a transfer cannot be interrupted without invalidating
the session: the effective duration is measured and reported, not assumed.

**Patterns.** Every packet carries its sequence number in word 0 and its length in word 1, and a
pattern derived from the sequence number, the phase and the direction in the other words. The
receiver compares all bytes; a stale, shifted, truncated or other-phase packet cannot match. The
timed loops print nothing; one line is printed per measurement, afterwards.

**What the time covers (end to end).** Pattern generation, the waits for the credit or the published
length (polling `IOR1`, then two function-1 bytes for the credit or three for the length), the CMD53,
the validation, and the C6 completion and re-arming. These polls are part of the explicit protocol
and of the figures: a host that waited for an interrupt would do better, and that is **not** what
is measured. The window ends when the C6 publishes its summary, i.e. after it handled the last
packet.

**Errors, no retry.** The first error of a measurement ends it: no packet, command or transfer is
repeated. It is counted by class (content, CRC, R5, timeout, other) with the packets done before
it and the R5 flags. The configuration is not retained as a reliable maximum, its remaining
directions are not measured, and the ladder rule skips the higher frequencies of the same width and
size (`SKIPPED`). A transient FUNCTION_NUMBER refusal while polling (the driver withdraws `IOR1`
at each completion) is not an error: it is counted per measurement (`not_ready_polls`) and shown.
The credit and the length are read byte by byte while the C6 updates them, so a value read across
an update (a torn read, for example `0x0ff` then `0x100`) is neither the old nor the new one: an
unexpected value is polled again and only the same unexpected value twice in a row is an error;
the number of such polls is reported (`unstable_polls`). Polling is repeated, a transfer never is.

## What is reported

Per measurement: configuration, **packet size, requested and real frequency, bus width**, mode,
window, packets and **useful bytes validated** (both directions for the round trip), **effective
duration**, goodput (useful bytes per second), packets per second, round-trip latency (minimum,
mean, maximum), error classes, `not_ready_polls`, and what the C6 validated. The evaluator
(`bench_verdict.py`) cross-checks every P4 phase against the C6 summary, requires the goodput to
equal bytes over duration, requires the useful bytes to equal packets x size x directions, applies
the same checks (and the P4/C6 cross-check of every phase, parameters and counters included) to the
warm-ups, the windows and the 30-second confirmation, requires the confirmed configuration to be the
computed best, the announced best and to carry the same parameters, derives the best error-free configuration itself and compares it to the
one the firmware chose, checks the 30-second confirmation, and bounds the number of log lines (no
per-packet logging). It writes `verdict-bench-<plan>.json` (every check, never truncated) and a
markdown table `report-bench-<plan>.md` in the evidence folders. **A C6 `receive timed out` that comes after the last phase it served (its 30 s idle timeout, when nobody
talks to it any more) is the end of its session, not a failure, provided every phase the P4 ran in that
session was served; any other C6 failure on a clean configuration stays a FAIL. The run is a PASS when it
completed and the two logs agree; errors found on exploratory steps are findings in the report, not
failures of the run. The exception is the reference configuration (configuration 0 of `smoke`,
`qualified` and `full`: 400 kHz, 1 bit, 64 bytes), which must be clean in the three directions:
otherwise the verdict is FAIL and the campaign stops.** The source SHA and the SHA-256 of the images are those of the build
manifest (`build.py`), recorded with the evidence, not in the pull request.

## Running it

Same protections as the qualification: the images (`sdio-bench-host`, `sdio-bench-slave`, scenario
`bench-<plan>`) are programmed by the recipe with the two backups, the independent readback and
the restoration. The operator's consent lists the scenarios and `parameters.json` gives the
observation time (`scenario_timeouts`, at least 90 s for `smoke`, 600 for `qualified`, 1500 for
`w1`, 1800 for `w4`, 3300 for `full`). Run `smoke` first, then `qualified`, before the ladders.
The benchmark is long: keep the PC awake and the grants alive. The campaign renews both
reservations every 300 s during an observation; a failed renewal stops the observation, keeps the
partial evidence and runs the cleanup. At the end, once the boards are restored, the campaign asks the
operator to revoke the local grants and waits up to 5 minutes for them to disappear (`Campaign.close()`
refuses an active grant); it never revokes a grant itself and reports one that stays active.

## Re-reading captures without flashing again

A change of the evaluator does not need a new hardware run. `reanalyze.py --p4 <evidence p4>
--c6 <evidence c6> --scenario bench-smoke` re-evaluates the saved captures and adds
`reanalysis-<scenario>-<evaluator commit>.json` to the P4 evidence: firmware source SHA that ran,
evaluator commit and file digests, SHA-256 of every raw input, the original verdict and the new
one. The original verdict is kept untouched and an existing reanalysis is never overwritten. It does
not make the run a pass of the revision that produced it.

## Limits to keep in mind

- One run per configuration after warm-up; three windows give a spread, not a statistical bound.
- The goodput is that of this blocking, polling host and of the single-descriptor slave cycle.
- 40 MHz uses the default sampling phase, untuned.
- Whether the C6 accepts block mode, 4 bits or high speed is unknown until measured.
