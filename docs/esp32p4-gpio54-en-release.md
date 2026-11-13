# ESP32-P4 firmware: release and hold the ESP32-C6 EN line (GPIO54)

Status: **compiled and checked in software, not flashed.** Nothing in this file is an observation of the board. The firmware is independent of the SDIO work: it starts no SDMMC, speaks no SDIO and never resets the C6.

Source: `examples/qa/src/bin/p4-gpio54-en-release.rs`, pure logic in `examples/qa/src/p4_en.rs` (host tests in `esp-hal-host-tests`).

## What the board connects (Waveshare ESP32-P4-WIFI6-POE-ETH schematic, published PDF)

Read from the netlist of the PDF; the physical board has **not** been probed.

| Net | Parts on it |
|---|---|
| `GPIO54` | P4 U7 pin 98, **R54 (0 ohm)** pin 2, expansion header P6 pin 31 |
| `C6_CHIP_PU` | R54 pin 1, **C6-MINI-1 U1 pin 8 (EN)**, **R13 (10 kohm)** pin 1, **C6 (1 uF)** pin 2 |

R13 pin 2 is on `ESP_3V3`, the 1 uF capacitor pin 1 on ground. So EN has a 10 kohm pull-up to 3V3 and a 1 uF capacitor to ground (time constant about 10 ms), and GPIO54 reaches it through a 0 ohm resistor. No transistor or inverter is on the path.

The same schematic shows that the C6 has **no DTR/RTS line** on connector H7 (H7: pin 1 = C6 TXD, pin 2 = C6 RXD, pin 4 = GPIO9), and that the P4 has its own DTR/RTS auto-reset circuit (U6, to `ESP_EN` and GPIO35). This supports the manual loader entry of the C6 and an automatic one for the P4, as inferences from the drawing.

One difference to note: the schematic names the C6 module `ESP32-C6-MINI-1-N4` (4 MB flash), while the bench identified 8 MB on the C6. The schematic may not match the assembled revision. Not investigated here.

## Polarity and levels
- EN of the C6 is **active high**: HIGH enables the chip, LOW holds it in reset.
- **Release = GPIO54 HIGH.** The firmware never commands LOW.

## GPIO54 at the P4 side (ESP32-P4 TRM, table 10.14-1)
- Function 0 and 1 = GPIO54, function 3 = `GMAC_PHY_RXER_PAD`; analog functions exist; drive strength 2 (about 20 mA).
- **Reset state 0**: input buffer disabled, high impedance. No mention of GPIO54 as a strapping pin (the TRM names GPIO34 for chip boot control).
- Consequence: whenever the P4 is in reset, being programmed, or has not started this firmware, GPIO54 is high impedance and **R13 pulls EN high: the C6 is released by the pull-up**, not held. This firmware makes the release explicit and keeps it while it runs. It does not hold the C6 across a P4 reset or reprogramming.
- Holding LOW would be a push-pull drive against the 10 kohm pull-up and the 1 uF capacitor: it holds the C6 in reset. This firmware never does it.

## Sequence
1. `Flex::new(GPIO54)`: pad in a known GPIO state, output driver disabled, pad hold released.
2. Write the output level HIGH **before** enabling the driver, so the pin goes from high impedance to HIGH and never through LOW.
3. Push-pull, no internal pull. Enable the input buffer to read the pad.
4. Read back the output register and the pad, print them, then hold. Nothing is written again, no periodic reset.

Push-pull was chosen because EN already has an external pull-up: open-drain would give the same release level but a slower rise through R13 into 1 uF.

