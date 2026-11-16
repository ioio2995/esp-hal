//! Protocol shared by the SDIO QA firmwares `sdio-qa-host` (ESP32-P4) and `sdio-qa-slave`
//! (ESP32-C6).
//!
//! The two firmwares share no side channel other than the SDIO bus. They advance in lockstep
//! through bus-visible state only: the cumulative receive credit (host register 0x44), the
//! cumulative published length (host register 0x60) and the CCCR `IOR1` readiness bit. The
//! scenario is chosen at build time with `SDIO_QA_SCENARIO`.
//!
//! This module touches no hardware so that its arithmetic can be checked on the host.

/// Scenario compiled into the firmware (`SDIO_QA_SCENARIO`, default `h1`).
pub const SCENARIO_NAME: &str = match option_env!("SDIO_QA_SCENARIO") {
    Some(name) => name,
    None => "h1",
};

/// Scenario selected at build time.
pub const SCENARIO: Scenario = match Scenario::parse(SCENARIO_NAME) {
    Some(scenario) => scenario,
    None => panic!("SDIO_QA_SCENARIO must be one of: h1, h2, h3, h4, h567"),
};

/// Hardware scenarios of the qualification mandate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// 64-byte round trip, repeated by the host over fresh sessions.
    H1,
    /// Three round trips in one session with explicit storage reuse.
    H2,
    /// Exactly 65 bytes from the slave to the host.
    H3,
    /// Cancel an async wait in both directions, then resume.
    H4,
    /// Stop an unused receive (H5), an unconsumed transmit (H6), then resynchronize (H7).
    H567,
}

