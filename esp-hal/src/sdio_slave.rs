//! # ESP32-C6 SDIO Slave: A1 enumeration and A2a host-to-slave reception
//!
//! One descriptor receives one incremental-address packet on SLC0. The host
//! writes CMD53 to `0x1F800 - length`, after function 1 is enabled and ready.
//! Slave-to-host DMA, interrupts, and automatic rearming are not implemented.
//!
//! [`SdioSlave::arm_receive`] consumes exclusive static storage in internal RAM.
//! Only a validated EOF with a parked DMA makes [`SdioSlave::received`] available.
//! [`SdioSlave::disable_bus`] clears readiness and waits for DMA idle before
//! releasing pads. [`SdioSlave::take_receive_buffers`] returns storage only after
//! this stop succeeds. A stop timeout retains ownership and holds the FSM reset.
//! If destruction cannot confirm idle, the clock reference and static storage
//! are retained instead of permitting unsafe reuse.
//!
//! Pads use fixed IO MUX function 0: CMD GPIO18, CLK GPIO19, DATA0..3 GPIO20..23.
//! Disabling the bus selects GPIO with input/output disabled. Previous pad
//! settings are not restored; pull resistors are unchanged.
//!
//! The implementation uses HINF, SLC and SLCHOST (ESP32-C6 TRM chapter 34).
//! A1 enumeration and an A2a 64-byte transfer were reported by the developer.
//! Those results do not qualify later source changes. See
//! `docs/esp32c6-sdio-a2a.md` for the current contract and limitations.

use core::sync::atomic::{Ordering, compiler_fence};

use crate::{
    gpio::{AlternateFunction, AnyPin, Pin},
    peripherals::{GPIO18, GPIO19, GPIO20, GPIO21, GPIO22, GPIO23, HINF, SLC, SLCHOST},
    system::{Peripheral, PeripheralGuard},
};

mod receive;
mod state;

use receive::Receive;
pub use receive::{ArmReceiveError, Descriptor};
use state::wait_for_idle;
pub use state::{Error, FunctionState, State};

/// The six pads of the SDIO Slave bus.
///
/// They are fixed to GPIO18..GPIO23 by IO MUX function 0.
#[non_exhaustive]
pub struct Pins<'d> {
    /// `SDIO_CMD`
    pub cmd: GPIO18<'d>,
    /// `SDIO_CLK`
    pub clk: GPIO19<'d>,
    /// `SDIO_DATA0`
    pub data0: GPIO20<'d>,
    /// `SDIO_DATA1`
    pub data1: GPIO21<'d>,
    /// `SDIO_DATA2`
    pub data2: GPIO22<'d>,
    /// `SDIO_DATA3`
    pub data3: GPIO23<'d>,
}

impl<'d> Pins<'d> {
    /// Groups the six SDIO pins.
    pub fn new(
        cmd: GPIO18<'d>,
        clk: GPIO19<'d>,
        data0: GPIO20<'d>,
        data1: GPIO21<'d>,
        data2: GPIO22<'d>,
        data3: GPIO23<'d>,
    ) -> Self {
        Self {
            cmd,
            clk,
            data0,
            data1,
            data2,
            data3,
        }
    }
}

/// Raw view of the receive path, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveDiagnostics {
    /// First word of the descriptor as the DMA left it (owner, eof, length, size).
    pub descriptor_dw0: u32,
    /// `SDIO_SLC0_TOKEN1`: accumulated number of receive buffers announced to the host.
    pub token: u32,
    /// `SDIO_SLC0INT_RAW_REG`.
    pub interrupt_raw: u32,
    /// `SDIO_SLC0_TXLINK_PARK`: the receive FSM is idle.
    pub parked: bool,
}

/// Raw diagnostic registers. The values are not proof of a successful
/// enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RegisterSnapshot {
    /// `HINF_CFG_DATA1_REG` (CCCR configuration and status).
    pub cfg_data1: u32,
    /// `HINF_CFG_DATA7_REG` (function 0 ready, interrupt and capability bits).
    pub cfg_data7: u32,
    /// `HINF_CONF_STATUS_REG` (function 0 configuration status).
    pub function0_status: u32,
    /// `SDIO_SLC0INT_RAW_REG`: SLC0 raw interrupt status.
    pub slc0_interrupt_raw: u32,
    /// `SDIO_SLC1INT_RAW_REG`: SLC1 raw interrupt status.
    pub slc1_interrupt_raw: u32,
}

