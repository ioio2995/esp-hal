use super::state::*;

const STATES: [State; 8] = [
    State::ControllerReady,
    State::BusExposed,
    State::Function1Receiving,
    State::Function1Received,
    State::Function1ReceiveError,
    State::Function1Transmitting,
    State::Function1Transmitted,
    State::Function1TransmitError,
];

#[test]
fn function_ready_is_only_allowed_with_a_receive_dma() {
    for s in STATES {
        assert_eq!(
            s.function_ready_allowed(),
            matches!(s, State::Function1Receiving | State::Function1Transmitting),
            "{s:?}"
        );
    }
}

#[test]
fn pads_are_routed_except_in_controller_ready() {
    assert!(!State::ControllerReady.pads_routed());
    assert!(State::BusExposed.pads_routed());
    assert!(State::Function1Receiving.pads_routed());
}

#[test]
fn transitions_are_limited_to_the_documented_graph() {
    use State::*;
    let allowed = [
        (ControllerReady, BusExposed),
        (BusExposed, ControllerReady),
        (BusExposed, Function1Receiving),
        (Function1Receiving, Function1Received),
        (Function1Receiving, Function1ReceiveError),
        (Function1Received, ControllerReady),
        (Function1Received, BusExposed),
        (Function1Received, Function1ReceiveError),
        (Function1ReceiveError, ControllerReady),
        (Function1Receiving, ControllerReady),
        (BusExposed, Function1Transmitting),
        (Function1Transmitting, Function1Transmitted),
        (Function1Transmitting, Function1TransmitError),
        (Function1Transmitted, Function1TransmitError),
        (Function1Transmitting, ControllerReady),
        (Function1Transmitted, ControllerReady),
        (Function1Transmitted, BusExposed),
        (Function1TransmitError, ControllerReady),
    ];
    for from in STATES {
        for to in STATES {
            assert_eq!(
                from.can_transition_to(to),
                allowed.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
}

#[test]
fn function_state_distinguishes_enabled_from_ready() {
    assert_eq!(function_state(false, false), FunctionState::Disabled);
    assert_eq!(function_state(true, false), FunctionState::EnabledByHost);
    assert_eq!(function_state(true, true), FunctionState::Ready);
}

#[test]
fn ready_without_host_enable_is_not_usable() {
    assert_eq!(function_state(false, true), FunctionState::Disabled);
}

#[test]
fn only_the_receiving_state_can_yield_a_ready_function() {
    for s in STATES {
        for enabled in [false, true] {
            let st = function_state(enabled, s.function_ready_allowed());
            assert_eq!(
                st == FunctionState::Ready,
                enabled && matches!(s, State::Function1Receiving | State::Function1Transmitting),
                "{s:?} enabled={enabled}"
            );
        }
    }
}

#[test]
fn error_is_comparable() {
    assert_eq!(Error::InvalidState, Error::InvalidState);
    assert_ne!(Error::InvalidState, Error::InvalidBuffer);
}

#[test]
fn stop_waits_for_idle_and_reports_timeout() {
    let mut polls = 0;
    assert!(wait_for_idle(|| {
        polls += 1;
        polls == 4
    }));
    assert_eq!(polls, 4);
    let mut polls = 0;
    assert!(!wait_for_idle(|| {
        polls += 1;
        false
    }));
    assert_eq!(polls, 10_000);
}

#[test]
fn token_no_replace_sets_bit_zero_without_changing_other_bits() {
    for bits in [0, 2, 0x0203_701a, u32::MAX] {
        let updated = super::receive::token_no_replace(bits);
        assert_eq!(updated & 1, 1);
        assert_eq!(updated & !1, bits & !1);
    }
}
