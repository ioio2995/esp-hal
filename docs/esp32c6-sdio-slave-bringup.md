# ESP32-C6 SDIO Slave — Native Rust bring-up

Status: **foundation only; not an operational SDIO Slave driver**.

## Goals

Implement the ESP32-C6 SDIO Slave controller using the technical reference
manual, `esp-pacs` register definitions, and `esp-hal` infrastructure.
The implementation must not use ESP-IDF, C FFI, or copied ESP-IDF driver logic.
The SDIO transport is hardware-specific; RPC, Zenoh, remote TCP, provisioning,
and network services are **not** part of this driver.

## A1 contract — controller bring-up (implemented, not hardware-qualified)

Sources: ESP32-C6 TRM v1.2 chapter 34 (SDIO Slave Controller), chapter 7 (IO MUX
and GPIO matrix) and chapter 8 (PCR); `esp-pacs` rev `61f9678` (field
descriptions). The SD / SDIO specifications were **not** available while writing
this: statements that depend on them are marked **[SPEC]**. Items marked
**[OPEN]** cannot be settled from the documents and need hardware. ESP-IDF is
not used.

### Scope

Clock, reset, pad routing, function-ready bits and safe diagnostics. Out of
scope: DMA, linked lists, interrupts, `IORx`, UHS / 1.8 V, LDO, Embassy, Zenoh
and any network service.

### States

| Notion | Meaning | Represented by |
|---|---|---|
| Controller initialised | Block clocked and reset, pads not routed | `State::ControllerReady` |
| Bus exposed | Clock on, reset released, pads routed to the controller, `IOREADY1/2` = 0 | `State::BusExposed` |
| Function enabled by host | CCCR `IOEx` = 1, read-only for the slave | `FunctionState::EnabledByHost` |
| Function ready | CCCR `IORx` = 1 | `FunctionState::Ready`, **never set by A1** |

`BusExposed` was `BusEnumerable` in the first A1 commit. It was renamed because
it only describes what the driver configured: no hardware evidence yet shows that
a host can run CMD5 or read CCCR/CIS.

### Verified in the TRM

- **Clock/reset.** `PCR_SDIO_SLAVE_CONF_REG` (0x00B0, §8): `CLK_EN` bit 0 reset 1,
  `RST_EN` bit 1 reset 0. `esp-hal` gates peripherals that have no owner at start
  (`disable_peripherals`), so the clock is requested explicitly through
  `PeripheralGuard::new(Peripheral::SdioSlave)`, which enables the clock and
  resets the block on first use (reference counted).
- **No SDIO signal in the GPIO matrix.** Table 7.11-1 lists only
  `sdio_tohost_int_out` (output signal 69); no `SDIO_CMD`, `SDIO_CLK` or
  `SDIO_DATAn` appears among the matrix signals. The only route to the pads is
  IO MUX function 0 of GPIO18..GPIO23 (Table 7.12-1).
- **Pad reset state.** GPIO18..23 reset to IE = 1, WPU = 1 (Table 7.12-1,
  "Reset" column 3), drive strength 2.
- **GPIO-matrix output enable.** §7.5.2 and §7.11: for signals routed through
  the matrix, the pad output enable is `GPIO_FUNCn_OEN_SEL` (reset 0: peripheral
  `*_oe` signal) or `GPIO_ENABLE_REG` (when `OEN_SEL` = 1). `GPIO_FUNCn_OUT_SEL`
  resets to 0x80 (plain GPIO). The TRM does not say how the output enable of a
  *direct IO MUX* function is generated.
- **CLK.** The TRM gives the slave no SDIO clock output. The driver never enables
  an output on GPIO19.
- **CCCR/FBR content.** Tables 34.5-1 and 34.5-2: CCCR is fixed except the fields
  linked to `HINF_CFG_DATA1` (`SDIO_VER`, `HIGHSPEED_ENABLE`); `IOR[2:1]` follow
  `HINF_SDIO_IOREADY1/2`; the common CIS pointer is 0x001000, function 1 CIS
  0x001100, function 2 CIS 0x001200.
- **CIS content that software can set** (§34.9.1): device/user ID of functions 1
  and 2 (`HINF_CFG_DATA0/16`, reset 0x6666/0x92 and 0x7777/0x92), CIS bytes 36..67
  (`HINF_CIS_CONF_Wn`, reset 0xffffffff), bytes 312/315/318/568/571/574
  (`HINF_CFG_DATA7` `CHIP_STATE`/`PIN_STATE`).
