//! ESP32-C6 native SDIO Slave support (early foundation).
//!
//! This module currently provides a **read-only diagnostic handle**, not a
//! working SDIO Slave driver. In particular, it does **not** configure the
//! SDIO pins, clocks, function registers, DMA descriptors, or interrupts.
//!
//! The SLC, HINF, and SLCHOST register blocks are defined by `esp-pacs`.
//! The driver will build on those definitions without ESP-IDF or C FFI.
//!
//! # Safety and scope
//!
//! Taking ownership of all three peripheral tokens prevents a second safe
//! instance from accessing the same blocks. Register snapshots are read-only,
//! so constructing this handle does not expose an incomplete DMA setup as an
//! operational peripheral. No host communication is promised at this stage.

use crate::peripherals::{HINF, SLC, SLCHOST};

/// Raw hardware registers useful for bringing up the C6's SDIO Slave block.
///
/// These values are intentionally **not** interpreted as proof of a successful
/// SDIO enumeration. They are useful for diagnostics before the state machine
/// and DMA driver are implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// SDIO function-zero configuration/status register (raw value).
    pub function0_status: u32,
    /// SLC0 interrupt status (raw value).
    pub slc0_interrupt_status: u32,
    /// SLC1 extended interrupt status (raw value).
    pub slc1_interrupt_status: u32,
}

/// Exclusive ownership of the three register blocks used by SDIO Slave.
///
/// This is a bring-up helper. It does not turn on the hardware and must not
/// be used as a substitute for a configured DMA/interrupt-driven driver.
pub struct SdioSlaveRegisters<'d> {
    _hinf: HINF<'d>,
    _slc: SLC<'d>,
    _slchost: SLCHOST<'d>,
}

impl<'d> SdioSlaveRegisters<'d> {
    /// Acquires the hardware register blocks without modifying any registers.
    pub fn new(hinf: HINF<'d>, slc: SLC<'d>, slchost: SLCHOST<'d>) -> Self {
        Self {
            _hinf: hinf,
            _slc: slc,
            _slchost: slchost,
        }
    }

    /// Reads status registers without acknowledging or clearing interrupts.
    pub fn snapshot(&self) -> RegisterSnapshot {
        RegisterSnapshot {
            function0_status: HINF::regs().conf_status().read().bits(),
            slc0_interrupt_status: SLC::regs().slc0int_st().read().bits(),
            slc1_interrupt_status: SLC::regs().slc1int_st1().read().bits(),
        }
    }
}
