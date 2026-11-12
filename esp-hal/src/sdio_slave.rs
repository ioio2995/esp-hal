//! # ESP32-C6 native SDIO Slave — hardware bring-up (A1)
//!
//! This module initialises the SDIO Slave controller so that an SDIO host can
//! **enumerate** the card. It is **not** a working SDIO Slave driver: there is
//! no DMA, no interrupt handling, and no data transfer. The driver never
//! declares an I/O function as ready (`IOR1`/`IOR2` stay 0), so a host cannot
//! start transferring data.
//!
//! The register blocks used are `HINF`, `SLC` and `SLCHOST`, described by the
//! ESP32-C6 TRM, chapter "SDIO Slave Controller".
//!
//! ## States
//!
//! | Notion | Where it comes from |
//! |---|---|
//! | Controller initialised | [`State::ControllerReady`] |
//! | Card enumerable | [`State::BusEnumerable`] |
//! | Function enabled by the host | [`SdioSlave::function1`] / [`SdioSlave::function2`] report [`FunctionState::EnabledByHost`] |
//! | Function ready to transfer | [`FunctionState::Ready`], never reached by this driver |
//!
//! ## Lifecycle
//!
//! [`SdioSlave::new`] takes the peripheral clock (reference counted, the block
//! is reset the first time) and nothing else: the pads are not routed and no
//! function register is written. [`SdioSlave::enable_bus`] clears the
//! function-ready bits, then routes the six SDIO pads. [`SdioSlave::disable_bus`]
//! gives the pads back.
//!
//! Dropping the driver gives the pads back if they are routed, then releases
//! the clock. It is **not** a complete hardware shutdown once DMA exists: a
//! future DMA layer must stop and drain the engine before the clock is
//! released. The clock guard is the last field of the struct for that reason.
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

/// Raw diagnostic registers. The values are not proof of a successful
/// enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct RegisterSnapshot {
    /// `HINF_CFG_DATA1_REG` (CCCR configuration and status).
    pub cfg_data1: u32,
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
            _clock: clock,
        }
    }

    /// Current bring-up state.
    pub fn state(&self) -> State {
        self.state
    }

    /// Makes the card visible to an SDIO host.
    ///
    /// 1. Clears `IOREADY1` and `IOREADY2` (no function is ready).
    /// 2. Routes the six pads to the controller (IO MUX function 0).
    ///
    /// The CCCR, FBR and CIS keep their reset values, and the interrupts and
    /// DMA are not touched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] unless the state is
    /// [`State::ControllerReady`].
    pub fn enable_bus(&mut self) -> Result<(), Error> {
        let next = State::BusEnumerable;
        if !self.state.can_transition_to(next) {
            return Err(Error::InvalidState);
        }

        self.clear_function_ready();

        for pad in &self.pads {
            pad.set_alternate_function(AlternateFunction::_0);
            pad.set_input_enable(true);
        }

        self.state = next;
        Ok(())
    }

    /// Takes the card off the bus: the pads go back to plain GPIO inputs.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] unless the state is
    /// [`State::BusEnumerable`].
    pub fn disable_bus(&mut self) -> Result<(), Error> {
        let next = State::ControllerReady;
        if !self.state.can_transition_to(next) {
            return Err(Error::InvalidState);
        }

        self.release_pads();
        self.state = next;
        Ok(())
    }

    /// State of I/O function 1, as seen by the host.
    pub fn function1(&self) -> FunctionState {
        let cfg = HINF::regs().cfg_data1().read();
        state::function_state(cfg.ioenable1().bit_is_set(), cfg.sdio_ioready1().bit_is_set())
    }

    /// State of I/O function 2, as seen by the host.
    pub fn function2(&self) -> FunctionState {
        let cfg = HINF::regs().cfg_data1().read();
        state::function_state(cfg.ioenable2().bit_is_set(), cfg.sdio_ioready2().bit_is_set())
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
            function0_status: HINF::regs().conf_status().read().bits(),
            slc0_interrupt_raw: SLC::regs().slc0int_raw().read().bits(),
            slc1_interrupt_raw: SLC::regs().slc1int_raw().read().bits(),
        }
    }

    fn clear_function_ready(&self) {
        debug_assert!(!self.state.function_ready_allowed());
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
            self.release_pads();
        }
    }
}