- **Sampling/driving edges.** Default from the GPIO4 and GPIO5 strapping values;
  `SLCHOST_CONF_REG` (0x1F0) overrides (§34.5.6). Not touched.
- **Interrupt status.** Masked status reads 0 while interrupts are disabled, so
  diagnostics read the raw registers.

### From the PAC (not stated in the TRM — treat as unverified)

- `HINF_CFG_DATA1` bit 0 `sdio_enable` ("sdio clock enable") resets to 1; TRM calls
  this bit reserved. Total reset value `0x0023_2011` matches both.
- `HINF_CFG_DATA7` resets to `0x2382_0000`: bit 17 `sdio_ioready0`
  ("sdio io ready") = 1, bit 22 `clk_en` ("apb clock for configuration force on",
  0 = gating) = 0, bits 23/24/25/29 (SDR50, SDR104, DDR50 and asynchronous
  interrupt capability in CCCR) = 1, bit 20 `sdio_switch_volt_sw` (0 = 3.3 V) = 0.
  The TRM shows the same reset value as reserved fields.

### Sequence and contract

`new`: takes `HINF`, `SLC`, `SLCHOST`, the six pins and the clock guard. No pad is
routed and no function register is written.

`enable_bus` (`ControllerReady` → `BusExposed`):

1. Clear `IOREADY1/2` by read-modify-write.
2. For each pad: `init_gpio()` (output disabled in `GPIO_ENABLE`, pad hold
   released, matrix output on plain GPIO, IO MUX on GPIO function, input off), so
   that no stale matrix or `GPIO_ENABLE` state can force an output or hold the pad;
   then select IO MUX function 0 and enable input.
3. Direction of CMD and DATA is left to the controller. The driver never sets
   `GPIO_ENABLE` or `OEN_SEL` after step 2. CLK is never an output.

`disable_bus` (`BusExposed` → `ControllerReady`):

1. Clear `IOREADY1/2` again, so that nothing is advertised as ready once the pads
   leave the bus.
2. For each pad: `init_gpio()`. Result: GPIO function, output disabled, input
   disabled, hold released. Pull resistors are **not** changed.
3. Not done: the previous pad configuration is **not** saved and **not**
   restored; the clock is kept; the block is not reset; no DMA is stopped (none
   exists).

`Drop`: if the pads are routed, the same two steps as `disable_bus`, then the
clock reference is released. The clock is gated only when the last reference
disappears (`PeripheralClockControl::disable`). The struct holds the clock guard
as its last field so it outlives the pads and registers. A DMA layer must stop
and drain its engine before both operations.

### Invariants

1. `new` does not route pads and does not write `HINF`/`SLC`/`SLCHOST`.
2. No code path sets `IOREADY1/2`, writes `*_LINK`, `*_LINK_ADDR` or
   `*SHAREMEM*`, or touches an interrupt enable.
3. `snapshot()` is always valid: the clock guard lives as long as the value.
   Diagnostics never clear or acknowledge anything. The snapshot also reads
   `HINF_CFG_DATA7` (function-0 ready, clock and capability bits).
4. The driver never forces a pad output enable.

### Enumeration analysis (CMD5, CCCR, FBR, CIS)

1. **Registers taking part in discovery.** The TRM documents no register for CMD5 /
   OCR handling: the command state machine is internal. The documented inputs are
   `HINF_CFG_DATA1` (`SDIO_VER`, `CD_ENABLE`, `IOREADY1/2`), `HINF_CFG_DATA0/16`
   (IDs), `HINF_CIS_CONF_Wn` and `HINF_CFG_DATA7`.
2. **Exposure of CCCR/FBR/CIS.** Generated by the controller from Tables 34.5-1/2
   and the registers above; CIS at 0x1000 / 0x1100 / 0x1200.
3. **Reset values sufficient?** **[OPEN]** The TRM states neither that they are
   sufficient nor what the base CIS tuples contain. `0xffffffff` in
   `HINF_CIS_CONF_Wn` may be the CISTPL_END marker **[SPEC]**, which would be a
   valid terminator, but this is not established.
4. **Program a CIS?** No. No normative content or identifier source exists in the
   documents available, and inventing tuples or IDs would be unjustified. The
   defaults are kept.
