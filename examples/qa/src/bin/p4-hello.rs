//! ESP32-P4 minimal diagnostic: says hello on UART0 and on USB-Serial-JTAG, nothing else.
//!
//! Purpose: tell apart "an esp-hal application does not start behind the factory bootloader" from a
//! problem specific to the SDIO A1 firmware. It touches no SDMMC, no SDIO and **no GPIO54** (the C6
//! EN line stays as the board leaves it).
//!
//! Stages (each line starts with `P4-HELLO`):
//! - `STAGE0` is printed on UART0 **before** `esp_hal::init`, through `esp-println` (raw UART0
//!   FIFO, baud as left by the ROM).
//! - `STAGE1` after `esp_hal::init`, on UART0 and on USB-Serial-JTAG.
//! - `ALIVE n` every second on both. The USB-Serial-JTAG writes are non-blocking and bounded:
//!   without a host reading that port they are skipped, so the UART0 output never stops because of
//!   them.
//!
//! Reading: seeing STAGE0 only means the bootloader reached the application entry but
//! `esp_hal::init` (or a later step) failed; STAGE1 garbled on UART0 but clean on USB means a UART0
//! baud/clock change after `init`; nothing at all means the application never ran (bootloader
//! hand-over). Lines only say what this firmware printed, never that the board is healthy.
//!
//! Build-time: `P4_HELLO_GIT_SHA` (source revision printed in `STAGE0`).

//% CHIP_FILTER: esp32p4
//% FEATURES: esp-println/uart

#![no_std]
#![no_main]

use core::fmt::Write as _;

use esp_backtrace as _;
use esp_hal::{delay::Delay, main, time::Instant, usb::usb_serial_jtag::UsbSerialJtag};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

const GIT_SHA: &str = match option_env!("P4_HELLO_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// Small fixed line buffer (no allocation).
struct Line {
    bytes: [u8; 128],
    len: usize,
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.bytes.len() - self.len;
        let take = s.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// Bounded, non-blocking USB-Serial-JTAG write: gives up as soon as the FIFO is not ready.
fn usb_try(usb: &mut UsbSerialJtag<'_, esp_hal::Blocking>, bytes: &[u8]) {
    for byte in bytes {
        let mut spins = 0u32;
        while usb.write_byte_nb(*byte).is_err() {
            spins += 1;
            if spins > 20_000 {
                return;
            }
        }
    }
    let _ = usb.flush_tx_nb();
}

#[main]
fn main() -> ! {
    println!(
        "P4-HELLO STAGE0 pre-init sha={} fw=p4-hello console=uart0-raw",
        GIT_SHA
    );

    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();
    let mut usb = UsbSerialJtag::new(peripherals.USB_DEVICE);

    let revision = esp_hal::efuse::chip_revision();
    let mut line = Line {
        bytes: [0; 128],
        len: 0,
    };
    let _ = write!(
        line,
        "P4-HELLO STAGE1 post-init sha={} chip_rev=v{}.{} (UART0 and USB-Serial-JTAG)\r\n",
        GIT_SHA, revision.major, revision.minor
    );
    println!(
        "{}",
        core::str::from_utf8(&line.bytes[..line.len])
            .unwrap_or("?")
            .trim_end()
    );
    usb_try(&mut usb, &line.bytes[..line.len]);

    let mut n: u32 = 0;
    loop {
        delay.delay_millis(1000);
        n = n.wrapping_add(1);
        let ms = Instant::now().duration_since_epoch().as_millis();
        let mut line = Line {
            bytes: [0; 128],
            len: 0,
        };
        let _ = write!(line, "P4-HELLO ALIVE n={} uptime_ms={}\r\n", n, ms);
        println!(
            "{}",
            core::str::from_utf8(&line.bytes[..line.len])
                .unwrap_or("?")
                .trim_end()
        );
        usb_try(&mut usb, &line.bytes[..line.len]);
    }
}