impl Scenario {
    /// Parses a scenario name in a const context.
    pub const fn parse(name: &str) -> Option<Self> {
        let name = name.as_bytes();
        if bytes_eq(name, b"h1") {
            Some(Self::H1)
        } else if bytes_eq(name, b"h2") {
            Some(Self::H2)
        } else if bytes_eq(name, b"h3") {
            Some(Self::H3)
        } else if bytes_eq(name, b"h4") {
            Some(Self::H4)
        } else if bytes_eq(name, b"h567") {
            Some(Self::H567)
        } else {
            None
        }
    }
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Length of every packet except H3.
pub const PACKET_LENGTH: usize = 64;
/// Length of the H3 packet: not a multiple of 4 and not a multiple of 64.
pub const ODD_LENGTH: usize = 65;
/// Offset of the SDIO window end. A CMD53 for `n` bytes uses `WINDOW_END - n`.
pub const WINDOW_END: u32 = 0x1_f800;
/// Receive credits are cumulative modulo 4096.
pub const CREDIT_MASK: u32 = 0xfff;
/// Published slave-to-host length is cumulative modulo 2^20.
pub const LENGTH_MASK: u32 = 0xf_ffff;
/// Bound, in milliseconds, of every wait on the peer.
pub const PEER_TIMEOUT_MS: u64 = 10_000;
/// Time the slave keeps an unconsumed transfer armed and ready before stopping the bus.
pub const OBSERVATION_MS: u64 = 1_500;
/// Time an async wait is left pending before it is cancelled (H4).
pub const CANCEL_AFTER_MS: u64 = 300;
/// Time the slave leaves the pads released between two sessions.
pub const SESSION_GAP_MS: u64 = 2_000;

/// Byte `i` of the host request number `k`. Request 0 of H1 is the historical `0..64` ramp.
pub const fn request_byte(scenario: Scenario, k: u32, i: usize) -> u8 {
    match scenario {
        Scenario::H1 => i as u8,
        _ => (i as u32)
            .wrapping_mul(13)
            .wrapping_add(k.wrapping_mul(41))
            .wrapping_add(0x5a) as u8,
    }
}

/// Byte `i` of the slave response number `k`.
///
/// H1 echoes the request, as the public example does. Other scenarios use a formula independent
/// of the request, so that an echo or a previous response cannot pass.
pub const fn response_byte(scenario: Scenario, k: u32, i: usize) -> u8 {
    match scenario {
        Scenario::H1 => request_byte(scenario, k, i),
        _ => (i as u32)
            .wrapping_mul(29)
            .wrapping_add(k.wrapping_mul(53))
            .wrapping_add(0x33) as u8,
    }
}

/// Byte `i` of the 65-byte H3 packet.
pub const fn odd_byte(i: usize) -> u8 {
    (i as u32).wrapping_mul(3).wrapping_add(0x11) as u8
}

/// Value of an untouched buffer byte, used to expose bytes beyond the announced length.
pub const CANARY: u8 = 0xee;

/// Credits published since `before`, modulo 4096.
pub const fn credit_delta(now: u32, before: u32) -> u32 {
    now.wrapping_sub(before) & CREDIT_MASK
}

/// Bytes published since `before`, modulo 2^20.
pub const fn length_delta(now: u32, before: u32) -> u32 {
    now.wrapping_sub(before) & LENGTH_MASK
}

/// Adds to a cumulative credit count.
pub const fn credit_add(base: u32, n: u32) -> u32 {
    base.wrapping_add(n) & CREDIT_MASK
}

/// Adds to a cumulative length.
pub const fn length_add(base: u32, n: u32) -> u32 {
    base.wrapping_add(n) & LENGTH_MASK
}

/// Index of the first differing byte, or the shorter length if one slice is a prefix.
pub fn first_mismatch(a: &[u8], b: &[u8]) -> Option<usize> {
    let common = a.len().min(b.len());
    (0..common)
        .find(|&i| a[i] != b[i])
        .or((a.len() != b.len()).then_some(common))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_names() {
        assert_eq!(Scenario::parse("h1"), Some(Scenario::H1));
        assert_eq!(Scenario::parse("h567"), Some(Scenario::H567));
        assert_eq!(Scenario::parse("h5"), None);
        assert_eq!(Scenario::parse(""), None);
    }

    #[test]
    fn h1_matches_public_example() {
        for i in 0..PACKET_LENGTH {
            assert_eq!(request_byte(Scenario::H1, 0, i), i as u8);
            assert_eq!(response_byte(Scenario::H1, 0, i), i as u8);
        }
    }

    #[test]
    fn patterns_change_with_every_packet() {
        for scenario in [Scenario::H2, Scenario::H4, Scenario::H567] {
            for k in 0..8u32 {
                let a: [u8; PACKET_LENGTH] = core::array::from_fn(|i| request_byte(scenario, k, i));
                let b: [u8; PACKET_LENGTH] =
                    core::array::from_fn(|i| request_byte(scenario, k + 1, i));
                let r: [u8; PACKET_LENGTH] =
                    core::array::from_fn(|i| response_byte(scenario, k, i));
                assert_ne!(a, b, "{scenario:?} request {k}");
                assert_ne!(a, r, "{scenario:?} response must not echo request {k}");
                let r2: [u8; PACKET_LENGTH] =
                    core::array::from_fn(|i| response_byte(scenario, k + 1, i));
                assert_ne!(r, r2, "{scenario:?} response {k}");
            }
        }
    }

    #[test]
    fn odd_packet_is_not_the_canary_and_not_periodic_at_64() {
        assert!((0..ODD_LENGTH).any(|i| odd_byte(i) != CANARY));
        assert_ne!(odd_byte(0), odd_byte(64) ^ 0xff);
        assert_ne!(odd_byte(64), CANARY);
    }

    #[test]
    fn cumulative_counters_wrap() {
        assert_eq!(credit_delta(1, 0), 1);
        assert_eq!(credit_delta(0, 4095), 1);
        assert_eq!(credit_add(4095, 1), 0);
        assert_eq!(length_delta(63, LENGTH_MASK - 0), 64);
        assert_eq!(length_add(LENGTH_MASK, 1), 0);
    }

    #[test]
    fn mismatch_reports_first_difference_or_length() {
        assert_eq!(first_mismatch(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(first_mismatch(&[1, 2, 3], &[1, 9, 9]), Some(1));
        assert_eq!(first_mismatch(&[1, 2], &[1, 2, 3]), Some(2));
    }
}
