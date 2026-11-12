# ESP32-C6 SDIO Slave — Native Rust bring-up

Status: **foundation only; not an operational SDIO Slave driver**.

## Goals

Implement the ESP32-C6 SDIO Slave controller using the technical reference
manual, `esp-pacs` register definitions, and `esp-hal` infrastructure.
The implementation must not use ESP-IDF, C FFI, or copied ESP-IDF driver logic.
The SDIO transport is hardware-specific; RPC, Zenoh, remote TCP, provisioning,
and network services are **not** part of this driver.

## A1 contract — controller bring-up (implemented, not hardware-qualified)

Source: ESP32-C6 TRM v1.2, chapter 34 "SDIO Slave Controller", chapter 7
(IO MUX) and chapter 8 (PCR). ESP-IDF is not used. Items marked **[OPEN]** are
not settled by the TRM and must be settled on hardware.

### Scope

Clock, reset, pad routing, function-ready bits and safe diagnostics. Out of
scope: DMA, linked lists, interrupts, `IORx`, UHS/1.8 V, LDO, Embassy, Zenoh and
any network service.

### States

| Notion | Meaning | Represented by |
|---|---|---|
| Controller initialised | Block clocked and reset, pads not routed | `State::ControllerReady` |
| Card enumerable | Pads routed, `IOREADY1/2` = 0 | `State::BusEnumerable` |
| Function enabled by host | CCCR `IOEx` = 1, read-only for the slave | `FunctionState::EnabledByHost` |
| Function ready | CCCR `IORx` = 1 | `FunctionState::Ready`, **never set by A1** |

`HINF_SDIO_IOREADY1/2` (`HINF_CFG_DATA1_REG`, bits 1 and 5) drive CCCR `IOR1/2`
and the "function ready" field of the CIS. Reset value 0. A1 clears them and
never sets them; only a later DMA lot may, once RX/TX buffers are installed.

### Facts from the TRM

- `PCR_SDIO_SLAVE_CONF_REG` (0x00B0): `CLK_EN` bit 0, reset **1**; `RST_EN`
  bit 1, reset 0. The existing `PeripheralClockControl` already handles
  `Peripheral::SdioSlave` (reference counted, reset on first use).
- `HINF_CFG_DATA1_REG` reset `0x0023_2011`: `SDIO_VER` = 0x232, `CD_ENABLE` = 1,
  `IOREADY1/2` = 0, `HIGHSPEED_ENABLE` = 0. `IOENABLE1/2` are read-only status.
- `HINF_CIS_CONF_Wn_REG` reset `0xffff_ffff`; device/user IDs reset to
  0x6666/0x92 (function 1) and 0x7777/0x92 (function 2). A1 keeps them.
- SDIO pads: IO MUX function 0 of GPIO18 CMD, GPIO19 CLK, GPIO20..23 DATA0..3.
  Their reset pad state is input enabled with pull-up. The GPIO matrix cannot be
  used for them.
- Sampling and driving edges default to the MTMS (GPIO4) and MTDI (GPIO5)
  strapping values; `SLCHOST_CONF_REG` (0x1F0) overrides them. A1 keeps the
  strapping default.
- Masked interrupt status reads 0 while interrupts are disabled, so diagnostics
  read the raw registers (`SLC0INT_RAW`, `SLC1INT_RAW`).
- The TRM marks SLC fields "please initialize to 1/0 and do not modify"
  (`*_RX_NO_RESTART_CLR`, `*_TOKEN_NO_REPLACE`, `*_TOKEN_AUTO_CLR`,
  `SDIO_CMD_HOLD_EN`, `*_STITCH_EN`, `SLC0_LEN_AUTO_CLR`). They configure the DMA
  engine, so they belong to the DMA lot (A2), not A1.

### Sequence

1. `SdioSlave::new`: take `HINF`, `SLC`, `SLCHOST`, the six pins and the clock
   guard. No pad is routed, no function register is written.
2. `enable_bus`: clear `IOREADY1/2` by read-modify-write, then select IO MUX
   function 0 and input enable on the six pads. State becomes `BusEnumerable`.
3. `disable_bus`: pads back to plain GPIO. State becomes `ControllerReady`.

Registers with non-zero reserved reset bits are only changed by read-modify-write.
`HINF_SDIO_RST` is not used: the PCR reset already resets the block.

### Lifecycle

`PeripheralGuard` is the last field, so the clock outlives everything else.
`Drop` gives the pads back when they are routed, then releases the clock. This
is not a full hardware shutdown once DMA exists: the DMA lot must stop and drain
the engine before the clock guard is released. Several guards may share the
clock (reference count).

### Invariants

1. `new` does not route pads and does not write `HINF`/`SLC`/`SLCHOST`.
2. No code path sets `IOREADY1/2`, writes `*_LINK`, `*_LINK_ADDR` or
   `*SHAREMEM*`, or touches an interrupt enable.
3. `snapshot()` is always valid: the clock guard lives as long as the value.
4. Diagnostics never clear or acknowledge anything.

### Open items on hardware

- **[OPEN]** Whether the reset CIS content (`HINF_CIS_CONF_Wn` = 0xffffffff) and
  the IDs are sufficient for a P4 host to enumerate. The TRM does not give the
  required CIS content.
- **[OPEN]** Pad output enable. The TRM says the output enable of an IO MUX pad
  comes from the GPIO matrix `OEN_SEL`; whether the SDIO output signals drive the
  pads after `init`-free function selection is not stated.
- **[OPEN]** Whether the SLC "initialize to" values of the previous list are
  needed for enumeration (assumed not, as they configure the DMA engine).
- **[OPEN]** Reset behaviour of `SLCHOST` registers (the PAC has no field
  descriptions).
- Electrical prerequisites: VDDPST voltage, host pull-ups, strapping of GPIO4/5.

### Software acceptance

Compile for `riscv32imac-unknown-none-elf` with and without `unstable`, fmt,
clippy, `xtask check`, rustdoc, and the host tests of `state.rs` in
`esp-hal-host-tests`. A software pass is not a hardware pass.

### Hardware acceptance (not performed)

Read `HINF_CONF_STATUS` and the raw SLC interrupts after `enable_bus` without a
hang; CMD5 / CCCR / CIS enumeration by the P4; `IOR1/2` read back as 0;
`disable_bus` and `Drop` restore the pads.

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