## Expected console lines (UART0 of the P4, 115200 baud)
```text
P4-EN BOOT INFO sha=<sha> fw=p4-gpio54-en-release scope=gpio54-only resets=none hold_ms=10000
P4-EN GPIO54_INIT PASS pin=54 mode=push-pull pull=none pad_held=0 (configuration calls completed)
P4-EN EN_RELEASE_COMMANDED PASS level=HIGH output_register=HIGH (commanded; the C6 state is not observed)
P4-EN PAD_READBACK PASS pad=HIGH commanded=HIGH (P4 pad only; not a measurement of C6 EN)
P4-EN HOLD INFO level=HIGH tick=1 pad=HIGH (P4 pad only; not a measurement of C6 EN)
```
These lines describe operations of the P4 side only. `PASS` means the register or the pad read back as commanded. They never say that the C6 started or that EN was measured. `PAD_READBACK FAIL` or `OUTPUT_CHANGED FAIL` would mean that something else drives the pad or that the register changed: an anomaly of the P4 side.

## Build and software checks (done)
```sh
P4_EN_GIT_SHA=$(git rev-parse --short=9 HEAD) cargo xtask build qa esp32p4 p4-gpio54-en-release
cargo +nightly fmt --all -- --check
cargo xtask host-tests esp-hal-host-tests
cd examples/qa && cargo clippy --release --target riscv32imafc-unknown-none-elf --features esp32p4,esp-println/uart --bin p4-gpio54-en-release --lib
```
Target `riscv32imafc-unknown-none-elf`; ELF in `target/riscv32imafc-unknown-none-elf/release/p4-gpio54-en-release`. Built from commit `c55d10b0a` (the firmware commit): ELF sha256 `e406273d14ac0023583d7ddd01948405fa1f495f312ef5c3c2b6b49c92c0ed4d`.

## Image for a later programming (offline, nothing flashed)
**Correction (flash contents).** An earlier version of this section assumed that the P4 held no valid partition table and
proposed a merged image written over `0x0` to `0x1F000`. That was wrong. A read-only reference backup of the board shows a
valid factory partition table at 0x8000 (`nvsfactory` 0x9000/200K, `nvs` 0x3b000/840K, `otadata` 0x10d000, `phy_init`
0x10f000, `factory` 0x110000/9M, `storage` 0xa10000/5M) and an ESP-IDF bootloader at 0x2000. A merged image or an image
at 0x10000 would replace the table and overwrite `nvsfactory`/`nvs`. **Do not use `--merge`, and do not write at 0x0 or
0x10000.**

Correct approach: build an application-only image for the `factory` partition and write it at **0x110000**, keeping the
factory bootloader and table. The procedure, the table and the compatibility analysis are in
`docs/esp32p4-waveshare-flash-layout.md` of the SDIO slave branch (`feat/esp32c6-sdio-slave`):
```sh
espflash save-image --chip esp32p4 --flash-mode dio --flash-size 32mb --flash-freq 80mhz \
  --partition-table <waveshare-p4-factory.csv> --target-app-partition factory -S <elf> p4-gpio54-en-app.bin
```
The earlier merged image (`927a62c5...`, region 0x0 to 0x1F000) is superseded and must not be programmed. A verified backup
and the restoration contract still come first.

## Programming and measurement procedure (NOT authorised, NOT executed)
All hardware actions go through IOBEWI Device Lab, with explicit operator consent for each scenario.
1. Preconditions: the P4 read problem is understood and its workaround verified (`rust_dev` `bench/docs/p4-flash-read-diagnostic.md`); full P4 backup verified (`p4-full-backup.md`); persistent evidence root; restoration contract accepted.
2. Operator-approved write region 0x110000 (whole 4 KiB sectors covering the application-only image), the image file pinned by its SHA-256, grant, destructive consent per command.
3. Programming and independent readback through the `PC-CYCLE` mechanism, with restoration available.
4. Observe the P4 console for the lines above.
5. Electrical check by the operator, outside the bench: with a meter or scope on the C6 EN pin (or the R54/R13 node), expect a steady high level (about 3.3 V) and no low pulse while the firmware runs. Record the capture. Without this measurement the electrical level is **not** established, whatever the console says.
6. Observe what happens at EN when the P4 is reset: expected high impedance on GPIO54 and EN kept high by R13. This is an expectation from the drawing and the TRM, to be confirmed.

Nothing in this procedure requires the C6 to be programmed or probed over its UART.
