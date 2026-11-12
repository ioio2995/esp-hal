//! Register-free state and invariant logic of the SDIO Slave bring-up.
//!
//! Nothing in this file touches hardware, so it is compiled by the host tests
//! as well (see `esp-hal-host-tests`). Keep it free of `esp-hal` imports.

/// Bring-up state of the controller, as tracked by the driver.
///
/// The states a host can observe are modelled separately, see
/// [`FunctionState`]: whether a function has been enabled by the host, or is
/// ready, is hardware state that the driver only reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum State {
    /// The SDIO block is clocked and out of reset. The SDIO pads are not routed
    /// to the controller, so the host cannot see the card.
    ControllerReady,
    /// The SDIO pads are routed to the controller and the function-ready bits
    /// are cleared: a host may enumerate the card (CMD5, CCCR, CIS). No
    /// function is ready to transfer data.
    BusEnumerable,
}

/// State of one SDIO I/O function, as observable on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum FunctionState {
    /// The host has not enabled the function (CCCR `IOEx` is 0).
    Disabled,
    /// The host has enabled the function but the slave has not declared it
    /// ready (CCCR `IORx` is 0). This is the only enabled state reachable in
    /// the bring-up driver.
    EnabledByHost,
    /// The function is enabled and declared ready to transfer.
    ///
    /// The bring-up driver never causes this state: it never sets `IORx`.
    Ready,
}

/// Error returned by state-dependent operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Error {
    /// The operation is not allowed in the current [`State`].
    InvalidState,
}

impl State {
    /// Whether the driver may move from `self` to `next`.
    pub const fn can_transition_to(self, next: State) -> bool {
        matches!(
            (self, next),
            (State::ControllerReady, State::BusEnumerable)
                | (State::BusEnumerable, State::ControllerReady)
        )
    }

    /// Whether the SDIO pads are routed to the controller in this state.
    pub const fn pads_routed(self) -> bool {
        matches!(self, State::BusEnumerable)
    }

    /// Value of the function-ready bits (`IOREADY1`, `IOREADY2`) that the
    /// driver requires in this state. It is `false` in every state: advertising
    /// a function as ready needs DMA buffers, which the bring-up driver lacks.
    pub const fn function_ready_allowed(self) -> bool {
        false
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
