//! Single-packet SLC0 interrupt and Embassy completion check.
//! `SDIO_A2=2` sends 64 bytes (40..7f); the default receives A2a (a0..df).
//! This firmware is prepared for a later hardware check. Building it runs no test.

//% CHIP_FILTER: esp32c6
//% FEATURES: esp-println/uart esp-rtos

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer, with_timeout};
use esp_backtrace as _;
use esp_hal::{
    sdio_slave::{Descriptor, FunctionState, Pins, SdioSlave},
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::sdio_a1::{a2a_packet_matches, parse_u32};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

static DESCRIPTOR: StaticCell<Descriptor> = StaticCell::new();
static BUFFER: StaticCell<[u32; 128]> = StaticCell::new();
const TRANSMIT: bool = parse_u32(option_env!("SDIO_A2"), 1) == 2;
const GIT_SHA: &str = match option_env!("SDIO_A1_GIT_SHA") {
    Some(value) => value,
    None => "unknown",
};
const RUN_ID: &str = match option_env!("SDIO_A1_RUN_ID") {
    Some(value) => value,
    None => "unknown",
};

#[esp_hal::main]
async fn main(_spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let timers = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timers.timer0);
    let _console = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(115_200),
    )
    .ok()
    .map(|uart| uart.with_tx(peripherals.GPIO16));
    println!(
        "SDIO-A4 C6 BOOT INFO run={} sha={} transmit={} irq=SLC0",
        RUN_ID, GIT_SHA, TRANSMIT
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
    )
    .into_async();
    slave.enable_bus().unwrap();
    let descriptor = DESCRIPTOR.init(Descriptor::new());
    let buffer = BUFFER.init([0; 128]);
    if TRANSMIT {
        for (i, word) in buffer.iter_mut().take(16).enumerate() {
            let first = 0x40 + (i * 4) as u8;
            *word = u32::from_le_bytes([first, first + 1, first + 2, first + 3]);
        }
        slave.arm_transmit(descriptor, buffer, 64).unwrap();
    } else {
        slave.arm_receive(descriptor, buffer).unwrap();
    }
    println!(
        "SDIO-A4 C6 ARMED INFO transmit={} irq=SLC0 slc1=off",
        TRANSMIT
    );
    let enabled = with_timeout(Duration::from_secs(30), async {
        while slave.function1() != FunctionState::EnabledByHost {
            Timer::after_millis(10).await;
        }
    })
    .await;
    if enabled.is_ok() {
        slave.set_function1_ready().unwrap();
        let result = with_timeout(Duration::from_secs(10), async {
            if TRANSMIT {
                slave.wait_transmitted().await
            } else {
                slave.wait_received().await
            }
        })
        .await;
        match result {
            Ok(Ok(64)) if TRANSMIT => {
                println!("SDIO-A4 C6 TX PASS length=64; inspect P4 payload verdict")
            }
            Ok(Ok(64)) if slave.received().is_some_and(a2a_packet_matches) => {
                println!("SDIO-A4 C6 RX PASS length=64 payload=match")
            }
            other => println!("SDIO-A4 C6 TRANSFER FAIL result={:?}", other),
        }
    } else {
        println!("SDIO-A4 C6 ENABLE FAIL timeout; no retry");
    }
    // A timed-out future has only masked IRQ notification. Stop DMA explicitly.
    match slave.disable_bus() {
        Ok(()) => {
            if TRANSMIT {
                let _ = slave.take_transmit_buffers().unwrap();
            } else {
                let _ = slave.take_receive_buffers().unwrap();
            }
            println!("SDIO-A4 C6 STOP PASS DMA idle confirmed");
        }
        Err(error) => println!("SDIO-A4 C6 STOP FAIL error={:?}; storage retained", error),
    }
    loop {
        Timer::after_secs(1).await;
    }
}