/// SDIO Slave controller, bring-up level.
pub struct SdioSlave<'d> {
    _hinf: HINF<'d>,
    _slc: SLC<'d>,
    _slchost: SLCHOST<'d>,
    pads: [AnyPin<'d>; 6],
    state: State,
    receive: Option<Receive>,
    receive_error: Option<Error>,
    // Dropped last: the clock must outlive every other resource.
    _clock: Option<PeripheralGuard>,
}

impl<'d> SdioSlave<'d> {
    /// Takes the controller and its pins. Enables the clock of the block.
    ///
    /// The first user of the clock also resets the block. No pad is routed and
    /// no function register is written.
    pub fn new(hinf: HINF<'d>, slc: SLC<'d>, slchost: SLCHOST<'d>, pins: Pins<'d>) -> Self {
        let clock = PeripheralGuard::new(Peripheral::SdioSlave);

        Self {
            _hinf: hinf,
            _slc: slc,
            _slchost: slchost,
            pads: [
                pins.cmd.degrade(),
                pins.clk.degrade(),
                pins.data0.degrade(),
                pins.data1.degrade(),
                pins.data2.degrade(),
                pins.data3.degrade(),
            ],
            state: State::ControllerReady,
            receive: None,
            receive_error: None,
            _clock: Some(clock),
        }
    }

    /// Current bring-up state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Exposes the SDIO controller on the bus.
    ///
    /// 1. Clears `IOREADY1` and `IOREADY2` (no function is ready).
    /// 2. Puts each pad in a defined state: output disabled, hold released, GPIO matrix output on
    ///    plain GPIO.
    /// 3. Selects IO MUX function 0 on the six pads and enables their input. The output direction
    ///    of the pads is left to the controller.
    ///
    /// The CCCR, FBR and CIS keep their reset values, and the interrupts and
    /// DMA are not touched. A successful return does not prove that a host can
    /// enumerate the card.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] unless the state is
    /// [`State::ControllerReady`].
    pub fn enable_bus(&mut self) -> Result<(), Error> {
        let next = State::BusExposed;
        if !self.state.can_transition_to(next) {
            return Err(Error::InvalidState);
        }

        self.clear_function_ready();

        for pad in &self.pads {
            // Known starting point: no output forced by the GPIO matrix, hold
            // released. Then hand the pad over to the controller.
            pad.init_gpio();
            pad.set_alternate_function(AlternateFunction::_0);
            pad.set_input_enable(true);
        }

        self.state = next;
        Ok(())
    }

    /// Takes the card off the bus.
    ///
    /// Clears `IOREADY1/2`, then puts the pads back in a defined GPIO state
    /// (output and input disabled, hold released). The previous pad
    /// configuration is not restored, the pulls are not changed, the clock is
    /// kept and the block is not reset.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] unless the pads are routed ([`State::BusExposed`] or
    /// [`State::Function1Receiving`], [`State::Function1Received`], or
    /// [`State::Function1ReceiveError`]). [`Error::DmaStopTimeout`] if DMA does not
    /// become idle. Storage, pads, and the clock remain owned on timeout.
    pub fn disable_bus(&mut self) -> Result<(), Error> {
        let next = State::ControllerReady;
        if !self.state.can_transition_to(next) {
            return Err(Error::InvalidState);
        }

        self.clear_function_ready();
        self.stop_receive_dma()?;
        self.release_pads();
        self.state = next;
        Ok(())
    }

    /// State of I/O function 1, as seen by the host.
    pub fn function1(&self) -> FunctionState {
        let cfg = HINF::regs().cfg_data1().read();
        state::function_state(
            cfg.ioenable1().bit_is_set(),
            cfg.sdio_ioready1().bit_is_set(),
        )
    }

    /// State of I/O function 2, as seen by the host.
    pub fn function2(&self) -> FunctionState {
        let cfg = HINF::regs().cfg_data1().read();
        state::function_state(
            cfg.ioenable2().bit_is_set(),
            cfg.sdio_ioready2().bit_is_set(),
        )
    }

