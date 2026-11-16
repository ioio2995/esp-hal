//! SDIO slave QA firmware for the ESP32-C6, companion of `sdio-qa-host` (ESP32-P4).
//!
//! The scenario is selected at build time with `SDIO_QA_SCENARIO` (`h1`, `h2`, `h3`, `h4`,
//! `h567`). See `examples/qa/SDIO-QA.md` for the contracts and the log format.
//!
//! The firmware only uses the public driver API. It never reads or writes a buffer while the
//! DMA owns it, and it never calls a function that would fabricate a completion.
//!
//! Terminology: SLC0 TX receives from the host, SLC0 RX transmits to the host.
//!
//! Pins: CMD GPIO18, CLK GPIO19, DATA0..3 GPIO20..23 (fixed IO MUX), console UART0 TX GPIO16.

//% CHIP_FILTER: esp32c6

#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_backtrace as _;
use esp_hal::{
    Async,
    peripherals::{GPIO18, GPIO19, GPIO20, GPIO21, GPIO22, GPIO23, HINF, SLC, SLCHOST},
    sdio_slave::{Descriptor, Error, FunctionState, SdioSlave, State},
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::sdio_qa::{
    CANARY,
    CANCEL_AFTER_MS,
    OBSERVATION_MS,
    ODD_LENGTH,
    PACKET_LENGTH,
    PEER_TIMEOUT_MS,
    SCENARIO,
    SCENARIO_NAME,
    SESSION_GAP_MS,
    Scenario,
    first_mismatch,
    odd_byte,
    request_byte,
    response_byte,
};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const WORDS: usize = 32;

static DESCRIPTOR: StaticCell<Descriptor> = StaticCell::new();
static BUFFER: StaticCell<[u32; WORDS]> = StaticCell::new();

type Storage = (&'static mut Descriptor, &'static mut [u32]);
type Res<T> = Result<T, &'static str>;

macro_rules! qa {
    ($($arg:tt)*) => {
        println!("QA|C6|t={}|{}", Instant::now().as_millis(), format_args!($($arg)*))
    };
}

fn err<E: core::fmt::Debug>(step: &'static str) -> impl FnOnce(E) -> &'static str {
    move |error| {
        qa!("fail|{}|{:?}", step, error);
        step
    }
}

struct Bus {
    hinf: HINF<'static>,
    slc: SLC<'static>,
    slchost: SLCHOST<'static>,
    cmd: GPIO18<'static>,
    clk: GPIO19<'static>,
    data0: GPIO20<'static>,
    data1: GPIO21<'static>,
    data2: GPIO22<'static>,
    data3: GPIO23<'static>,
}

impl Bus {
    /// A new driver instance. The first clock user resets the SDIO block.
    fn session(&mut self) -> SdioSlave<'_, Async> {
        SdioSlave::new(
            self.hinf.reborrow(),
            self.slc.reborrow(),
            self.slchost.reborrow(),
        )
        .into_async()
        .with_cmd(self.cmd.reborrow())
        .with_clk(self.clk.reborrow())
        .with_data0(self.data0.reborrow())
        .with_data1(self.data1.reborrow())
        .with_data2(self.data2.reborrow())
        .with_data3(self.data3.reborrow())
    }
}

/// Fills a whole buffer with the canary, then writes the first `len` bytes. Only call with a
/// buffer returned by the driver.
fn fill(buffer: &mut [u32], len: usize, byte: impl Fn(usize) -> u8) {
    for (w, word) in buffer.iter_mut().enumerate() {
        let mut bytes = [CANARY; 4];
        for (j, slot) in bytes.iter_mut().enumerate() {
            let i = w * 4 + j;
            if i < len {
                *slot = byte(i);
            }
        }
        *word = u32::from_le_bytes(bytes);
    }
}

// Descriptor word 0 decoding is an interpretation of TRM 34.5.5.1, printed with the raw value:
// bit 31 owner (1 = DMA), bit 30 EOF, bits 23..12 length, bits 11..0 capacity.
fn diag_rx(tag: &str, slave: &SdioSlave<'_, Async>) {
    let d = slave.receive_diagnostics();
    qa!(
        "diag|{}|slc0_tx(host->slave) dw0={:#010x} owner_dma={} eof={} len={} size={} \
         token={} int_raw={:#010x} park={}",
        tag,
        d.descriptor_dw0,
        d.descriptor_dw0 >> 31,
        (d.descriptor_dw0 >> 30) & 1,
        (d.descriptor_dw0 >> 12) & 0xfff,
        d.descriptor_dw0 & 0xfff,
        d.token,
        d.interrupt_raw,
        d.parked
    );
}

fn diag_tx(tag: &str, slave: &SdioSlave<'_, Async>) {
    let d = slave.transmit_diagnostics();
    qa!(
        "diag|{}|slc0_rx(slave->host) dw0={:#010x} owner_dma={} eof={} len={} size={} \
         published={} int_raw={:#010x} park={}",
        tag,
        d.descriptor_dw0,
        d.descriptor_dw0 >> 31,
        (d.descriptor_dw0 >> 30) & 1,
        (d.descriptor_dw0 >> 12) & 0xfff,
        d.descriptor_dw0 & 0xfff,
        d.published_length,
        d.interrupt_raw,
        d.parked
    );
}

fn snapshot(tag: &str, slave: &SdioSlave<'_, Async>) {
    let s = slave.snapshot();
    qa!(
        "snapshot|{}|state={:?} f1={:?} cfg1={:#010x} cfg7={:#010x} f0={:#010x} \
         slc0_raw={:#010x} slc1_raw={:#010x}",
        tag,
        slave.state(),
        slave.function1(),
        s.cfg_data1,
        s.cfg_data7,
        s.function0_status,
        s.slc0_interrupt_raw,
        s.slc1_interrupt_raw
    );
}

async fn wait_enabled(slave: &SdioSlave<'_, Async>) -> Res<()> {
    with_timeout(Duration::from_millis(PEER_TIMEOUT_MS), async {
        while slave.function1() == FunctionState::Disabled {
            Timer::after_millis(5).await;
        }
    })
    .await
    .map_err(err("host enable timed out"))?;
    qa!("host_enabled_function1");
    Ok(())
}

fn check_request(received: Option<&[u8]>, length: usize, k: u32) -> Res<()> {
    let expected: [u8; PACKET_LENGTH] = core::array::from_fn(|i| request_byte(SCENARIO, k, i));
    let mismatch = received.and_then(|bytes| first_mismatch(bytes, &expected));
    qa!(
        "rx_check|k={}|length={}|present={}|first_mismatch={:?}",
        k,
        length,
        received.is_some(),
        mismatch
    );
    if length != PACKET_LENGTH || received.is_none() || mismatch.is_some() {
        return Err("request payload mismatch");
    }
    Ok(())
}

/// One P4 -> C6 request and the matching C6 -> P4 response, reusing the same allocations.
/// With `cancel`, each direction first starts a real async wait and cancels it by timeout
/// before readiness is declared to the host, so the cancellation precedes the host transfer.
async fn round(
    slave: &mut SdioSlave<'_, Async>,
    storage: Storage,
    k: u32,
    cancel: bool,
) -> Res<Storage> {
    let (descriptor, buffer) = storage;
    fill(buffer, 0, |_| 0);
    slave
        .arm_receive(descriptor, buffer)
        .map_err(err("arm_receive"))?;
    diag_rx("rx_armed", slave);
    wait_enabled(slave).await?;
    if cancel {
        let before = slave.receive_diagnostics();
        let outcome = with_timeout(
            Duration::from_millis(CANCEL_AFTER_MS),
            slave.wait_received(),
        )
        .await;
        match outcome {
            Err(_) => qa!("cancel|rx|wait pending then cancelled"),
            Ok(result) => {
                qa!(
                    "fail|cancel|rx|wait completed before host transfer: {:?}",
                    result
                );
                return Err("rx wait completed before the host transfer");
            }
        }
        let after = slave.receive_diagnostics();
        diag_rx("rx_after_cancel", slave);
        let held = slave.state() == State::Function1Receiving
            && slave.received().is_none()
            && after.descriptor_dw0 >> 31 == 1
            && (after.descriptor_dw0 >> 30) & 1 == 0
            && after.token == before.token;
        qa!(
            "cancel_check|rx|dma_still_armed_and_no_extra_credit={}",
            held
        );
        if !held {
            return Err("rx cancellation changed driver or credit state");
        }
    }
    slave.set_function1_ready().map_err(err("rx ready"))?;
    let length = with_timeout(
        Duration::from_millis(PEER_TIMEOUT_MS),
        slave.wait_received(),
    )
    .await
    .map_err(err("receive timed out"))?
    .map_err(err("receive failed"))?;
    check_request(slave.received(), length, k)?;
    diag_rx("rx_done", slave);

    let (descriptor, buffer) = slave.finish_receive().map_err(err("finish_receive"))?;
    fill(buffer, PACKET_LENGTH, |i| response_byte(SCENARIO, k, i));
    slave
        .arm_transmit(descriptor, buffer, PACKET_LENGTH)
        .map_err(err("arm_transmit"))?;
    diag_tx("tx_armed", slave);
    if cancel {
        let before = slave.transmit_diagnostics();
        let outcome = with_timeout(
            Duration::from_millis(CANCEL_AFTER_MS),
            slave.wait_transmitted(),
        )
        .await;
        match outcome {
            Err(_) => qa!("cancel|tx|wait pending then cancelled"),
            Ok(result) => {
                qa!(
                    "fail|cancel|tx|wait completed before host transfer: {:?}",
                    result
                );
                return Err("tx wait completed before the host transfer");
            }
        }
        let after = slave.transmit_diagnostics();
        diag_tx("tx_after_cancel", slave);
        let held = slave.state() == State::Function1Transmitting
            && after.descriptor_dw0 >> 31 == 1
            && (after.descriptor_dw0 >> 30) & 1 == 0
            && after.published_length == before.published_length;
        qa!(
            "cancel_check|tx|dma_still_armed_and_no_extra_length={}",
            held
        );
        if !held {
            return Err("tx cancellation changed driver or length state");
        }
    }
    slave.set_function1_ready().map_err(err("tx ready"))?;
    let sent = with_timeout(
        Duration::from_millis(PEER_TIMEOUT_MS),
        slave.wait_transmitted(),
    )
    .await
    .map_err(err("transmit timed out"))?
    .map_err(err("transmit failed"))?;
    qa!("tx_done|k={}|dma_bytes={}", k, sent);
    diag_tx("tx_done", slave);
    if sent != PACKET_LENGTH {
        return Err("unexpected transmitted length");
    }
    slave.finish_transmit().map_err(err("finish_transmit"))
}

async fn idle(scenario: &str, slave: SdioSlave<'_, Async>) -> ! {
    qa!("RESULT|{}|C6_DONE|state={:?}", scenario, slave.state());
    loop {
        Timer::after_secs(1).await;
    }
}

async fn h1_h2_h4(bus: &mut Bus, storage: Storage) -> Res<()> {
    let mut slave = bus.session();
    slave.enable_bus().map_err(err("enable_bus"))?;
    snapshot("bus_enabled", &slave);
    let (rounds, cancel) = match SCENARIO {
        Scenario::H2 => (3, false),
        Scenario::H4 => (1, true),
        _ => (1, false),
    };
    let mut storage = storage;
    for k in 0..rounds {
        // H1 keeps request number 0 for the historical ramp; H2 uses 1..=3.
        let k = if SCENARIO == Scenario::H2 { k + 1 } else { k };
        qa!("round_begin|k={}", k);
        storage = round(&mut slave, storage, k, cancel).await?;
        qa!("round_end|k={}|state={:?}", k, slave.state());
    }
    idle(SCENARIO_NAME, slave).await
}

async fn h3(bus: &mut Bus, storage: Storage) -> Res<()> {
    let mut slave = bus.session();
    slave.enable_bus().map_err(err("enable_bus"))?;
    let (descriptor, buffer) = storage;
    fill(buffer, ODD_LENGTH, odd_byte);
    slave
        .arm_transmit(descriptor, buffer, ODD_LENGTH)
        .map_err(err("arm_transmit"))?;
    diag_tx("tx_armed_65", &slave);
    wait_enabled(&slave).await?;
    slave.set_function1_ready().map_err(err("tx ready"))?;
    let sent = with_timeout(
        Duration::from_millis(PEER_TIMEOUT_MS),
        slave.wait_transmitted(),
    )
    .await
    .map_err(err("transmit timed out"))?
    .map_err(err("transmit failed"))?;
    qa!("tx_done|dma_bytes={}", sent);
    diag_tx("tx_done_65", &slave);
    if sent != ODD_LENGTH {
        return Err("unexpected transmitted length");
    }
    slave.finish_transmit().map_err(err("finish_transmit"))?;
    idle(SCENARIO_NAME, slave).await
}

#[derive(Clone, Copy, PartialEq)]
enum Direction {
    /// SLC0 TX: host -> slave.
    Receive,
    /// SLC0 RX: slave -> host.
    Transmit,
}

/// Arms one transfer that the host never consumes, then stops the bus. Returns the storage only
/// when the stop is confirmed and the driver reports `SessionInvalidated`.
async fn interrupted(
    bus: &mut Bus,
    storage: Storage,
    direction: Direction,
    tag: &'static str,
) -> Res<Storage> {
    let mut slave = bus.session();
    slave.enable_bus().map_err(err("enable_bus"))?;
    let (descriptor, buffer) = storage;
    match direction {
        Direction::Receive => {
            fill(buffer, 0, |_| 0);
            slave
                .arm_receive(descriptor, buffer)
                .map_err(err("arm_receive"))?;
            diag_rx("armed", &slave);
        }
        Direction::Transmit => {
            fill(buffer, PACKET_LENGTH, |i| response_byte(SCENARIO, 90, i));
            slave
                .arm_transmit(descriptor, buffer, PACKET_LENGTH)
                .map_err(err("arm_transmit"))?;
            diag_tx("armed", &slave);
        }
    }
    wait_enabled(&slave).await?;
    slave.set_function1_ready().map_err(err("ready"))?;
    Timer::after_millis(OBSERVATION_MS).await;

    let premature = match direction {
        Direction::Receive => slave.take_receive_buffers().is_err(),
        Direction::Transmit => slave.take_transmit_buffers().is_err(),
    };
    qa!(
        "pre_stop|{}|restitution_refused_while_dma_active={}",
        tag,
        premature
    );
    match direction {
        Direction::Receive => diag_rx("before_stop", &slave),
        Direction::Transmit => diag_tx("before_stop", &slave),
    }
    snapshot("before_stop", &slave);
    if !premature {
        return Err("storage was returned before the stop");
    }

    let started = Instant::now();
    let stop = slave.disable_bus();
    qa!(
        "stop|{}|result={:?}|state={:?}|duration_us={}",
        tag,
        stop,
        slave.state(),
        started.elapsed().as_micros()
    );
    match direction {
        Direction::Receive => diag_rx("after_stop", &slave),
        Direction::Transmit => diag_tx("after_stop", &slave),
    }
    if let Err(error) = stop {
        let retained = match direction {
            Direction::Receive => slave.take_receive_buffers().is_err(),
            Direction::Transmit => slave.take_transmit_buffers().is_err(),
        };
        qa!(
            "stop_failed|{}|{:?}|allocations_retained={}",
            tag,
            error,
            retained
        );
        return Err(if retained {
            "stop not confirmed; allocations retained"
        } else {
            "stop not confirmed and allocations were returned"
        });
    }
    if slave.state() != State::SessionInvalidated {
        return Err("stop did not invalidate the session");
    }
    let restored = match direction {
        Direction::Receive => slave.take_receive_buffers(),
        Direction::Transmit => slave.take_transmit_buffers(),
    }
    .map_err(err("take buffers"))?
    .ok_or("no storage to restore")?;
    let other = match direction {
        Direction::Receive => slave.take_transmit_buffers(),
        Direction::Transmit => slave.take_receive_buffers(),
    };
    qa!(
        "restitution|{}|other_direction_empty={}",
        tag,
        matches!(other, Ok(None))
    );
    let reopen = slave.enable_bus();
    qa!("reopen|{}|result={:?}", tag, reopen);
    if reopen != Err(Error::SessionInvalidated) {
        return Err("enable_bus did not refuse the invalidated instance");
    }
    snapshot("invalidated", &slave);
    drop(slave);
    qa!("dropped|{}|pads released for {} ms", tag, SESSION_GAP_MS);
    Timer::after_millis(SESSION_GAP_MS).await;
    Ok(restored)
}

async fn h567(bus: &mut Bus, storage: Storage) -> Res<()> {
    let storage = interrupted(bus, storage, Direction::Receive, "h5").await?;
    let storage = interrupted(bus, storage, Direction::Transmit, "h6").await?;
    qa!("h7|new_session_begin");
    let mut slave = bus.session();
    slave.enable_bus().map_err(err("enable_bus"))?;
    snapshot("h7_bus_enabled", &slave);
    diag_rx("h7_fresh", &slave);
    diag_tx("h7_fresh", &slave);
    round(&mut slave, storage, 7, false).await?;
    idle(SCENARIO_NAME, slave).await
}

#[esp_hal::main]
async fn main(_spawner: Spawner) {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let timers = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timers.timer0);
    let _console = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(115_200),
    )
    .unwrap()
    .with_tx(peripherals.GPIO16);
    qa!("boot|firmware=sdio-qa-slave|scenario={}", SCENARIO_NAME);
    let mut bus = Bus {
        hinf: peripherals.HINF,
        slc: peripherals.SLC,
        slchost: peripherals.SLCHOST,
        cmd: peripherals.GPIO18,
        clk: peripherals.GPIO19,
        data0: peripherals.GPIO20,
        data1: peripherals.GPIO21,
        data2: peripherals.GPIO22,
        data3: peripherals.GPIO23,
    };
    let storage = (
        DESCRIPTOR.init(Descriptor::new()),
        BUFFER.init([0; WORDS]).as_mut_slice(),
    );
    let result = match SCENARIO {
        Scenario::H1 | Scenario::H2 | Scenario::H4 => h1_h2_h4(&mut bus, storage).await,
        Scenario::H3 => h3(&mut bus, storage).await,
        Scenario::H567 => h567(&mut bus, storage).await,
    };
    if let Err(reason) = result {
        qa!("RESULT|{}|C6_FAIL|{}", SCENARIO_NAME, reason);
    }
    // No retry: a failed scenario needs a fresh session.
    loop {
        Timer::after_secs(1).await;
    }
}
