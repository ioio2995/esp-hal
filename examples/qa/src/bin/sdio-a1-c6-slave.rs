//! SDIO Slave A1 qualification firmware, ESP32-C6 side.
//!
//! Runs the real `esp_hal::sdio_slave` bring-up driver and reports what it observes on the UART
//! console, so that an operator can pair these lines with the log of the ESP32-P4 host firmware
//! (`sdio-a1-p4-host`). The firmware starts no DMA engine, enables no SLC interrupt, never sets
//! `IOREADY1`/`IOREADY2`, and does not answer any SDIO command by software: the hardware does.
//!
//! This firmware never prints a global verdict. `PASS` in a line means that the step was observed
//! to succeed on this chip; whether a host can enumerate the card is shown by the P4 log.
//!
//! Console: UART0 (GPIO16 TX, GPIO17 RX), 115200 baud, selected with `esp-println/uart`.
//!
//! Pins (SDIO function 0 of IO MUX, fixed):
//!
//! | Signal | ESP32-C6 |
//! | ------ | -------- |
//! | CMD    | GPIO18   |
//! | CLK    | GPIO19   |
//! | DATA0  | GPIO20   |
//! | DATA1  | GPIO21   |
//! | DATA2  | GPIO22   |
//! | DATA3  | GPIO23   |
//!
//! Build-time parameters (environment variables, all optional):
//!
//! - `SDIO_A1_GIT_SHA`: source revision printed in every header line.
//! - `SDIO_A1_RUN_ID`: identifier of the trial.
//! - `SDIO_A1_MONITOR_MS`: polling period of the register monitor (default 500).
//! - `SDIO_A1_ALIVE_MS`: period of the `ALIVE` line (default 10000).

//% CHIP_FILTER: esp32c6
//% FEATURES: esp-println/uart

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    main,
    sdio_slave::{FunctionState, Pins, RegisterSnapshot, SdioSlave, State},
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::sdio_a1::{Outcome, ioenable_bits, ioready_bits, parse_u32};

esp_bootloader_esp_idf::esp_app_desc!();

const GIT_SHA: &str = match option_env!("SDIO_A1_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
const RUN_ID: &str = match option_env!("SDIO_A1_RUN_ID") {
    Some(id) => id,
    None => "unset",
};
const MONITOR_MS: u32 = parse_u32(option_env!("SDIO_A1_MONITOR_MS"), 500);
const ALIVE_MS: u32 = parse_u32(option_env!("SDIO_A1_ALIVE_MS"), 10_000);

/// One log line: `SDIO-A1 <MCU> <STEP> <RESULT> <details>`.
macro_rules! a1 {
    ($step:expr, $outcome:expr, $($arg:tt)*) => {
        println!(
            "SDIO-A1 C6 {} {} run={} {}",
            $step,
            Outcome::as_str($outcome),
            RUN_ID,
            format_args!($($arg)*)
        )
    };
}

fn print_snapshot(step: &str, s: &RegisterSnapshot) {
    a1!(
        step,
        Outcome::Info,
        "cfg_data1={:#010x} cfg_data7={:#010x} conf_status={:#010x} slc0int_raw={:#010x} \
         slc1int_raw={:#010x}",
        s.cfg_data1,
        s.cfg_data7,
        s.function0_status,
        s.slc0_interrupt_raw,
        s.slc1_interrupt_raw
    );
}

/// Verdict on the function-ready bits: they must be clear in A1.
fn check_ioready(step: &str, s: &RegisterSnapshot) -> bool {
    let (r1, r2) = ioready_bits(s.cfg_data1);
    let clear = !r1 && !r2;
    a1!(
        step,
        if clear { Outcome::Pass } else { Outcome::Fail },
        "ioready1={} ioready2={}",
        r1 as u8,
        r2 as u8
    );
    clear
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    // Console: set UART0 up through the esp-hal driver (needed behind the factory ESP-IDF bootloader on the P4, see
    // sdio-a1-p4-host). TX = GPIO16, the pin wired to the H7 console header. Does not touch the SDIO pads GPIO18..23.
    let _console = Uart::new(peripherals.UART0, UartConfig::default().with_baudrate(115_200))
        .ok()
        .map(|uart| uart.with_tx(peripherals.GPIO16));

    a1!(
        "BOOT",
        Outcome::Info,
        "sha={} fw=sdio-a1-c6-slave scope=A1 dma=off irq=off ioready=never",
        GIT_SHA
    );

    // Takes the clock of the SDIO block and resets it. Pads are not routed yet.
    let mut slave = SdioSlave::new(
        peripherals.HINF,
        peripherals.SLC,
        peripherals.SLCHOST,
        Pins::new(
            peripherals.GPIO18,
            peripherals.GPIO19,
            peripherals.GPIO20,
            peripherals.GPIO21,
            peripherals.GPIO22,
            peripherals.GPIO23,
        ),
    );
    let state = slave.state();
    a1!(
        "DRIVER_NEW",
        if state == State::ControllerReady { Outcome::Pass } else { Outcome::Fail },
        "state={:?}",
        state
    );

    let pre = slave.snapshot();
    print_snapshot("SNAPSHOT_PRE", &pre);
    check_ioready("IOREADY_PRE", &pre);

    match slave.enable_bus() {
        Ok(()) => a1!(
            "ENABLE_BUS",
            Outcome::Pass,
            "driver returned Ok; this does not prove that a host can enumerate the card"
        ),
        Err(e) => a1!("ENABLE_BUS", Outcome::Fail, "error={:?}", e),
    }

    let state = slave.state();
    a1!(
        "STATE",
        if state == State::BusExposed { Outcome::Pass } else { Outcome::Fail },
        "state={:?} expected={:?}",
        state,
        State::BusExposed
    );

    let post = slave.snapshot();
    print_snapshot("SNAPSHOT_POST", &post);
    check_ioready("IOREADY_POST", &post);
    a1!(
        "FUNCTIONS",
        Outcome::Info,
        "function1={:?} function2={:?}",
        slave.function1(),
        slave.function2()
    );
    a1!(
        "LISTENING",
        Outcome::Info,
        "waiting for the host; reads below never clear or acknowledge anything"
    );

    // Monitor: print on change, and an `ALIVE` line periodically.
    let mut last = post;
    let mut ticks: u32 = 0;
    let mut since_alive: u32 = 0;
    let mut ready_reported = false;
    loop {
        delay.delay_millis(MONITOR_MS);
        since_alive = since_alive.saturating_add(MONITOR_MS);

        let now = slave.snapshot();
        if now != last {
            let (e1, e2) = ioenable_bits(now.cfg_data1);
            a1!(
                "CHANGE",
                Outcome::Info,
                "ioenable1={} ioenable2={} function1={:?} function2={:?}",
                e1 as u8,
                e2 as u8,
                slave.function1(),
                slave.function2()
            );
            print_snapshot("CHANGE_REGS", &now);
            last = now;
        }

        let (r1, r2) = ioready_bits(now.cfg_data1);
        let ready = r1
            || r2
            || slave.function1() == FunctionState::Ready
            || slave.function2() == FunctionState::Ready;
        if ready && !ready_reported {
            a1!(
                "IOREADY_VIOLATION",
                Outcome::Fail,
                "a function-ready bit is set while the A1 contract forbids it"
            );
            ready_reported = true;
        }

        if since_alive >= ALIVE_MS {
            since_alive = 0;
            ticks = ticks.wrapping_add(1);
            a1!("ALIVE", Outcome::Info, "tick={} state={:?}", ticks, slave.state());
        }
    }
}
