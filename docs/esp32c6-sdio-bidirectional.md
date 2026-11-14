# Single-packet SDIO: A2b, A3 and A4

This extension builds on the A2a ownership corrections in PR #3. It adds one
slave-to-host packet, SLC0 interrupt completion, and cancellable async waits.
The P4 uses the existing `SdHostController`; its A1/A2a path is retained.

No hardware test has been run for this extension. The reported A1/A2a success
does not qualify these changes. Host tests and Miri validate software invariants;
compilation validates the two firmware targets. Neither validates bus signals.

## Supported contract

- Function 1, incremental-address packets, SLC0, one descriptor per packet.
- One direction at a time, with exclusive static internal-DRAM storage.
- Buffers contain 1..=4095 words. Transmit length is 1..=buffer capacity in bytes.
- Blocking polling and async completion. There is no automatic rearming.
- Completion requires the direction's EOF, PARK, CPU owner, matching capacity,
  EOF in the descriptor, and an in-range length. Transmit length must also match
  the published length exactly. A DMA fault takes priority over EOF.
- `finish_receive()` and `finish_transmit()` confirm shutdown, return storage,
  and move to `BusExposed` without releasing pads. The next arm is explicit.
- Error or early cancellation uses `disable_bus()` and then the matching
  `take_*_buffers()`. A stop timeout retains storage and holds reset asserted.

SLC1/function 2, simultaneous full duplex, descriptor chains, automatic queues,
and host DAT1 notification handling are outside this single-packet API.
The P4 QA deliberately polls for availability at 400 kHz in 1-bit mode.

## A2b register sequence

The hardware reference is the ESP32-C6 TRM v1.2, sections 34.5.5, 34.6 and 34.7.1:
<https://documentation.espressif.com/esp32-c6_technical_reference_manual_en.pdf>.
Register and descriptor names below use the hardware's direction convention.
SLC0 **RX** sends from C6 to P4; SLC0 **TX** receives from P4.

1. Validate both allocations in internal DRAM before modifying the descriptor.
2. Set owner=DMA, eof=1, length=valid bytes, size=word-aligned capacity, next=0.
3. Configure RX_AUTO_WRBACK=1 and RX_LOOP_TEST=0 so write-back returns ownership.
   Preserve the required TOKEN_NO_REPLACE and RX_NO_RESTART_CLR initialization.
4. Reset the RX FSM and clear only its stale EOF/underflow/descriptor-fault bits.
5. Order CPU stores with the DMA fence, then write the full RXLINK address.
6. Add length with LEN_INC_MORE. Never overwrite the cumulative counter with a
   per-packet length. Order publication before RXLINK_START and function readiness.
7. P4 enables function 1 and waits for IOR1. It reads PKT_LEN at function-1 0x60.
   Available bytes are `(published - consumed) & 0xfffff`.
8. P4 reads one packet with CMD53, byte mode, incrementing address
   `0x1f800 - length`. The A2b test expects exactly 64 bytes `40..7f` and valid R5.
9. C6 validates RX_EOF, PARK and descriptor write-back after an acquire fence.
   Its result proves DMA completion, not host payload validation.

The standalone P4 test starts after a full C6 reset and initializes consumed=0.
This is not a reconnect/resume protocol. After aborting a published outbound
packet, restart the host/slave session: the driver does not retract cumulative
length or assume the host consumed it. Successful packet reuse preserves counters.

## SLC0 interrupts and async cancellation

`into_async()` binds SLC0. The single outstanding wait owns `&mut SdioSlave`, so
two waits or arms cannot run concurrently through the safe API. SLC1 is unused.

The ISR masks the SLC0 DMA notifications and wakes the registered task. It does
not clear RAW, validate descriptors, or return storage. The task registers its
waker before checking completion and enabling notifications. An event before
ENA remains latched; an event after ENA invokes the handler. If EOF precedes
PARK, the future requests another poll without re-enabling the level interrupt.
Only the task's completion/stop path acknowledges its direction's DMA events.
Host-vector and SLC1 status bits are not acknowledged.

`wait_received()` and `wait_transmitted()` work with Embassy. They have no
implicit deadline. `embassy_time::with_timeout` can cancel a wait. Dropping the
future only masks notification: DMA and its allocations remain owned. A later
wait can resume. `disable_bus()` performs explicit DMA cancellation. No automatic
replay, rearming, or buffer restitution occurs when an executor drops a future.

## Build-only checks

Run from the repository root. These commands build firmware; they do not flash it.

```sh
cargo xtask host-tests esp-hal-host-tests
cargo xtask check esp-hal esp32c6
cargo xtask lint esp-hal esp32c6
SDIO_A2=1 cargo xtask build qa sdio-a2-c6-slave esp32c6
SDIO_A2=2 cargo xtask build qa sdio-a2-c6-slave esp32c6
SDIO_A2=1 cargo xtask build qa sdio-a1-p4-host esp32p4
SDIO_A2=2 cargo xtask build qa sdio-a1-p4-host esp32p4
SDIO_A2=1 cargo xtask build qa sdio-a4-c6-slave esp32c6
SDIO_A2=2 cargo xtask build qa sdio-a4-c6-slave esp32c6
```

The async QA waits up to 30 seconds for host enable and 10 seconds for completion.
On timeout it explicitly disables the bus and reports whether shutdown succeeded.
It does not retry the packet or claim payload validation on the transmitting side.

## Remaining targeted hardware checks

These checks require a separately authorized session and source review:

1. One A2b packet: compare C6 descriptor EOF/owner/length against P4 R5 and all
   64 payload bytes. Observe cumulative length before and after host consumption.
2. Complete and explicitly rearm two packets without disconnecting pads; verify
   token and length increments, including switching direction between packets.
3. Check SLC0 IRQ delivery for both directions and no repeated interrupt after
   masking/acknowledgment. Observe EOF/PARK ordering.
4. Cancel an async wait before completion, resume it, and then check explicit
   DMA stop/restitution. Observe STOP/PARK, including reset fallback behavior.

Upstream submission still requires an independent review and hardware evidence
for the affected paths. This document does not declare hardware qualification.
