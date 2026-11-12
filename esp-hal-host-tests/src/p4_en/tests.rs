use super::logic::*;

#[test]
fn the_release_level_is_high_and_the_reset_level_low() {
    assert_eq!(EN_PIN, 54);
    assert_eq!(release_level(), Line::High);
    assert_eq!(reset_level(), Line::Low);
    assert_ne!(release_level(), reset_level());
}

#[test]
fn the_release_passes_only_when_the_output_register_reads_back_high() {
    for pad in [Line::Low, Line::High] {
        assert_eq!(release_outcome(Readback { output: Line::High, pad }), Outcome::Pass);
        assert_eq!(release_outcome(Readback { output: Line::Low, pad }), Outcome::Fail);
    }
}

#[test]
fn the_pad_must_follow_the_commanded_level() {
    assert_eq!(pad_outcome(Readback { output: Line::High, pad: Line::High }), Outcome::Pass);
    assert_eq!(pad_outcome(Readback { output: Line::High, pad: Line::Low }), Outcome::Fail);
    assert_eq!(pad_outcome(Readback { output: Line::Low, pad: Line::Low }), Outcome::Pass);
    assert_eq!(pad_outcome(Readback { output: Line::Low, pad: Line::High }), Outcome::Fail);
}

#[test]
fn a_pad_anomaly_does_not_cancel_the_register_verdict() {
    let r = Readback { output: Line::High, pad: Line::Low };
    assert_eq!(release_outcome(r), Outcome::Pass);
    assert_eq!(pad_outcome(r), Outcome::Fail);
}

#[test]
fn log_texts_use_the_expected_words() {
    assert_eq!(Line::High.as_str(), "HIGH");
    assert_eq!(Line::Low.as_str(), "LOW");
    assert_eq!(Outcome::Pass.as_str(), "PASS");
    assert_eq!(Outcome::Fail.as_str(), "FAIL");
    assert_eq!(Outcome::Info.as_str(), "INFO");
}

#[test]
fn the_notes_make_no_claim_about_the_c6_or_a_measurement() {
    assert!(!makes_forbidden_claim(READBACK_NOTE), "{READBACK_NOTE}");
    assert!(!makes_forbidden_claim(RELEASE_NOTE), "{RELEASE_NOTE}");
    assert!(READBACK_NOTE.contains("not a measurement"));
    assert!(RELEASE_NOTE.contains("not observed"));
}

#[test]
fn the_claim_detector_catches_what_the_firmware_cannot_know() {
    for text in ["the C6 booted", "C6 is running", "EN measured HIGH", "STARTED", "qualified"] {
        assert!(makes_forbidden_claim(text), "{text}");
    }
    assert!(!makes_forbidden_claim("level=HIGH pad=HIGH"));
    assert!(!makes_forbidden_claim(""));
}

#[test]
fn parse_u32_accepts_decimal_and_falls_back() {
    assert_eq!(parse_u32(Some("10000"), 5), 10000);
    assert_eq!(parse_u32(None, 5), 5);
    assert_eq!(parse_u32(Some(""), 5), 5);
    assert_eq!(parse_u32(Some("1x"), 5), 5);
    assert_eq!(parse_u32(Some("99999999999"), 5), 5);
}
