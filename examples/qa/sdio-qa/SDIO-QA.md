# SDIO P4 host / C6 slave: hardware qualification kit

Targets the native SDIO slave driver of this branch (`esp-hal/src/sdio_slave.rs`), exercised
by an ESP32-P4 host (SDMMC slot 1) against an ESP32-C6 slave. **Hardware trials not executed
in this revision.** Nothing here qualifies descriptor chains, SLC1, automatic rearming,
performance, or any board other than the one named in the parameters.

The production driver is not modified: `build.py` refuses to build if any production path
differs from the qualified base commit and records that proof in the build manifest.

## Contents

| File | Role |
| --- | --- |
| `../src/bin/sdio-qa-slave.rs` | C6 firmware, all scenarios |
| `../src/bin/sdio-qa-host.rs` | P4 firmware, all scenarios |
| `../src/sdio_qa.rs` | Shared patterns, counters and scenario selection (host-tested arithmetic) |
| `build.py` | Reproducible build of the five scenario pairs and `build-manifest.json` |
| `verdict.py`, `test_verdict.py` | Console parsing and verdict logic; software tests on synthetic logs |
| `inputs.py`, `test_inputs.py` | Refusal of templates, unreviewed builds and mismatched consent |
| `campaign.py`, `test_campaign.py` | Campaign flow with injectable collaborators; safeguards tested with doubles |
| `test_sdio_qa_hardware.py` | Opt-in Device Lab recipe (`iobewi_bench`), skipped by default |
| `parameters.example.json`, `consent.example.json` | Templates. A template authorizes nothing. |

## Scenario selection and builds

One firmware pair per scenario, selected at build time:

```sh
SDIO_QA_SCENARIO=h4 cargo xtask build sdio-qa-slave esp32c6
SDIO_QA_SCENARIO=h4 cargo xtask build sdio-qa-host esp32p4
python examples/qa/sdio-qa/build.py --base <qualified base SHA> --output <new private dir> \
  --p4-flash-size 32mb --c6-flash-size 8mb   # the verified capacities of the two boards
```

Scenarios: `h1`, `h2`, `h3`, `h4`, `h567` and the P4-only control image `c6ctl` (H5, H6 and H7 form one ordered sequence because H7
resynchronizes after H5 and H6). Each image is a
**full flash-prefix image** made from the ELF by `espflash save-image --merge --skip-padding`:
it starts at flash address 0 and holds the second-stage bootloader (bundled with espflash,
ESP-IDF v6.1-dev, offset 0x0 on the C6 and 0x2000 on the P4), the partition table at 0x8000
(`nvs`, `phy_init`, `factory` at 0x10000) and the application. Nothing is padded to the flash
size. `image_layout.py` parses the image and the manifest records its layout; the P4 images
require chip revision 3.00 or later. **Writing a bootloader and a partition table needs the
operator's distinct, explicit consent** (`write_bootloader_and_partition_table`).
The manifest records the source SHA, the proof that production sources are unchanged, the
`esp-hal` package path Cargo resolved (a local path, never the registry), the command, the
target, and the SHA-256 of every ELF and image.

## How the two MCUs stay in lockstep

There is no side channel besides the SDIO bus and the C6 EN line (P4 GPIO54). Both firmwares
advance only on bus-visible state:

* the cumulative receive credit, function 1 register 0x44 bits 27..16 (modulo 4096);
* the cumulative published length, function 1 register 0x60 (modulo 2^20);
* CCCR `IOR1` (function 0 register 3 bit 1), set by `set_function1_ready()`.

The slave arms a transfer first (credit or length becomes visible), and declares `IOR1` only
when the host may act. The host waits for the expected counter value **and** `IOR1`; any other
counter value is a failure ("extra credit", "unexpected length"). In H4 the slave declares
readiness only after the cancelled wait, so the cancellation provably precedes the host
transfer. Everywhere else every wait on the peer is bounded (10 s) and ends in an explicit
failure. Nothing is retried.

Direction vocabulary: SLC0 **TX** receives from the host; SLC0 **RX** transmits to the host.
Logs name the direction as `slc0_tx(host->slave)` or `slc0_rx(slave->host)`.

Timing assumptions, all bounded and checked by outcome: after the slave stops its bus (H5,
H6) it keeps the pads released for 2 s, then recreates its driver; the host re-enumerates
after 3.5 s with up to ten enumeration attempts (enumeration changes no card state that a
fresh `SdioSlave::new` does not reset).

## Scenario contracts

Common effects: the P4 firmware holds the C6 in reset at boot, then releases it; a P4 reset
therefore starts a fresh C6 session. Only the flash prefix covered by the images is written (the largest scenario image of a board sets its region, rounded to whole sectors); the flash is never erased as a whole.

