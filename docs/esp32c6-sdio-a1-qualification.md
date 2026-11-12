# ESP32-C6 SDIO Slave A1 — hardware qualification recipe (P4 host ↔ C6 slave)

Status: **prepared, not executed.** No firmware described here has been flashed or run. No result
in this file is an observation.

Scope: qualify the A1 bring-up of `esp_hal::sdio_slave` (clock, pads, `IOREADY1/2` kept at 0) by
enumerating the ESP32-C6 from the ESP32-P4 SDMMC host on a Waveshare ESP32-P4-WIFI6-POE-ETH board.
Out of scope: DMA, CMD53, interrupts, `IOREADY` set, high speed / UHS, any application protocol.

Related: `docs/esp32c6-sdio-slave-bringup.md` (driver contract, TRM findings, open points).

## 1. Firmwares

| Firmware | Source | Chip | What it does |
|---|---|---|---|
| `sdio-a1-c6-slave` | `examples/qa/src/bin/sdio-a1-c6-slave.rs` | ESP32-C6 | `SdioSlave::new`, `enable_bus`, register snapshots, function states, monitor loop. No DMA, no interrupt, no `IOREADY`. |
| `sdio-a1-p4-host` | `examples/qa/src/bin/sdio-a1-p4-host.rs` | ESP32-P4 | Drives C6 EN (GPIO54), then CMD0, CMD5, CMD3, CMD7 and CMD52 **reads** of CCCR, FBR and CIS, at 400 kHz, 1-bit, SDMMC slot 1. |
| shared logic | `examples/qa/src/sdio_a1.rs` | both | Decoders (R4, R6, CCCR, FBR), bounded CIS walk, tally. Compiled by the host tests too. |

The P4 firmware never writes a CCCR register, never changes bus width or clock, never negotiates
high speed or UHS, never enables a function and never issues CMD53. Neither firmware prints a
global verdict: it is established from both console logs (section 7).

### Build

From the repository root, with the revision of the sources and an identifier of the trial:

```sh
SHA=$(git rev-parse --short=9 HEAD)
export SDIO_A1_GIT_SHA=$SHA SDIO_A1_RUN_ID=<trial-id>
cargo xtask build qa esp32c6 sdio-a1-c6-slave
cargo xtask build qa esp32p4 sdio-a1-p4-host
```

The ELF files are written under the repository `target/` directory, to
`target/riscv32imac-unknown-none-elf/release/sdio-a1-c6-slave` and
`target/riscv32imafc-unknown-none-elf/release/sdio-a1-p4-host`. Check that the source SHA is embedded
(`strings -a <elf> | grep <sha>`); a build made without the variable prints `sha=unknown`. Both builds select the
`esp-println/uart` console through the `//% FEATURES` annotation. Record the `sha256sum` of both
ELF files in the evidence folder. Build a given trial from a clean checkout of the reviewed SHA;
do not edit the sources between the build and the run.

Optional parameters (environment variables read at build time, decimal): see the header comments of
each firmware. P4: `SDIO_A1_RESET_HOLD_MS` (100), `SDIO_A1_BOOT_WAIT_MS` (2000),
`SDIO_A1_CMD5_TRIES` (10), `SDIO_A1_CMD5_RETRY_MS` (200), `SDIO_A1_ROUNDS` (1),
`SDIO_A1_ROUND_GAP_MS` (500), `SDIO_A1_CLOCK_HZ` (400000). C6: `SDIO_A1_MONITOR_MS` (500),
`SDIO_A1_ALIVE_MS` (10000). The values in use are printed in the `PARAMS` and `BOOT` lines.

## 2. Hardware

### SDIO bus (wired on the board)

