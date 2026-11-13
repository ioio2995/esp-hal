//! # ESP32-C6 native SDIO Slave — bring-up (A1) and first receive path (A2a)
//!
//! **A2a (not hardware-validated yet):** [`SdioSlave::arm_receive`] mounts one SLC0 receive
//! descriptor (host to slave, I/O function 1, incremental-address packets) and
//! [`SdioSlave::set_function1_ready`] then declares function 1 ready. A packet that the host writes
//! with CMD53 at address `0x1F800 - length` lands in the buffer; [`SdioSlave::poll_received`]
//! reports its length. There is still no slave to host DMA, no interrupt handling and no flow
//! control. The text below describes A1 and stays true for [`State::BusExposed`].
//!
//! This module clocks the SDIO Slave controller and exposes its pads, which is
//! the prerequisite for a host to enumerate the card. Whether a host actually
//! can has **not** been demonstrated on hardware. It is **not** a working SDIO
//! Slave driver: there is no DMA, no interrupt handling, and no data transfer.
//! The driver never declares an I/O function as ready (`IOR1`/`IOR2` stay 0),
//! so a host cannot start transferring data.
//!
//! The register blocks used are `HINF`, `SLC` and `SLCHOST`, described by the
//! ESP32-C6 TRM, chapter "SDIO Slave Controller".
//!
//! ## States
//!
//! | Notion | Where it comes from |
//! |---|---|
//! | Controller initialised | [`State::ControllerReady`] |
//! | Bus exposed (not proven enumerable) | [`State::BusExposed`] |
//! | Function enabled by the host | [`SdioSlave::function1`] / [`SdioSlave::function2`] report [`FunctionState::EnabledByHost`] |
//! | Function ready to transfer | [`FunctionState::Ready`], never reached by this driver |
//!
//! ## Lifecycle
//!
//! [`SdioSlave::new`] takes the peripheral clock (reference counted, the block
//! is reset the first time) and nothing else: the pads are not routed and no
//! function register is written.
//!
//! [`SdioSlave::enable_bus`] clears the function-ready bits, puts each pad in a
//! defined GPIO state (output disabled, pad hold released, GPIO matrix output
//! on plain GPIO), then selects IO MUX function 0 with input enabled. The
//! controller keeps control of the signal directions: the driver never forces an
//! output enable.
//!
//! [`SdioSlave::disable_bus`] clears the function-ready bits again, then puts
//! each pad back in the same defined GPIO state: output disabled, input
//! disabled, hold released, GPIO function selected. The pull resistors are not
//! touched. **The configuration the pads had before `enable_bus` is not saved
//! and is not restored.** The clock is kept and the block is not reset.
//!
//! Dropping the driver does what `disable_bus` does if the pads are routed, then
//! releases the clock reference. The clock is only gated when the last reference
//! goes away. Neither operation stops a DMA engine, because none exists yet:
//! a future DMA layer must stop and drain it **before** the pads are released
//! and before the clock is. The clock guard is the last field of the struct for
//! that reason.
//!
//! ## Pins
//!
//! The SDIO pads are fixed IO MUX function 0 of GPIO18 (CMD), GPIO19 (CLK) and
//! GPIO20..GPIO23 (DATA0..DATA3). The GPIO matrix cannot be used.
//!
//! ## Hardware status
//!
//! Nothing here has been qualified on hardware. See
//! `docs/esp32c6-sdio-slave-bringup.md`.

use core::ptr;

use crate::{
    gpio::{AlternateFunction, AnyPin, Pin},
    peripherals::{GPIO18, GPIO19, GPIO20, GPIO21, GPIO22, GPIO23, HINF, SLC, SLCHOST},
    system::{Peripheral, PeripheralGuard},
};

mod state;

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

/// One SDIO slave DMA linked-list descriptor (TRM 34.5.5.1): three words.
///
/// The layout is fixed by the hardware: `dw0` holds `owner` (bit 31), `eof` (bit 30), the valid
/// `length` (bits 27:14) and the buffer `size` (bits 13:0); `buffer` is the word-aligned buffer
/// address; `next` is the next descriptor or 0. It must live in memory the DMA can reach and must
/// not move while the DMA owns it, hence the `'static` borrow taken by [`SdioSlave::arm_receive`].
#[repr(C, align(4))]
#[derive(Debug)]
pub struct Descriptor {
    dw0: u32,
    buffer: u32,
    next: u32,
}

impl Descriptor {
    /// An empty descriptor, not owned by the DMA.
    pub const fn new() -> Self {
        Self {
            dw0: 0,
            buffer: 0,
            next: 0,
        }
    }
}