| ID | Objective | Assertions (both firmwares check, `verdict.py` re-checks from the logs) |
| --- | --- | --- |
| H0 | Identify, back up, program | Chip and flash size equal the parameters; the existing table is recorded (blank or erased flash yields no valid table and is accepted; a valid table is never overwritten without `allow_overwrite_valid_table`); two reads of the region to write are identical and, if configured, match the historical backup, otherwise no write; programming has an independent `flash.read` equal to the full image, then the table read from the board must equal the image's table; no erase. |
| H1 | 64-byte round trip, three fresh sessions | Enumeration; P4->C6 and C6->P4 payloads compared byte for byte on both sides (`0..63` ramp, echoed as in the public example); credit 1 and length 64 in every session; three C6 reboots. |
| H2 | Reuse in one session | Three round trips without reset or re-enumeration; the pattern changes with every packet and the response is independent of the request; credits 1,2,3 and cumulative length 64,128,192; host consumed exactly 64 bytes and one credit per round; no credit appears without a re-armed resource; state returns to `BusExposed` between directions. |
| H3 | 65-byte C6 -> P4 | Allocation of 128 bytes, exactly 65 published and read with a CMD53 of count 65; length announced 65, received 65, each byte compared; bytes past the word-rounded length are observed. |
| H4 | Cancel a wait, then resume (both directions) | A real wait is started and cancelled by timeout; the state stays armed, the descriptor word is exactly what it was before the wait and still DMA-owned, no completion or fault event is pending, no extra credit or length; `IOR1` is declared only afterwards (so the slave's own log proves cancellation precedes the host transfer; the host's wait duration is not an assertion); the resumed wait completes with the exact content. A transmit descriptor is armed with EOF already set (end of packet): that bit is not a completion event. |
| H5 | Stop an unused receive | Restitution refused while DMA is active; `disable_bus()` result and PARK observed; restitution only after confirmed stop; state `SessionInvalidated`; `enable_bus()` refuses the instance; the host sees the bus released and consumed nothing. |
| H6 | Stop an unconsumed transmit | Same contract, with the published length 64 still visible to the host before the stop. |
| H7 | Resynchronize | Storage reused in a new driver instance (the first clock user resets the block); host accounting recreated and the host re-enumerates; no stale credit (0 or 1 right after enumeration, then exactly 1) or length (exactly 64); a new 64-byte round trip validates. |

If the stop times out in H5 or H6 the firmware verifies that restitution is still refused,
prints `stop_failed|...|allocations_retained=true` and reports a failure; it never forces
recovery.

### H3 and the host API

`DmaAlignedMut::new` requires a multiple of the cache line, so the P4 firmware builds the
65-byte view with `new_unchecked` over a prefix of a `repr(align(64))` 128-byte buffer that
the transfer owns. The SDMMC host rounds the descriptor size up to a whole word (`(size + 3)
& !3`) while programming `BYTCNT` with 65. If the controller or the cache maintenance rejects
this, the P4 logs `cmd_error|CMD53 read` and the verdict is not `PASS`; classify it `BLOCKED`
only with that evidence and the documented constraint, never replace it by a 64- or 68-byte
transfer.

## Log format

Both consoles print `QA|<P4|C6>|t=<ms since boot>|<kind>|key=value...`. The clocks are
independent; correlate with the bench UART timestamps and with `c6_en|low`/`c6_en|high`
(the C6 boots when EN rises). Evidence lines:

* `cmd52|f=..|addr=..|r5_data=..|r5_flags=..` for enumeration writes, `cmd53_write`/`cmd53_read|r5=`;
* `credit_observed`, `length_observed`, `ready_observed|waited_ms`, `request_sent`,
  `response_check`, `response_consumed` (host counters);
* `diag|<tag>|slc0_tx|slc0_rx ... dw0=<raw> owner_dma eof len size token|published int_raw park`
  and `snapshot|...` (C6 registers, raw values; the owner/eof/length decoding is an
  interpretation of the descriptor word, never a proof of memory safety);
* `cancel*`, `pre_stop`, `stop`, `restitution`, `reopen`, `stop_observed`;
* `RESULT|<scenario>|P4_PASS|C6_DONE` or `..._FAIL|reason`.

The firmware never reads or writes a buffer while the DMA owns it, never fabricates a
completion, and uses only public driver API. A `park=true` observation is a register
observation, not an exhaustive safety proof. Software interpretation, register observation
and unobservable properties are reported separately.

## Campaign safeguards

