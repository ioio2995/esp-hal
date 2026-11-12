use super::state::*;

const STATES: [State; 2] = [State::ControllerReady, State::BusExposed];

#[test]
fn function_ready_is_never_allowed() {
    for s in STATES {
        assert!(!s.function_ready_allowed(), "{s:?} must not allow IOREADY");
    }
}

#[test]
fn only_bus_exposed_routes_pads() {
    assert!(!State::ControllerReady.pads_routed());
    assert!(State::BusExposed.pads_routed());
}

#[test]
fn transitions_are_limited_to_the_bring_up_cycle() {
    for from in STATES {
        for to in STATES {
            let expected = from != to;
            assert_eq!(from.can_transition_to(to), expected, "{from:?} -> {to:?}");
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
fn bring_up_never_yields_a_ready_function() {
    // With the ready bit forced to the value the driver allows, no host-side
    // enable can ever produce `Ready`.
    for s in STATES {
        for enabled in [false, true] {
            let st = function_state(enabled, s.function_ready_allowed());
            assert_ne!(st, FunctionState::Ready);
        }
    }
}

#[test]
fn error_is_comparable() {
    assert_eq!(Error::InvalidState, Error::InvalidState);
}