impl Default for Descriptor {
    fn default() -> Self {
        Self::new()
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
    rx_desc: Option<&'static mut Descriptor>,
    rx_buffer: Option<&'static mut [u32]>,
    // Dropped last: the clock must outlive every other resource.
    _clock: PeripheralGuard,
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
            rx_desc: None,
            rx_buffer: None,
            _clock: clock,
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
    /// [`State::Function1Receiving`], whose receive DMA is stopped first).
    pub fn disable_bus(&mut self) -> Result<(), Error> {
        let next = State::ControllerReady;
        if !self.state.can_transition_to(next) {
            return Err(Error::InvalidState);
        }

        self.stop_receive_dma();
        self.clear_function_ready();
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
    /// 5. Writes 1 to the SLC0 token register, so the host reads one available buffer in
    ///    `SLCHOST_SLC0HOST_TOKEN_RDATA_REG`.
    ///
    /// The function is **not** declared ready here, see [`Self::set_function1_ready`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidState`] unless the state is [`State::BusExposed`]; [`Error::InvalidBuffer`]
    /// if the buffer is empty or larger than 16380 bytes.
    pub fn arm_receive(
        &mut self,
        descriptor: &'static mut Descriptor,
        buffer: &'static mut [u32],
    ) -> Result<(), Error> {
        let next = State::Function1Receiving;
        if !self.state.can_transition_to(next) || self.state != State::BusExposed {
            return Err(Error::InvalidState);
        }
        let size = buffer.len() * 4;
        if buffer.is_empty() || size > 0x3ffc {
            return Err(Error::InvalidBuffer);
        }

        let slc = SLC::regs();
        slc.slcconf0().modify(|_, w| {
            w.sdio_slc0_token_auto_clr().clear_bit();
            w.sdio_slc0_rx_no_restart_clr().set_bit();
            w.sdio_slc0_tx_loop_test().clear_bit()
        });
        slc.slc_rx_dscr_conf()
            .modify(|_, w| w.sdio_slc0_token_no_replace().set_bit());
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

        // Descriptor: owned by the DMA, empty, sized as the buffer, no next descriptor.
        let buffer_address = buffer.as_mut_ptr() as u32;
        unsafe {
            ptr::write_volatile(
                descriptor as *mut Descriptor,
                Descriptor {
                    dw0: (1 << 31) | (size as u32 & 0x3fff),
                    buffer: buffer_address,
                    next: 0,
                },
            );
        }
        let descriptor_address = descriptor as *mut Descriptor as u32;

        slc.slc0int_clr()
            .write(|w| w.sdio_slc0_tx_suc_eof_int_clr().set_bit());
        slc.slc0tx_link_addr()
            .write(|w| unsafe { w.sdio_slc0_txlink_addr().bits(descriptor_address) });
        slc.slc0tx_link()
            .modify(|_, w| w.sdio_slc0_txlink_start().set_bit());
        slc.slc0token1().write(|w| {
            unsafe { w.sdio_slc0_token1_wdata().bits(1) };
            w.sdio_slc0_token1_wr().set_bit()
        });

        self.rx_desc = Some(descriptor);
        self.rx_buffer = Some(buffer);
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

    /// (A2a) Checks whether the host has finished writing a packet (`SLC0_TX_SUC_EOF` raw
    /// interrupt). If so, clears that flag and returns the packet length in bytes, read from the
    /// descriptor the DMA wrote back. The buffer is then readable with [`Self::received`]. The
    /// receive DMA is **not** re-armed: one packet per [`Self::arm_receive`].
    pub fn poll_received(&mut self) -> Option<usize> {
        let slc = SLC::regs();
        if !slc
            .slc0int_raw()
            .read()
            .sdio_slc0_tx_suc_eof_int_raw()
            .bit_is_set()
        {
            return None;
        }
        slc.slc0int_clr()
            .write(|w| w.sdio_slc0_tx_suc_eof_int_clr().set_bit());
        let descriptor = self.rx_desc.as_ref()?;
        let dw0 = unsafe { ptr::read_volatile(&descriptor.dw0) };
        Some(((dw0 >> 14) & 0x3fff) as usize)
    }

    /// (A2a) The first `length` bytes of the receive buffer (clamped to its size).
    pub fn received(&self, length: usize) -> &[u8] {
        match self.rx_buffer.as_ref() {
            Some(buffer) => {
                let bytes = buffer.len() * 4;
                let length = length.min(bytes);
                unsafe { core::slice::from_raw_parts(buffer.as_ptr() as *const u8, length) }
            }
            None => &[],
        }
    }

    /// (A2a) Raw receive-path registers, read without clearing anything.
    pub fn receive_diagnostics(&self) -> ReceiveDiagnostics {
        let slc = SLC::regs();
        let descriptor_dw0 = self
            .rx_desc
            .as_ref()
            .map(|d| unsafe { ptr::read_volatile(&d.dw0) })
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

    /// Stops the SLC0 receive DMA and resets its FSM, if it was armed. The descriptor and the
    /// buffer are released to the caller's static storage only after the DMA is stopped.
    fn stop_receive_dma(&mut self) {
        if self.state != State::Function1Receiving {
            return;
        }
        let slc = SLC::regs();
        slc.slc0tx_link()
            .modify(|_, w| w.sdio_slc0_txlink_stop().set_bit());
        slc.slcconf0().modify(|_, w| w.sdio_slc0_tx_rst().set_bit());
        slc.slcconf0()
            .modify(|_, w| w.sdio_slc0_tx_rst().clear_bit());
        self.rx_desc = None;
        self.rx_buffer = None;
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
            self.stop_receive_dma();
            self.clear_function_ready();
            self.release_pads();
        }
    }
}