* **Soft control of the C6 from the P4.** The C6 EN pin is wired to P4 GPIO54, only the C6
  UART TX/RX reach the operator, and a P4 reset reboots the C6. The P4 therefore runs a control
  firmware (`c6ctl` scenario, also the idle state after every scenario) that reads console lines
  and drives GPIO54: `C6 HOLD` (EN low), `C6 RELEASE` (EN high), `C6 RESET` (EN low 200 ms then
  high) and `C6 PING`; each is acknowledged as `c6ctl|ack=<command>|en=<level>`. The recipe sends
  them with the bench `uart.write` command (local grant) and opens the P4 console only once per
  P4 boot, because opening the port may itself reset the P4.
* **Order of a campaign.** The P4 is identified and backed up in ROM download, then written with
  the `c6ctl` image and reset. For each scenario: the operator grounds the C6 GPIO9 (BOOT) by
  hand, the P4 resets the C6 by software so that it enters ROM download, and the C6 phase runs
  in one go (identify and two backups once, programming, table check); the operator then releases
  GPIO9, puts the P4 in ROM download and the P4 scenario image is written and booted. The P4
  reset reboots the already programmed C6. The C6 restoration uses the same GPIO9 + `C6 RESET`
  sequence through the control listener.
* After a reset the C6 has a fresh ROM, but the page keeps its manual-entry loader session open
  and its commands then time out with no byte (observed). The recipe therefore issues the
  `console` command on the C6 before each reset: it is the only way to leave that session, and
  the next loader command re-synchronizes. The C6 loader speed is its own setting (115200).
* A missing acknowledgement stops the campaign before any C6 write; only `C6 PING` is repeated,
  because it changes nothing. Do not probe the C6 beforehand.
* The campaign stops at the first scenario that is not `PASS`; evidence is kept, nothing more
  is programmed, then cleanup runs.
* A board is restored only if a write was engaged on it. A backup alone is not a reason.
* Operations are budgeted for `Orchestrator.program()`: `2 x timeout + 90` seconds of grant and
  reservation (programming 120 s -> 330 s; restoration and backups 300 s -> 690 s). The
  parameters must grant at least 690 s; the reservation is renewed before every write.
* Verdict priority: a demonstrated violation is `FAIL`; a truncated or missing trace is
  `INDETERMINATE`. A deliberate probe of a released bus (`probe_no_response`) is not an error;
  any other `cmd_error` is.

## Review and campaign gate (repository rules)

Per `AGENTS.md` of the Device Lab repository (`rust_dev`), section "Méthode obligatoire:
rédiger, relire, puis qualifier", steps 2 and 3, and `bench/docs/qualification-workflow.md`
section 3 ("Revue croisée avant matériel"): the PR and its head SHA are reviewed by Codex; no
hardware trial starts before that review of the retained commit and the agreement of the
project owner for the campaign. A PR, a green CI or a pytest opt-in is not that approval.
`consent.json` records the operator's agreement; the assistant never writes it.

## Operator prerequisites (hardware not yet available to the preparation)

1. Both boards connected through the Device Lab page on the operator's PC, each with its own
   logical id, chip and verified flash capacity (the C6 capacity is not inferred from its
   module name). The current server lists only two `esp32p4` boards and no `esp32c6`.
2. C6 without DTR/RTS: select "manual entry" (`no_reset`, loader session kept open); put the C6
   in ROM download by hand. The P4 must be held in reset or running firmware that leaves
   GPIO54 alone, so that EN is not toggled while the C6 is loaded. The C6 loader speed is its
   own setting; do not reuse the P4 value.
3. P4: native USB appears only after BOOT+RESET (`303a:0012`, manual entry), or use the UART
   bridge at 115200; declare which in `transports`. After programming, a reset is a manual
   gesture (no `reset` capability on that port). Both application consoles are 115200 baud
   (`esp-println` is silent until UART0 is initialised, which both firmwares do).
4. The existing flash state of each board, from `identify` and `partitions`: a blank or erased
   flash has no valid table (observed on the P4 on 2026-11-16, not proof that it is blank); if
   a valid table exists the operator decides whether it may be overwritten (backed up and restored
   by the campaign). Nothing outside the image prefix is written.
5. Private files outside Git: parameters, consent, evidence root on verified persistent
   storage, `BENCH_CLIENT_TOKEN` in the process environment, a local grant for each programming
   operation (revoked afterwards), and enough reservation time for programming and readback.
6. A fresh P4 backup pair and, if a historical backup is used, its cross-check against a fresh
   read; a mismatch blocks programming.

## Software checks (no hardware)

```sh
python -m pytest examples/qa/sdio-qa/test_verdict.py examples/qa/sdio-qa/test_inputs.py \
  examples/qa/sdio-qa/test_campaign.py -q
rustc --edition 2024 --test examples/qa/src/sdio_qa.rs -o /tmp/sdio_qa_tests && /tmp/sdio_qa_tests
```

These use synthetic logs and a pure Rust module (including descriptor decoding). They validate the verdict and input logic
only; a simulated PASS is never a hardware PASS.
