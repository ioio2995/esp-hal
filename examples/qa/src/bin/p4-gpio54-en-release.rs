//! ESP32-P4: commands the release of the ESP32-C6 EN line through GPIO54, then holds that level.
//!
//! The firmware drives GPIO54 high from the first instant (the output level is written before the driver is enabled, so the pin never
//! drives low), keeps it high, and prints what the P4 itself can observe: the commanded level, the output register read back and the
//! level read from the P4 pad. It never pulses EN, never resets the C6, and does not start SDMMC or SDIO. It makes no statement about
//! the C6: the lines say "commanded" and "P4 pad only", because a register or a pad read is not a measurement of EN at the C6.
//!
//! Board (Waveshare ESP32-P4-WIFI6-POE-ETH, from the published schematic; the board was not probed):
//!
//! | P4 side | Net / part | C6 side |
//! | ------- | ---------- | ------- |
//! | GPIO54 (U7 pin 98) | R54, 0 ohm | `C6_CHIP_PU` = EN, pin 8 of ESP32-C6-MINI-1 (U1) |
//!
//! `C6_CHIP_PU` also carries R13 (10 kohm to 3V3) and C6 (1 uF to ground). EN is active high: HIGH enables the C6, LOW holds it in
//! reset. After a P4 reset the pad is an input with the input buffer disabled (ESP32-P4 TRM, table 10.14-1, reset state 0), so R13
//! pulls EN high and the C6 runs by itself; this firmware makes the release explicit and keeps it. When the P4 is reset or
//! reprogrammed the pad returns to that state: the C6 is then released by the pull-up, not held by this firmware.
//!
//! Console: UART0 of the P4, 115200 baud, selected with `esp-println/uart`. Lines: `P4-EN <STEP> <RESULT> <details>`.
//!
//! Build-time parameters (environment variables, optional):
//!
//! - `P4_EN_GIT_SHA`: source revision printed in the `BOOT` line.
//! - `P4_EN_HOLD_MS`: period of the `HOLD` line, in milliseconds (default 10000).

//% CHIP_FILTER: esp32p4
//% FEATURES: esp-println/uart

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Flex, InputConfig, Level, OutputConfig},
    main,
};
use esp_println::println;
use qa_test::p4_en::{
    EN_PIN,
    Line,
    Outcome,
    READBACK_NOTE,
    RELEASE_NOTE,
    Readback,
    pad_outcome,
    parse_u32,
    release_level,
    release_outcome,
};

esp_bootloader_esp_idf::esp_app_desc!();

const GIT_SHA: &str = match option_env!("P4_EN_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
const HOLD_MS: u32 = parse_u32(option_env!("P4_EN_HOLD_MS"), 10_000);

/// One log line: `P4-EN <STEP> <RESULT> <details>`.
macro_rules! en {
    ($step:expr, $outcome:expr, $($arg:tt)*) => {
        println!("P4-EN {} {} {}", $step, Outcome::as_str($outcome), format_args!($($arg)*))
    };
}

fn line(level: Level) -> Line {
    match level {
        Level::High => Line::High,
        Level::Low => Line::Low,
    }
}

fn level(line: Line) -> Level {
    match line {
        Line::High => Level::High,
        Line::Low => Level::Low,
    }
}

/// Reads back the output register and the pad of the P4 pin.
fn read_back(pin: &Flex<'_>) -> Readback {
    Readback {
        output: line(pin.output_level()),
        pad: if pin.is_high() { Line::High } else { Line::Low },
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    en!(
        "BOOT",
        Outcome::Info,
        "sha={} fw=p4-gpio54-en-release scope=gpio54-only resets=none hold_ms={}",
        GIT_SHA,
        HOLD_MS
    );

    // `Flex::new` puts the pad in a known GPIO state with the output driver disabled.
    let mut en = Flex::new(peripherals.GPIO54);
    // Write the release level first, enable the driver after: the pin goes from high impedance to HIGH, never through LOW.
    en.set_level(level(release_level()));
    en.apply_output_config(&OutputConfig::default());
    en.set_output_enable(true);
    en.apply_input_config(&InputConfig::default());
    en.set_input_enable(true);
    en!(
        "GPIO54_INIT",
        Outcome::Pass,
        "pin={} mode=push-pull pull=none pad_held={} (configuration calls completed)",
        EN_PIN,
        en.is_pad_held() as u8
    );

    let first = read_back(&en);
    en!(
        "EN_RELEASE_COMMANDED",
        release_outcome(first),
        "level={} output_register={} ({})",
        release_level().as_str(),
        first.output.as_str(),
        RELEASE_NOTE
    );
    en!("PAD_READBACK", pad_outcome(first), "pad={} commanded={} ({})", first.pad.as_str(), first.output.as_str(), READBACK_NOTE);

    // Hold: read back periodically. Nothing is written again, so EN is never pulsed.
    let mut ticks: u32 = 0;
    let mut reported_change = false;
    loop {
        delay.delay_millis(HOLD_MS);
        ticks = ticks.wrapping_add(1);
        let now = read_back(&en);
        if now.output != first.output && !reported_change {
            reported_change = true;
            en!(
                "OUTPUT_CHANGED",
                Outcome::Fail,
                "output_register={} expected={} (nothing in this firmware writes it again)",
                now.output.as_str(),
                first.output.as_str()
            );
        }
        en!(
            "HOLD",
            if now == first { Outcome::Info } else { pad_outcome(now) },
            "level={} tick={} pad={} ({})",
            now.output.as_str(),
            ticks,
            now.pad.as_str(),
            READBACK_NOTE
        );
    }
}