| Signal | ESP32-P4 | ESP32-C6 |
|---|---|---|
| CLK | GPIO18 | GPIO19 |
| CMD | GPIO19 | GPIO18 |
| DATA0 | GPIO14 | GPIO20 |
| DATA1 | GPIO15 | GPIO21 |
| DATA2 | GPIO16 | GPIO22 |
| DATA3 | GPIO17 | GPIO23 |
| C6 reset | GPIO54 | EN / CHIP_PU |

This bus is not the microSD socket. The P4 side uses **SDMMC slot 1**, routed by the GPIO matrix
(`esp-hal` `sdmmc` driver: slot 1 accepts any pin; slot 0 is IO_MUX-fixed on GPIO39..44). The C6
side uses the fixed IO MUX function 0 pads.

### Facts taken from the code, to check against the schematic

- `SdHostController::new` runs a P4 chip setup that switches on-chip LDO channel 4 to 3.3 V. The
  driver comments describe it as the supply of the SD pads GPIO39..44 and of the card on the
  reference board. **Whether the pads GPIO14..19 are in a domain that this LDO powers on the
  Waveshare board is not known.** Check the pad supply of GPIO14..19, and what else is connected to
  the LDO 4 output (for instance the microSD socket).
- The host driver enables the internal pull-up on CMD and DATA0..3 of a matrix slot. The C6 pads
  reset with pull-up enabled and the driver does not change them. Whether the board adds external
  pull-ups is not known.
- The first CMD5 relies on the C6 having booted. The firmware only waits for a timer.

### C6 console (UART0 on connector H7)

- An operator-supplied CP2102 is on H7. The correspondence between H7 pins and the C6 UART0 pins
  (GPIO16 TX, GPIO17 RX), and the voltage level (3.3 V), must be read on the Waveshare schematic
  **before connecting**. Not verified here. Cross TX and RX, join the grounds, never connect a 5 V
  pin to the C6.
- 115200 baud, 8N1, no flow control.
- GPIO9 of the C6 is wired to GND to enter the ROM download mode. It must be released for normal
  runs (see recipe R0). The P4 cannot drive GPIO9.

### P4 console

`esp-println/uart` prints on UART0 of the P4. The path from the P4 UART0 to a host serial port on
this board (on-board USB bridge or header) is **not verified here**; read it on the schematic.

## 3. Programming procedure (not executed)

Port names below are placeholders. The programming tool is the operator's choice (`espflash`
or the repository `cargo xtask run` flow); neither was tried against this board.

1. **Programming the C6.**
   1. Keep GPIO9 of the C6 at GND. Make sure the P4 is not running the A1 firmware, so that it does
      not drive EN: leave it unprogrammed or in its default state. The state of GPIO54 after a P4
      reset is **not known**; check EN with a meter before programming.
   2. Reset the C6 (EN button if the board has one, otherwise a power cycle). Strapping is sampled
      at reset, so GPIO9 must be low at that moment.
   3. Flash the C6 ELF over the CP2102:
      `espflash flash --chip esp32c6 --port <C6_PORT> target/riscv32imac-unknown-none-elf/release/sdio-a1-c6-slave`.
   4. Remove the GPIO9 → GND link.
   5. Reset the C6 again and open the C6 console. Expect the `BOOT` line of section 4.
2. **Programming the P4.** Flash the P4 ELF through its own programming path:
   `espflash flash --chip esp32p4 --port <P4_PORT> target/riscv32imafc-unknown-none-elf/release/sdio-a1-p4-host`.
   Open the P4 console before releasing the P4 reset, so that the start-up lines are not lost.
3. **Run.** The P4 firmware drives GPIO54 low immediately, then runs the campaign, then parks. The
   C6 firmware stays in its monitor loop. Both logs go to the evidence folder, one file per run.

## 4. Log contract

Every line: `SDIO-A1 <MCU> <STEP> <RESULT> run=<id> <details>`. `RESULT` is `PASS`, `FAIL`,
`BLOCKED` or `INFO`. `BOOT` lines carry the source SHA and the firmware name. A `PASS` line is an
observation made by that firmware on its own side; lines about the reset pin state that the
firmware commanded, and the boot wait, say so in their text.

