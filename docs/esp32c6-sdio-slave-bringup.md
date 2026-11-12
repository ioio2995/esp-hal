# ESP32-C6 SDIO Slave — Native Rust bring-up

Status: **foundation only; not an operational SDIO Slave driver**.

## Goals

Implement the ESP32-C6 SDIO Slave controller using the technical reference
manual, `esp-pacs` register definitions, and `esp-hal` infrastructure.
The implementation must not use ESP-IDF, C FFI, or copied ESP-IDF driver logic.
The SDIO transport is hardware-specific; RPC, Zenoh, remote TCP, provisioning,
and network services are **not** part of this driver.

## First patch (this branch)

- Expose the C6 SLC block and SLC0/SLC1 interrupt sources to esp-hal metadata.
- Add a C6-only native module holding HINF, SLC, SLCHOST peripheral tokens.
- Expose a **read-only** register snapshot for diagnostics.
- Do not enable IO_READY, pins, interrupts, or DMA without verified sequencing.
- No hardware qualification or SDIO transfer has been performed.

## Required A1 work before an operational driver

1. Validate peripheral metadata generation and compile with
   `--features esp32c6,unstable`.
2. Confirm the SDIO pin mapping and the fixed IO-MUX functions against the C6
   TRM and the actual board schematic (not assumptions from P4 GPIO routing).
3. Implement clock/reset ownership in the esp-hal peripheral clock machinery;
   verify correct access to the SLC, HINF, and SLCHOST blocks.
4. Define the SDIO function-0 configuration registers, initialization sequence,
   function readiness, interrupt enable/acknowledgement and teardown invariants.
5. Add tests proving that no IO_READY state is advertised before DMA and RX/TX
   buffers have been installed.

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
