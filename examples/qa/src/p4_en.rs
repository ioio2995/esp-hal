//! Register-free logic of the ESP32-P4 firmware that drives GPIO54, the line wired to the ESP32-C6 EN pin.
//!
//! Nothing here touches hardware, so the host tests of `esp-hal-host-tests` compile this file as well. Keep it free of `esp-hal`
//! imports.
//!
//! Board facts (Waveshare ESP32-P4-WIFI6-POE-ETH schematic, read from the published PDF; the board itself has not been probed):
//! GPIO54 of the P4 (U7 pin 98) goes through R54 (0 ohm) to the net `C6_CHIP_PU`, which is the EN pin (pin 8) of the ESP32-C6-MINI-1
//! module (U1), with R13 (10 kohm) to 3V3 and C6 (1 uF) to ground. EN is active high: a high level enables the chip, a low level holds
//! it in reset. After a reset of the P4 the pad is an input with the input buffer disabled (ESP32-P4 TRM, table 10.14-1, "Reset" 0):
//! high impedance, so R13 pulls EN high and the C6 is released.

/// Pin driven by the firmware.
pub const EN_PIN: u8 = 54;

/// Logic level of a line, as the firmware commands or reads it. This is not a voltage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// Low level.
    Low,
    /// High level.
    High,
}

impl Line {
    /// Text used in the log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Line::Low => "LOW",
            Line::High => "HIGH",
        }
    }
}

/// Level that releases the C6 (EN is active high on this board).
pub const fn release_level() -> Line {
    Line::High
}

/// Level that would hold the C6 in reset. The firmware never commands it.
pub const fn reset_level() -> Line {
    Line::Low
}

/// Result of a step, as printed in the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The step was observed to succeed on the P4 side.
    Pass,
    /// The step was observed to fail on the P4 side.
    Fail,
    /// Information only.
    Info,
}

impl Outcome {
    /// Text used in the log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Info => "INFO",
        }
    }
}

/// What the firmware read back from the P4 itself after commanding the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readback {
    /// Level held in the output register of the pin.
    pub output: Line,
    /// Level read from the pad through the input buffer. This is the P4 pad, not the C6 EN pin.
    pub pad: Line,
}

/// Verdict on the commanded release, from the output register read back.
pub const fn release_outcome(readback: Readback) -> Outcome {
    match (readback.output, release_level()) {
        (Line::High, Line::High) | (Line::Low, Line::Low) => Outcome::Pass,
        _ => Outcome::Fail,
    }
}

/// Verdict on the pad level: it must follow the commanded level. A difference means that something else drives the pad, or that the
/// pad is shorted: an anomaly of the P4 side, not a statement about the C6.
pub const fn pad_outcome(readback: Readback) -> Outcome {
    match (readback.pad, readback.output) {
        (Line::High, Line::High) | (Line::Low, Line::Low) => Outcome::Pass,
        _ => Outcome::Fail,
    }
}

/// Words that a log line of this firmware must never contain, because the firmware cannot observe them: the C6 booting, EN being
/// measured, the qualification being a success.
pub const FORBIDDEN_CLAIMS: [&str; 6] = ["booted", "started", "running", "measured", "qualified", "c6 is"];

/// Note attached to every readback line.
pub const READBACK_NOTE: &str = "P4 pad only; not a measurement of C6 EN";

/// Note attached to the release line.
pub const RELEASE_NOTE: &str = "commanded; the C6 state is not observed";

/// Whether a log text makes a claim that this firmware cannot support.
pub fn makes_forbidden_claim(text: &str) -> bool {
    let lower = text.as_bytes();
    FORBIDDEN_CLAIMS.iter().any(|word| contains_ignore_case(lower, word.as_bytes()))
}

fn contains_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

/// Parses a decimal `u32` at compile time, for `option_env!` parameters. Returns `default` for a missing, empty or invalid text.
pub const fn parse_u32(text: Option<&str>, default: u32) -> u32 {
    let Some(text) = text else {
        return default;
    };
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return default;
    }
    let mut value: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            return default;
        }
        value = match value.checked_mul(10) {
            Some(v) => match v.checked_add((b - b'0') as u32) {
                Some(v) => v,
                None => return default,
            },
            None => return default,
        };
        i += 1;
    }
    value
}