C6 steps: `BOOT`, `DRIVER_NEW`, `SNAPSHOT_PRE`, `IOREADY_PRE`, `ENABLE_BUS`, `STATE`,
`SNAPSHOT_POST`, `IOREADY_POST`, `FUNCTIONS`, `LISTENING`, then `CHANGE` / `CHANGE_REGS` on a
register change, `IOREADY_VIOLATION` (FAIL) if a function-ready bit ever rises, and `ALIVE`.

P4 steps: `BOOT`, `PARAMS`, `HOST_INIT`, `ROUND_START`, `RESET_ASSERTED`, `RESET_RELEASED`,
`BOOT_WAIT`, `INIT_CLOCKS`, `CMD0`, `CMD5_TRY`, `CMD5_INQUIRY`, `CMD5`, `CMD5_FUNCTIONS`, `CMD3`,
`CMD7`, `CCCR_READ`, `CCCR_VERSION`, `CCCR_IO`, `CCCR_IOR`, `CCCR_TRM`, `CCCR_CAPS`, `FBR_READ`,
`FBR_TRM`, `CIS_FN0`, `CIS_FN1`, `CIS_FN2`, `REMAINING` (BLOCKED), `ROUND_SUMMARY`,
`CAMPAIGN_SUMMARY`. The summaries say `host_side=` and never claim a global result.

## 5. Recipes

Each recipe uses a new run identifier and a new evidence folder. A failed recipe is not repeated
with modified sources: a change goes through review first.