    /// Reads the diagnostic registers without clearing or acknowledging
    /// anything.
    ///
    /// The block is clocked for as long as this value exists, so the read is
    /// always valid. Raw interrupt status is read: the masked status would
    /// read zero while the interrupts are disabled.
    pub fn snapshot(&self) -> RegisterSnapshot {
        RegisterSnapshot {
            cfg_data1: HINF::regs().cfg_data1().read().bits(),
            cfg_data7: HINF::regs().cfg_data7().read().bits(),
            function0_status: HINF::regs().conf_status().read().bits(),
            slc0_interrupt_raw: SLC::regs().slc0int_raw().read().bits(),
            slc1_interrupt_raw: SLC::regs().slc1int_raw().read().bits(),
        }
    }

    /// (A2a) Arms the SLC0 receive DMA (host to slave) with one descriptor and one buffer, and
    /// announces one receive buffer to the host.
    ///
    /// 1. Initialises the DMA configuration bits that the TRM says to initialise (`TOKEN_AUTO_CLR`
    ///    0, `RX_NO_RESTART_CLR` 1, `TOKEN_NO_REPLACE` 1, the `SLCCONF1` bits 0).
    /// 2. Pulses the SLC0 TX (host to slave) FSM reset.
    /// 3. Writes the descriptor (`owner` = DMA, `eof` = 0, `length` = 0, `size` = buffer size).
    /// 4. Writes the descriptor address to `SDIO_SLC0TX_LINK_ADDR_REG` and sets `TXLINK_START`.
    /// 5. Adds one to the SLC0 token counter, so the host reads one more available buffer in
    ///    `SLCHOST_SLC0HOST_TOKEN_RDATA_REG`.
    ///
    /// The function is **not** declared ready here, see [`Self::set_function1_ready`].
    ///
    /// # Errors
    ///
    /// [`ArmReceiveError`] retains the original storage and contains
    /// [`Error::InvalidState`] unless the state is [`State::BusExposed`]; [`Error::InvalidBuffer`]
    /// if the buffer is empty or larger than 16380 bytes. [`Error::UnsupportedMemory`]
    /// if either allocation is outside internal RAM. Existing storage must be
    /// reclaimed with [`Self::take_receive_buffers`] before a new arm.
    pub fn arm_receive(
        &mut self,
        descriptor: &'static mut Descriptor,
        buffer: &'static mut [u32],
    ) -> Result<(), ArmReceiveError> {
        let next = State::Function1Receiving;
        if !self.state.can_transition_to(next) || self.state != State::BusExposed {
            return Err(ArmReceiveError::new(
                Error::InvalidState,
                descriptor,
                buffer,
            ));
        }
        if self.receive.is_some() {
            return Err(ArmReceiveError::new(
                Error::InvalidState,
                descriptor,
                buffer,
            ));
        }
        if buffer.is_empty() || buffer.len() > 0x3ffc / 4 {
            return Err(ArmReceiveError::new(
                Error::InvalidBuffer,
                descriptor,
                buffer,
            ));
        }

        if !crate::soc::is_slice_in_dram(buffer)
            || !crate::soc::is_slice_in_dram(core::slice::from_ref(descriptor))
        {
            return Err(ArmReceiveError::new(
                Error::UnsupportedMemory,
                descriptor,
                buffer,
            ));
        }
        let receive = Receive::new(descriptor, buffer)?;
        let descriptor_address = receive.descriptor_address();
        self.receive = Some(receive);
        self.receive_error = None;

        let slc = SLC::regs();
        slc.slcconf0().modify(|_, w| {
            w.sdio_slc0_token_auto_clr().clear_bit();
            w.sdio_slc0_rx_no_restart_clr().set_bit();
            w.sdio_slc0_tx_loop_test().clear_bit()
        });
        // esp-pacs 61f9678 addresses bit 1 here. TRM 34.19 specifies bit 0.
        slc.slc_rx_dscr_conf()
            .modify(|r, w| unsafe { w.bits(receive::token_no_replace(r.bits())) });
        slc.slcconf1().modify(|_, w| {
            w.sdio_sdio_cmd_hold_en().clear_bit();
            w.sdio_slc0_len_auto_clr().clear_bit();
            w.sdio_slc0_tx_stitch_en().clear_bit();
            w.sdio_slc0_rx_stitch_en().clear_bit()
        });
        // Reset the SLC0 TX FSM, then release it.
        slc.slcconf0().modify(|_, w| w.sdio_slc0_tx_rst().set_bit());
        slc.slcconf0()
            .modify(|_, w| w.sdio_slc0_tx_rst().clear_bit());

        // Orders descriptor stores before link activation on the C6 AHB bus.
        dma_fence();

        slc.slc0int_clr()
            .write(|w| w.sdio_slc0_tx_suc_eof_int_clr().set_bit());
        slc.slc0tx_link_addr()
            .write(|w| unsafe { w.sdio_slc0_txlink_addr().bits(descriptor_address) });
        slc.slc0tx_link()
            .modify(|_, w| w.sdio_slc0_txlink_start().set_bit());
        // The counter is cumulative: overwriting it with 1 on explicit reuse
        // would make the host's already-consumed credit disappear.
        slc.slc0token1()
            .write(|w| w.sdio_slc0_token1_inc().set_bit());

        self.state = next;
        Ok(())
    }