5. **SLC/SLCHOST before enumeration?** The TRM names no such step. The SLC "do not
   modify / initialise to" fields configure the DMA engine and are left for the DMA
   lot. **[OPEN]**
6. **Function 0 with `IOR1/IOR2` = 0?** Function 0 holds only CCCR, FBR and CIS
   (§34.5.3) and `IOR[2:1]` are independent read-only fields; `sdio_ioready0`
   (PAC) resets to 1. By structure the host can read function 0 **[SPEC]**, which
   is the intended enumeration path. Not demonstrated.

### Electrical and hardware prerequisites

- VDDPST2 (digital GPIO domain) voltage and the 3.3 V SDIO signalling level; the
  1.8 V switch (`sdio_switch_volt_sw`) is not used. The default CCCR advertises
  SDR50/SDR104/DDR50 capability (PAC reset bits): a host that attempts a
  1.8 V switch is **not** supported by A1.
- Pull-ups on CMD and DAT0..3 are expected from the host/board **[SPEC]**; the pads
  reset with WPU on and the driver does not change them. Board design must be
  checked, the driver does not guarantee them.
- Strapping of GPIO4 (sampling edge) and GPIO5 (driving edge); GPIO24..30 are
  used by in-package flash on some variants and are not SDIO pads.

### Open items on hardware

- **[OPEN]** Pad output enable for CMD/DAT in IO MUX function 0 (see above). If
  CMD/DAT stay high-impedance, the card will not answer; the corrective action
  would be a documented, minimal write and must be reviewed first.
- **[OPEN]** Enumeration with reset CIS/IDs, and whether the host sees the
  UHS-I capability bits as a problem.
- **[OPEN]** Whether the configuration APB clock gating (`clk_en` = 0, PAC) hinders
  HINF/SLC/SLCHOST register access or enumeration.
- **[OPEN]** Reset behaviour of `SLCHOST` (the PAC has no field descriptions).

### Software acceptance

Compile for `riscv32imac-unknown-none-elf` with and without `unstable`, and with
`defmt`; fmt; clippy; `xtask check`; rustdoc; host tests of `state.rs` in
`esp-hal-host-tests`. A software pass is not a hardware pass.

The firmwares and the operator recipe for this qualification are described in
`docs/esp32c6-sdio-a1-qualification.md` (prepared, not executed).

### Hardware qualification (not performed)

1. `enable_bus` then `snapshot()`: no hang; `cfg_data1` bits 0 and 4 set, bits 1 and
   5 clear; `cfg_data7` bit 17 as expected.
2. Scope CMD/DAT/CLK: CLK never driven by the C6; CMD/DAT driven only while
   answering.
3. P4 host: CMD5, CMD3/CMD7, CCCR and FBR reads, CIS walk (function 0), without
   enabling any function.
4. `IOR1/IOR2` read back as 0 at every step.
5. `disable_bus` and `Drop`: pads on plain GPIO with input and output disabled (pulls unchanged), `IOREADY1/2` still 0,
   repeated `enable_bus`/`disable_bus` cycles.

## Required A2–A4 work

- A2: DMA descriptors, accessible/aligned memory, buffer ownership, queue
  lifetime and cancellation, with zero C dependencies.
- A3: SLC0/SLC1 interrupt dispatch, wake-up of async tasks, DAT1 notification,
  full/empty and error handling.
- A4: async RX/TX API, bounded queues, restart behavior, data integrity tests,
  and actual P4 Host ↔ C6 Slave SDIO HIL tests.

## Hardware acceptance (not performed)

1. CMD5 enumeration + CCCR/FBR discovery by the P4 Host.
2. CMD52 read/write; CMD53 byte and block modes, 1-bit then 4-bit.
3. DMA RX/TX correctness across alignment and transfer-size boundaries.
4. DAT1 event notification and re-arming.
5. Concurrent RX/TX with flow control, resets of either MCU, and recovery.
6. Sustained throughput, latency, and CPU/RAM measurements.

Function 2 and dual-SLC scheduling are later optimizations. Their relationship
must be established from the TRM and measurements, not inferred from register
names alone.

## Primary references

- ESP32-C6 Technical Reference Manual:
  https://www.espressif.com/sites/default/files/documentation/esp32-c6_technical_reference_manual_en.pdf
- esp-pacs registers: `esp32c6/src/hinf.rs`, `slc.rs`, `slchost.rs`
- P4 SDMMC Host driver: `esp-hal/src/sdmmc/mod.rs`