| ID | Precondition | Action | Expected observation | Evidence | PASS / FAIL / BLOCKED |
|---|---|---|---|---|---|
| **R0** Setup | Both ELF files built from the reviewed SHA; schematic read for H7 mapping, pad supply of GPIO14..19, GPIO54, P4 console path. | Record `sha256sum` of the ELF files. Check board wiring against section 2. | Wiring as in section 2. | Hashes, photo or note of the wiring, schematic references. | PASS if all checks done. BLOCKED if the H7 mapping, the pad supply or the P4 console path cannot be established. |
| **R1** C6 boots | C6 programmed, GPIO9 link removed, console open, P4 not driving EN. | Reset the C6. | `BOOT INFO` with the SHA; `DRIVER_NEW PASS`; `SNAPSHOT_PRE`; `IOREADY_PRE PASS`; `ENABLE_BUS PASS`; `STATE PASS`; `IOREADY_POST PASS`; `LISTENING`; then `ALIVE` lines. | C6 console log. | PASS if all those lines appear with the right SHA and `ioready1=0 ioready2=0`. FAIL on any FAIL line, on a wrong SHA or a panic. BLOCKED if there is no console output (wiring, level, baud) — then fix R0 first. |
| **R2** EN control | R1 PASS. P4 programmed. Meter or scope on C6 EN. | Reset the P4 and let it run the first round. | EN low at `RESET_ASSERTED`, high at `RESET_RELEASED` (measured); the C6 console shows a fresh `BOOT` shortly after `RESET_RELEASED`. | P4 log, C6 log, scope or meter capture of EN. | PASS only with a measured EN edge **and** a new C6 `BOOT`. The P4 lines alone are not enough. FAIL if EN does not follow, or the C6 does not reboot. BLOCKED if EN cannot be probed. |
| **R3** Host init | R2 PASS. | Read the P4 log. | `HOST_INIT PASS`, then `INIT_CLOCKS PASS`. Scope on P4 GPIO18: about 400 kHz clock. | P4 log, scope capture. | PASS if both lines and a clock are seen. FAIL on a controller, slot or clock error. BLOCKED if the host does not start (check the LDO 4 and pad supply notes). |
| **R4** CMD0 / CMD5 | R3 PASS. | Read `CMD0`, `CMD5_TRY`, `CMD5_INQUIRY`, `CMD5`, `CMD5_FUNCTIONS`. | `CMD5 PASS` with `ready=1`, `s18a=0` offered (never requested), `functions=2` (TRM 34.2). `memory=0` expected for an SDIO-only card. | P4 log, scope on CMD if failing. | PASS if `CMD5 PASS`. `CMD5_FUNCTIONS` FAIL if the count is not 2. FAIL with `no_response`, `invalid_response` or `response_crc`: keep the cause. If CMD5 does not answer, run R9 before anything else. |
| **R5** CMD3 | R4 PASS. | Read `CMD3`. | `CMD3 PASS` with a non-zero RCA and no error flag. | P4 log. | PASS / FAIL per the line. BLOCKED if R4 did not pass (the P4 prints `REMAINING BLOCKED`). |
| **R6** CMD7 | R5 PASS. | Read `CMD7`. | `CMD7 PASS`, a response was received. The selection state itself is not decoded: the SDIO R1 layout is not confirmed. | P4 log. | PASS / FAIL per the line. BLOCKED if R5 did not pass. |
| **R7** CCCR | R6 PASS. | Read `CCCR_READ`, `CCCR_VERSION`, `CCCR_IO`, `CCCR_IOR`, `CCCR_TRM`, `CCCR_CAPS`. | `CCCR_READ PASS`; `CCCR_IOR PASS` (IOR1 = IOR2 = 0); `CCCR_IO` shows `ioe=0x00`. `CCCR_TRM`: expected `deviates=0` (cis_ptr 0x001000, card_cap 0x17 once E4MI is ignored, SCSI set). `CCCR_CAPS` is information: capabilities advertised but not implemented. | P4 log. | PASS if `CCCR_READ` and `CCCR_IOR` pass. FAIL if a mandatory byte (`0x00..=0x13`) cannot be read, or `IOR1/2` read non-zero. A `CCCR_TRM` deviation is reported to the reviewer and is not by itself a FAIL. |
| **R8** FBR and CIS | R7 PASS. | Read `FBR_READ` for functions 1 and 2, `CIS_FN0`, `CIS_FN1`, `CIS_FN2`. | `FBR_READ PASS` with `cis_ptr` 0x001100 and 0x001200. For each CIS: tuples listed, `end=EndTuple` or `LinkTerminator`, `reason=well_formed`. The walk is bounded at 16 tuples and 256 bytes. | P4 log (all tuple lines). | PASS per CIS if `well_formed`. FAIL on `empty_cis`, a limit reached, or a read error. **An `empty_cis` is the expected symptom of the open point on the reset CIS content (`HINF_CIS_CONF_Wn` = 0xffffffff); report it with the raw lines, do not mask it.** |
| **R9** CMD fault finding | CMD5 gives no answer. | Probe C6 GPIO18 (CMD) and GPIO20..23 with a scope during a CMD5 try; also CLK on C6 GPIO19. | The P4 command frame is visible on the line; the C6 answer (R4) follows. | Scope captures of CLK, CMD during `CMD5_TRY`; C6 console (still alive, `ALIVE` lines). | Diagnostic recipe, no verdict of its own. Classify: command frame absent (P4 side or wiring); frame present and the line stays high-impedance (C6 output enable, an open point of `docs/esp32c6-sdio-slave-bringup.md`); answer present but corrupted (timing, sampling edge, pull-ups). |
| **R10** IOREADY stays 0 | R1 and R7 PASS. | Let the campaign run, then keep the C6 monitor running for 10 minutes. | No `IOREADY_VIOLATION`; `IOREADY_POST PASS`; the P4 `CCCR_IOR PASS`; any `CHANGE` line shows `function1=Disabled function2=Disabled` (A1 enables nothing). | C6 log, P4 log. | PASS if both sides agree for the whole period. FAIL on any `IOREADY_VIOLATION` or `IOR` read as set. |
| **R11** Reset cycles and recovery | R2 and R4 PASS. | Rebuild the P4 ELF with `SDIO_A1_ROUNDS=5` (new run id) and run it. | Five rounds, each with `RESET_ASSERTED`, `RESET_RELEASED`, a fresh C6 `BOOT` and `CMD5 PASS`. `ROUND_SUMMARY` per round. | P4 log, C6 log (five `BOOT`), scope on EN. | PASS if five of five rounds pass. FAIL if a round fails after the first success (record which and why). BLOCKED if the first round already fails. |
| **R12** Late C6 | R4 PASS. | Run with `SDIO_A1_BOOT_WAIT_MS=0` and `SDIO_A1_CMD5_TRIES=15`. | The first `CMD5_TRY` lines time out (`no_response`) until the C6 has booted, then a valid R4 appears. | P4 log, C6 log. | PASS if the first CMD5 try fails or succeeds and the campaign recovers within the tries. FAIL if it never recovers. This checks the retry bound; it does not prove the boot time. |

