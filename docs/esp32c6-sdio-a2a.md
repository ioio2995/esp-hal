# ESP32-C6 SDIO A2a: one packet from P4 to C6

Status: A1 enumeration and one 64-byte A2a transfer were reported by the developer
for source `785357be3d2ab323e57f357dcc4d1e106b33b36d`. The corrections described
here have not been exercised on hardware. This document describes the A2a
receive path. The later [bidirectional extension](esp32c6-sdio-bidirectional.md)
adds A2b, SLC0 IRQ completion, and async waits; it is not hardware-qualified.

## Ownership and completion

`arm_receive` consumes one `&'static mut Descriptor` and one `&'static mut [u32]`.
On refusal, `ArmReceiveError::into_parts()` returns the error code and both
original allocations unchanged. Both complete allocations must be in internal DRAM. The buffer is non-empty,
word-aligned, and at most 16380 bytes. Static storage alone does not establish
DMA accessibility.

The driver owns raw pointers and retains the exclusive borrow contract. The
DMA-written descriptor word uses `UnsafeCell`; diagnostic reads use a volatile
raw pointer and do not form references to DMA-owned words. This does not make
the diagnostics a coherent multi-register snapshot.

A RISC-V `fence iorw, iorw` and compiler fences order ownership changes. Starting
the link follows descriptor publication. Packet access follows successful EOF,
a parked DMA, and an acquire barrier. Valid write-back requires owner=CPU,
eof=1, the configured size, and length no greater than capacity.

States are ControllerReady -> BusExposed -> Function1Receiving ->
Function1Received. DMA errors enter Function1ReceiveError. `received()` returns
`None` until completion; afterwards it exposes exactly the validated length.
`poll_received()` returns Result<Option<usize>, Error>, and reports completion
once. It never rearms the link. Readiness is withdrawn at completion or error.

## Stop and explicit reuse

`disable_bus()` withdraws IOREADY1/2 before stopping SLC0 TX. It polls PARK and
the self-clearing START/STOP command bits with a bounded loop, then resets the
receive FSM. If stopping does not produce idle, reset is asserted and another
bounded idle check is made. On timeout reset stays asserted, resources remain
owned, and DmaStopTimeout is returned. Pads and the clock are not released.

Only successful disable reaches ControllerReady. `take_receive_buffers()` can
then return the two exclusive static borrows once. The caller may explicitly
re-enable and arm again; the host must retain its consumed-credit count modulo
4096 (or both peers must explicitly resynchronize). There is no automatic queue
or flow-control API.

The bidirectional extension also adds `finish_receive()` after validated
completion. It confirms DMA shutdown and returns storage without disconnecting
the pads. A new arm remains explicit; no memory is returned on a stop timeout.

If Drop cannot establish idle, it retains the clock reference and does not
return the static storage. This deliberately fails closed; a timeout is not
reported as successful cancellation. This path and the STOP/PARK hardware
semantics still need a targeted hardware observation, not a full campaign.

## Register references

References: ESP32-C6 TRM v1.2 sections 34.5.5, 34.7.2 and registers 34.10/34.19;
esp-pacs `61f9678dd039f76d692aac19f42be8c0bf0457e7`.

The pinned PAC puts SLC0_TOKEN_NO_REPLACE at bit 1. TRM register 34.19 puts it at
bit 0; Espressif's C6 register header agrees. The local workaround sets only bit
0 using read-modify-write. Bit 1 and all other bits are preserved. No C driver
implementation is copied or linked.

SLC0 TX is host-to-slave; SLC0 RX is slave-to-host. Function 1 incremental CMD53
uses address 0x1F800 - payload length. A2a increments the cumulative token counter
by one receive credit after
preparing the link. The host starts from a reset C6, checks a nonzero token
credit, and sends exactly one packet.

## QA contract

Build the C6 binary `sdio-a2-c6-slave` and the P4 binary `sdio-a1-p4-host` with
`SDIO_A2=1`. P4 uses 1-bit, 400 kHz, byte-mode CMD53, function 1, incrementing
address 0x1F7C0, count 64. The pattern is A0..DF.

The P4 fails an IOR read error instead of treating it as a ready bit. It checks
receive credit before CMD53. The C6 reports RX_PACKET PASS only when the
validated received length is 64 and all bytes match. A host-side PASS alone
still does not prove the C6 contents.

Both firmwares print source SHA and run ID. Capture both logs for the same run;
no old log or result is rewritten by a code correction. Host tests and Miri
exercise ownership/validation with simulated write-back, not a real DMA.

## GPIO54 and P4 image

The A1/A2a host owns GPIO54 and commands C6 EN low then high for each round.
The separate `p4-gpio54-en-release` diagnostic holds HIGH and never pulses EN;
it is an alternative P4 firmware, not a second owner running concurrently.
The pull-up can release C6 while P4 is reset; the running host firmware does
not guarantee an EN hold across P4 reprogramming.

See [the P4 flash layout](esp32p4-waveshare-flash-layout.md). Only the P4
application image belongs at 0x110000 under that documented factory layout.
Programming is a separate operator-authorized action and is not part of the
software validation of these changes.

## Upstream preparation

The SDIO changes, local QA, and generic xtask correction have separate review
concerns. Before an upstream submission, extract the xtask regression fix and
the PAC metadata patch as independent proposals. Keep board-specific Device Lab
recipes and factory flash assumptions out of the portable driver contract.

The C6-specific PAC patch is included as [esp-pacs-c6-slc.patch](esp-pacs-c6-slc.patch). It corrects
TOKEN_NO_REPLACE (bit 0), both TOKEN1_WDATA fields (12 bits), SLC1 token field
names, and read-only PARK fields. Other chips sharing `common_patches/slc.yaml`
are unchanged. Source references: C6 TRM v1.2 registers 34.8/34.10/34.12/34.14,
34.16/34.17/34.19; Espressif v5.4.2 `components/soc/esp32c6/register/soc/slc_reg.h`.

Software acceptance does not establish STOP/PARK timing, pad electrical levels,
EN measurements, or behaviour under a physical reset. Those limits stay explicit;
the earlier 64-byte result does not qualify the corrected revision.

## Software validation commands

- `cargo xtask host-tests esp-hal-host-tests`: native host tests and Miri.
- `cargo test -p esp-devtool --lib host_test_command_tests`: Miri command construction regressions.
- `cargo xtask check esp-hal esp32c6`: C6 configuration matrix, including the added defmt case.
- `cargo xtask lint esp-hal esp32c6`: Clippy with warnings denied.
- `cargo xtask documentation --packages=esp-hal esp32c6`: rustdoc.
- `cargo xtask build qa esp32c6 sdio-a2-c6-slave`: C6 QA compilation.
- `SDIO_A2=1 cargo xtask build qa esp32p4 sdio-a1-p4-host`: P4 A2a QA compilation.

The separate PAC patch was generated and built with `cargo xtask build esp32c6`
in esp-pacs. Its three host regressions run with an explicit host target:
`cargo +stable test --manifest-path esp32c6/Cargo.toml --target x86_64-unknown-linux-gnu --test slc_fields`.
They exercise generated accessor bit packing against RAM-backed registers, not
real peripherals. The four PARK fields are also verified read-only in the patched SVD.