    /// (A2a) Declares I/O function 1 ready (`IOREADY1`). Only possible once the receive DMA is
    /// armed. The host sees it in CCCR `IOR1` once it has enabled the function (`IOE1`).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidState`] unless the state is [`State::Function1Receiving`].
    pub fn set_function1_ready(&mut self) -> Result<(), Error> {
        if !self.state.function_ready_allowed() {
            return Err(Error::InvalidState);
        }
        HINF::regs()
            .cfg_data1()
            .modify(|_, w| w.sdio_ioready1().set_bit());
        Ok(())
    }

    /// Checks for one successfully received packet.
    ///
    /// The EOF event and parked DMA are both required. The descriptor must have
    /// CPU ownership, EOF, and a length within the buffer. A successful result
    /// makes [`Self::received`] available and withdraws function readiness.
    /// Subsequent polls return `Ok(None)`. No automatic rearming occurs.
    ///
    /// # Errors
    ///
    /// [`Error::DmaFault`] on a DMA overflow or descriptor error.
    /// [`Error::InvalidDescriptor`] on invalid write-back. Storage remains owned
    /// on error and must be stopped with [`Self::disable_bus`].
    pub fn poll_received(&mut self) -> Result<Option<usize>, Error> {
        if let Some(error) = self.receive_error {
            return Err(error);
        }
        if self.state != State::Function1Receiving {
            return Ok(None);
        }
        let slc = SLC::regs();
        let raw = slc.slc0int_raw().read();
        if raw.sdio_slc0_tx_dscr_err_int_raw().bit_is_set()
            || raw.sdio_slc0_tx_ovf_int_raw().bit_is_set()
        {
            return self.receive_failed(Error::DmaFault);
        }
        if !raw.sdio_slc0_tx_suc_eof_int_raw().bit_is_set()
            || !slc
                .slc0tx_link()
                .read()
                .sdio_slc0_txlink_park()
                .bit_is_set()
        {
            return Ok(None);
        }
        self.clear_function_ready();
        dma_fence();
        // SAFETY: successful EOF and parked DMA were observed above. The
        // acquire barrier precedes every CPU access to the received bytes.
        let result = unsafe { self.receive.as_mut().unwrap().finish() };
        let length = match result {
            Ok(length) => length,
            Err(error) => return self.receive_failed(error),
        };
        slc.slc0int_clr()
            .write(|w| w.sdio_slc0_tx_suc_eof_int_clr().set_bit());
        self.state = State::Function1Received;
        Ok(Some(length))
    }

    /// Returns the validated packet after successful completion.
    ///
    /// Returns `None` before completion and after an error or bus disable.
    /// The length comes from validated DMA write-back, not from the caller.
    pub fn received(&self) -> Option<&[u8]> {
        if self.state != State::Function1Received {
            return None;
        }
        self.receive.as_ref()?.received()
    }

    /// Returns descriptor and buffer storage after disabling the bus.
    ///
    /// This operation permits explicit reuse of the static allocations. It does
    /// not arm DMA, expose pads, or publish a new token.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidState`] unless [`Self::disable_bus`] has succeeded.
    pub fn take_receive_buffers(
        &mut self,
    ) -> Result<Option<(&'static mut Descriptor, &'static mut [u32])>, Error> {
        if self.state != State::ControllerReady {
            return Err(Error::InvalidState);
        }
        // SAFETY: ControllerReady can only be reached after DMA is confirmed
        // stopped, or before any transfer. take() prevents double restitution.
        Ok(self
            .receive
            .take()
            .map(|receive| unsafe { receive.into_buffers() }))
    }