### Stop and resume

- Stop at the first FAIL of R1 to R8. Keep the logs; do not edit the sources. If a cause is found,
  open a change, review it, then run again with a **new** run identifier and folder.
- Never flash or reset to "see if it works again" without recording the first result.
- If the C6 console is silent, stop and fix the console path (R0, R1) before judging the bus.
- If a recipe is BLOCKED, say which prerequisite is missing; do not downgrade it to PASS.
- Resuming after an interruption starts from R0 with a new run identifier.

## 6. Electrical and board points to establish (before or during R0)

1. Pad supply and level of P4 GPIO14..19 and of C6 GPIO18..23 (3.3 V signalling expected; the
   campaign never requests 1.8 V).
2. External pull-ups on CMD and DATA0..3, if any.
3. State of P4 GPIO54 after reset, and the C6 EN circuit (RC, button).
4. What the on-chip LDO 4 output feeds on this board.
5. Strapping of C6 GPIO4 (sampling edge) and GPIO5 (driving edge): they set the default sampling
   and driving edges (TRM 34.5.6). The driver does not change them.
6. H7 pin mapping and voltage.

## 7. Establishing the verdict

The firmwares do not print a global verdict. The verdict is built from the two logs of the same
run identifier:

| Element | Source | Requirement |
|---|---|---|
| C6 started the driver | C6 log | `ENABLE_BUS PASS`, `STATE PASS`, `IOREADY_PRE/POST PASS`, no `IOREADY_VIOLATION`, SHA matches. |
| EN control | P4 log + measurement | R2 PASS. |
| Enumeration | P4 log | R4 to R8 PASS. |
| `IOR1/IOR2` at 0 | both logs | R7 and R10 PASS. |
| Reset and recovery | both logs | R11 PASS. |

A1 is qualified on hardware only when every row holds for the same SHA. Any BLOCKED or FAIL row
keeps A1 open. A software pass (compilation, host tests) is never a substitute.

## 8. Known limits of this preparation

- No hardware observation exists. Every `PASS` condition above is expected behaviour, not a result.
- The SD/SDIO specifications were not available: R4/R6/R5 field layouts and the R1 form for CMD7
  come from the `sdio` crate and from the specification as remembered, and are marked **[SPEC]** in
  `examples/qa/src/sdio_a1.rs`.
- `CMD7` is sent with the `sdio` crate command, whose response type is R1 without busy wait; the
  SDIO specification may require R1b. The result is reported as seen.
- The host driver is blocking-only for these commands. It was not modified; it has no UHS support
  (`esp-hal` `sdmmc` documentation), which is what A1 needs.
- The reset timings are parameters, not datasheet values: no ESP32-C6 datasheet figure was
  consulted for the minimum EN low time or the boot time.
- Whether the C6 drives CMD and DATA with the current pad configuration is an open point (output
  enable of IO MUX function 0), settled only by R4 or R9.
