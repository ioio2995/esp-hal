//! ESP32-C6 SDIO slave, A2a: first receive path (host to slave, I/O function 1).
//!
//! Brings the bus up like A1, arms the SLC0 receive DMA with one descriptor and one 512-byte buffer
//! ([`esp_hal::sdio_slave::SdioSlave::arm_receive`]), declares function 1 ready once the host has
//! enabled it, then waits for one CMD53 packet and prints it. One packet only: the DMA is not
//! re-armed.
//!
//! Log lines: `SDIO-A2 C6 <STEP> <RESULT> run=<id> <details>`. A `PASS` here is an observation made
//! by this firmware.
//!
//! Build-time: `SDIO_A1_GIT_SHA`, `SDIO_A1_RUN_ID` (as the A1 firmwares).

//% CHIP_FILTER: esp32c6
//% FEATURES: esp-println/uart

#![no_std]
#![no_main]

use core::{fmt::Write as _, ptr::addr_of_mut};

use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    main,
    sdio_slave::{Descriptor, FunctionState, Pins, SdioSlave},
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::sdio_a1::{a2a_packet_matches, parse_u32};

esp_bootloader_esp_idf::esp_app_desc!();

const GIT_SHA: &str = match option_env!("SDIO_A1_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
const RUN_ID: &str = match option_env!("SDIO_A1_RUN_ID") {
    Some(id) => id,
    None => "unknown",
};

/// `SDIO_A2=2` sends a packet to P4; the default retains the A2a receive test.
const TRANSMIT: bool = parse_u32(option_env!("SDIO_A2"), 1) == 2;

static mut RX_DESCRIPTOR: Descriptor = Descriptor::new();
/// 512 bytes, word aligned (an array of `u32`).
static mut RX_BUFFER: [u32; 128] = [0; 128];

macro_rules! a2 {
    ($step:expr, $result:expr, $($arg:tt)*) => {
        println!("SDIO-A2 C6 {} {} run={} {}", $step, $result, RUN_ID, format_args!($($arg)*))
    };
}

/// Hex dump of up to 64 bytes on one line.
struct Hex<'a>(&'a [u8]);

impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, byte) in self.0.iter().take(64).enumerate() {
            if i > 0 {
                f.write_char(' ')?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    // Console through the esp-hal driver (see sdio-a1-c6-slave).
    let _console = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(115_200),
    )
    .ok()
    .map(|uart| uart.with_tx(peripherals.GPIO16));

    a2!(
        "BOOT",
        "INFO",
        "sha={} fw=sdio-a2-c6-slave scope={} dma=single-packet irq=off",
        GIT_SHA,
        if TRANSMIT { "A2b" } else { "A2a" }
    );

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
    match slave.enable_bus() {
        Ok(()) => a2!("ENABLE_BUS", "PASS", "state={:?}", slave.state()),
        Err(e) => {
            a2!("ENABLE_BUS", "FAIL", "error={:?}", e);
            loop {
                delay.delay_millis(1000);
            }
        }
    }

    // The static storage is handed to the driver once; nothing else touches it afterwards.
    let (descriptor, buffer) = unsafe {
        (
            &mut *addr_of_mut!(RX_DESCRIPTOR),
            &mut *addr_of_mut!(RX_BUFFER),
        )
    };
    if TRANSMIT {
        for (i, word) in buffer.iter_mut().take(16).enumerate() {
            let first = 0x40 + (i * 4) as u8;
            *word = u32::from_le_bytes([first, first + 1, first + 2, first + 3]);
        }
    }
    let armed = if TRANSMIT {
        slave.arm_transmit(descriptor, buffer, 64)
    } else {
        slave.arm_receive(descriptor, buffer)
    };
    match armed {
        Ok(()) => {
            if TRANSMIT {
                a2!(
                    "ARM_TRANSMIT",
                    "PASS",
                    "state={:?} diagnostics={:?}",
                    slave.state(),
                    slave.transmit_diagnostics()
                );
            } else {
                let d = slave.receive_diagnostics();
                a2!(
                    "ARM_RECEIVE",
                    "PASS",
                    "state={:?} descriptor_dw0={:#010x} token={} parked={} int_raw={:#010x}",
                    slave.state(),
                    d.descriptor_dw0,
                    d.token,
                    d.parked,
                    d.interrupt_raw
                );
            }
        }
        Err(e) => {
            a2!("ARM", "FAIL", "error={:?}", e);
            loop {
                delay.delay_millis(1000);
            }
        }
    }

    let mut ready = false;
    let mut received = false;
    let mut ticks = 0u32;
    loop {
        delay.delay_millis(20);
        ticks += 1;

        if !ready && slave.function1() == FunctionState::EnabledByHost {
            match slave.set_function1_ready() {
                Ok(()) => {
                    ready = true;
                    a2!(
                        "FUNCTION1",
                        "PASS",
                        "enabled by the host; IOREADY1 set -> {:?}",
                        slave.function1()
                    );
                }
                Err(e) => a2!("FUNCTION1", "FAIL", "error={:?}", e),
            }
        }

        if !received {
            if TRANSMIT {
                match slave.poll_transmitted() {
                    Ok(Some(length)) => {
                        received = true;
                        a2!(
                            "TX_PACKET",
                            if length == 64 { "PASS" } else { "FAIL" },
                            "length={} diagnostics={:?} DMA completion only; inspect P4 payload verdict",
                            length,
                            slave.transmit_diagnostics()
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        received = true;
                        a2!("TX_PACKET", "FAIL", "error={:?}; no retry", error);
                    }
                }
            } else {
                match slave.poll_received() {
                    Ok(Some(length)) => {
                        received = true;
                        let d = slave.receive_diagnostics();
                        let bytes = slave.received().unwrap_or(&[]);
                        a2!(
                            "RX_PACKET",
                            if a2a_packet_matches(bytes) {
                                "PASS"
                            } else {
                                "FAIL"
                            },
                            "length={} descriptor_dw0={:#010x} int_raw={:#010x} data={}",
                            length,
                            d.descriptor_dw0,
                            d.interrupt_raw,
                            Hex(bytes)
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        received = true;
                        a2!("RX_PACKET", "FAIL", "error={:?}; no retry", error);
                    }
                }
            }
        }

        if ticks % 100 == 0 {
            if TRANSMIT {
                a2!(
                    "ALIVE",
                    "INFO",
                    "tick={} function1={:?} transmitted={} diagnostics={:?}",
                    ticks / 100,
                    slave.function1(),
                    received,
                    slave.transmit_diagnostics()
                );
            } else {
                let d = slave.receive_diagnostics();
                a2!(
                    "ALIVE",
                    "INFO",
                    "tick={} function1={:?} received={} descriptor_dw0={:#010x} token={} int_raw={:#010x}",
                    ticks / 100,
                    slave.function1(),
                    received,
                    d.descriptor_dw0,
                    d.token,
                    d.interrupt_raw
                );
            }
        }
    }
}