    fn receive_failed(&mut self, error: Error) -> Result<Option<usize>, Error> {
        self.clear_function_ready();
        self.receive_error = Some(error);
        self.state = State::Function1ReceiveError;
        Err(error)
    }

    /// (A2a) Raw receive-path registers, read without clearing anything.
    pub fn receive_diagnostics(&self) -> ReceiveDiagnostics {
        let slc = SLC::regs();
        let descriptor_dw0 = self
            .receive
            .as_ref()
            .map(Receive::descriptor_word)
            .unwrap_or(0);
        ReceiveDiagnostics {
            descriptor_dw0,
            token: slc.slc0token1().read().sdio_slc0_token1().bits() as u32,
            interrupt_raw: slc.slc0int_raw().read().bits(),
            parked: slc
                .slc0tx_link()
                .read()
                .sdio_slc0_txlink_park()
                .bit_is_set(),
        }
    }

    /// Confirms idle before allowing resource reuse. A timeout holds TX reset
    /// asserted and preserves storage, pads, and clock ownership.
    fn stop_receive_dma(&mut self) -> Result<(), Error> {
        if self.receive.is_none() {
            return Ok(());
        }
        let slc = SLC::regs();
        slc.slc0tx_link()
            .modify(|_, w| w.sdio_slc0_txlink_stop().set_bit());
        dma_fence();
        let parked = || {
            let link = slc.slc0tx_link().read();
            link.sdio_slc0_txlink_park().bit_is_set()
                && link.sdio_slc0_txlink_start().bit_is_clear()
                && link.sdio_slc0_txlink_stop().bit_is_clear()
        };
        let mut idle = wait_for_idle(parked);
        slc.slcconf0().modify(|_, w| w.sdio_slc0_tx_rst().set_bit());
        dma_fence();
        if !idle {
            idle = wait_for_idle(parked);
        }
        if !idle {
            self.receive_error = Some(Error::DmaStopTimeout);
            self.state = State::Function1ReceiveError;
            return Err(Error::DmaStopTimeout);
        }
        dma_fence();
        slc.slcconf0()
            .modify(|_, w| w.sdio_slc0_tx_rst().clear_bit());
        slc.slc0int_clr().write(|w| {
            w.sdio_slc0_tx_suc_eof_int_clr().set_bit();
            w.sdio_slc0_tx_dscr_err_int_clr().set_bit();
            w.sdio_slc0_tx_ovf_int_clr().set_bit()
        });
        dma_fence();
        self.receive_error = None;
        Ok(())
    }

    fn clear_function_ready(&self) {
        HINF::regs().cfg_data1().modify(|_, w| {
            w.sdio_ioready1().clear_bit();
            w.sdio_ioready2().clear_bit()
        });
    }

    fn release_pads(&self) {
        for pad in &self.pads {
            pad.init_gpio();
        }
    }
}

impl Drop for SdioSlave<'_> {
    fn drop(&mut self) {
        if self.state.pads_routed() {
            self.clear_function_ready();
            if self.stop_receive_dma().is_err() {
                // Reset stays asserted. Leaking one clock reference and static
                // storage is safer than permitting DMA to access reused memory.
                if let Some(clock) = self._clock.take() {
                    core::mem::forget(clock);
                }
                return;
            }
            self.release_pads();
        }
    }
}

/// Orders CPU memory and peripheral I/O accesses at DMA ownership boundaries.
fn dma_fence() {
    compiler_fence(Ordering::SeqCst);
    // SAFETY: this instruction only orders accesses; it touches no registers or
    // stack memory. No `nomem` option: the compiler must also respect the barrier.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
    compiler_fence(Ordering::SeqCst);
}

impl core::fmt::Debug for SdioSlave<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SdioSlave")
            .field("state", &self.state)
            .field("receive_error", &self.receive_error)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "defmt")]
impl defmt::Format for SdioSlave<'_> {
    fn format(&self, f: defmt::Formatter<'_>) {
        defmt::write!(
            f,
            "SdioSlave {{ state: {:?}, receive_error: {:?} }}",
            self.state,
            self.receive_error
        );
    }
}
