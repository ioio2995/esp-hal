use super::state::*;

const STATES: [State; 3] = [
    State::ControllerReady,
    State::BusExposed,
    State::Function1Receiving,
];

#[test]
fn function_ready_is_only_allowed_with_a_receive_dma() {
    for s in STATES {
        assert_eq!(
            s.function_ready_allowed(),
            s == State::Function1Receiving,
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
        (Function1Receiving, BusExposed),
        (Function1Receiving, ControllerReady),
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
                enabled && s == State::Function1Receiving,
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
