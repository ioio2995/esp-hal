//! Register-free state and invariant logic of the SDIO Slave bring-up.
//!
//! Nothing in this file touches hardware, so it is compiled by the host tests
//! as well (see `esp-hal-host-tests`). Keep it free of `esp-hal` imports.

/// Bring-up state of the controller, as tracked by the driver.
///
/// The states a host can observe are modelled separately, see
/// [`FunctionState`]: whether a function has been enabled by the host, or is
/// ready, is hardware state that the driver observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum State {
    /// The SDIO block is clocked and out of reset. The SDIO pads are not routed
    /// to the controller, so the host cannot see the card.
    ControllerReady,
    /// The SDIO pads are routed to the controller and the function-ready bits
    /// `IOREADY1/2` are cleared. The clock is running and the block is out of
    /// reset.
    ///
    /// This state only describes what the driver has configured. It does **not**
    /// mean that a host has enumerated the card. No function is ready to transfer
    /// data.
    BusExposed,
    /// (A2a) The SLC0 receive DMA (host to slave) is armed with a linked list and
    /// a buffer, and the driver may declare function 1 ready. The pads are routed.
    ///
    /// The opposite direction cannot be armed until this packet is reclaimed.
    Function1Receiving,
    /// One packet completed. The receive buffer is owned by the CPU.
    Function1Received,
    /// A DMA error or stop timeout. Resources remain owned by the driver.
    Function1ReceiveError,
    /// One slave-to-host packet is published and its DMA owns the storage.
    Function1Transmitting,
    /// The slave-to-host packet reached EOF and its DMA is parked.
    Function1Transmitted,
    /// An outbound DMA fault or stop timeout. Storage remains owned.
    Function1TransmitError,
}

/// State of one SDIO I/O function, as observable on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum FunctionState {
    /// The host has not enabled the function (CCCR `IOEx` is 0).
    Disabled,
    /// The host has enabled the function but the slave has not declared it
    /// ready (CCCR `IORx` is 0).
    EnabledByHost,
    /// The function is enabled and declared ready to transfer.
    ///
    /// Function 1 can enter this state after either DMA direction is armed.
    Ready,
}

/// Error returned by state-dependent operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Error {
    /// The operation is not allowed in the current [`State`].
    InvalidState,
    /// A DMA buffer is empty or larger than 16380 bytes.
    InvalidBuffer,
    /// A descriptor or buffer is outside internal DMA-accessible RAM.
    UnsupportedMemory,
    /// DMA write-back has an invalid owner, EOF, size, or length.
    InvalidDescriptor,
    /// DMA reported an overflow, underflow, or descriptor error.
    DmaFault,
    /// DMA did not become idle within the polling bound.
    DmaStopTimeout,
}

impl State {
    /// Whether the driver may move from `self` to `next`.
    pub const fn can_transition_to(self, next: State) -> bool {
        matches!(
            (self, next),
            (State::ControllerReady, State::BusExposed)
                | (State::BusExposed, State::ControllerReady)
                | (State::BusExposed, State::Function1Receiving)
                | (State::Function1Receiving, State::Function1Received)
                | (State::Function1Receiving, State::Function1ReceiveError)
                | (State::Function1Received, State::ControllerReady)
                | (State::Function1Received, State::BusExposed)
                | (State::Function1Received, State::Function1ReceiveError)
                | (State::Function1ReceiveError, State::ControllerReady)
                | (State::Function1Receiving, State::ControllerReady)
                | (State::BusExposed, State::Function1Transmitting)
                | (State::Function1Transmitting, State::Function1Transmitted)
                | (State::Function1Transmitting, State::Function1TransmitError)
                | (State::Function1Transmitted, State::Function1TransmitError)
                | (State::Function1Transmitting, State::ControllerReady)
                | (State::Function1Transmitted, State::ControllerReady)
                | (State::Function1Transmitted, State::BusExposed)
                | (State::Function1TransmitError, State::ControllerReady)
        )
    }

    /// Whether the SDIO pads are routed to the controller in this state.
    pub const fn pads_routed(self) -> bool {
        !matches!(self, State::ControllerReady)
    }

    /// Whether the driver may declare a function ready (`IOREADY1`) in this
    /// state. Advertising readiness requires an armed DMA direction.
    pub const fn function_ready_allowed(self) -> bool {
        matches!(
            self,
            State::Function1Receiving | State::Function1Transmitting
        )
    }
}

/// Derives the observable state of a function from the two hardware bits.
///
/// `ready` without `enabled` is reported as [`FunctionState::Disabled`]: a
/// function the host has not enabled cannot be used.
pub const fn function_state(enabled_by_host: bool, ready: bool) -> FunctionState {
    match (enabled_by_host, ready) {
        (false, _) => FunctionState::Disabled,
        (true, false) => FunctionState::EnabledByHost,
        (true, true) => FunctionState::Ready,
    }
}

/// Bounds polling without interpreting a request to stop as confirmation.
pub(super) fn wait_for_idle(mut parked: impl FnMut() -> bool) -> bool {
    for _ in 0..10_000 {
        if parked() {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidState => "operation is not allowed in this state",
            Self::InvalidBuffer => {
                "buffer must contain 1..=4095 words and transmit length must fit"
            }
            Self::UnsupportedMemory => "DMA storage must be in internal DRAM",
            Self::InvalidDescriptor => "invalid DMA descriptor write-back",
            Self::DmaFault => "DMA overflow or descriptor fault",
            Self::DmaStopTimeout => "DMA stop did not reach idle within the polling bound",
        })
    }
}

impl core::error::Error for Error {}
