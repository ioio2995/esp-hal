//! SDIO throughput benchmark, ESP32-C6 slave side. Companion of `sdio-bench-host` (ESP32-P4).
//!
//! The C6 firmware does not know the configuration in advance. After each enumeration the P4
//! describes every phase in a 64-byte control packet (direction, packet length, packet count,
//! phase identifier) and the C6 then serves it with the explicit driver cycle: arm, declare
//! ready, wait for the completion, validate, hand the storage back, arm again. At the end of a
//! phase it sends the P4 a 64-byte summary of what it validated. Nothing is printed inside a
//! phase. A timeout or a protocol error is reported once and ends the run: no retry; the P4
//! resets the C6 for the next configuration.
//!
//! The driver is the production one and is never modified for the benchmark.
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
    sdio_slave::{Descriptor, FunctionState, SdioSlave},
    timer::timg::TimerGroup,
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::sdio_bench::{CONTROL_LENGTH, Control, Mode, PLAN_NAME, Summary, fill_words, salt, verify};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// 4096 bytes: the largest packet of the benchmark. The driver accepts up to 4095 words.
const WORDS: usize = 1024;
/// Bound of every wait on the P4. The longest silence is the P4 reconfiguring the bus.
const PEER_TIMEOUT_S: u64 = 30;

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

/// What the C6 validated during one phase.
struct Stats {
    packets_ok: u32,
    bytes_ok: u32,
    content_errors: u32,
    first_error_byte: u32,
    first_error_seq: u32,
}

impl Stats {
    const fn new() -> Self {
        Self {
            packets_ok: 0,
            bytes_ok: 0,
            content_errors: 0,
            first_error_byte: u32::MAX,
            first_error_seq: u32::MAX,
        }
    }

    fn bad(&mut self, seq: u32, byte: u32) {
        if self.content_errors == 0 {
            self.first_error_byte = byte;
            self.first_error_seq = seq;
        }
        self.content_errors += 1;
    }
}

async fn wait_enabled(slave: &SdioSlave<'_, Async>) -> Res<()> {
    with_timeout(Duration::from_secs(PEER_TIMEOUT_S), async {
        while slave.function1() == FunctionState::Disabled {
            Timer::after_millis(2).await;
        }
    })
    .await
    .map_err(err("host enable timed out"))
}

/// Arms the receive with the whole storage, declares function 1 ready, waits for one packet and
/// returns its length. The packet is left in the driver for `slave.received()`.
async fn receive(slave: &mut SdioSlave<'_, Async>, storage: Storage) -> Res<usize> {
    let (descriptor, buffer) = storage;
    slave.arm_receive(descriptor, buffer).map_err(err("arm_receive"))?;
    slave.set_function1_ready().map_err(err("rx ready"))?;
    with_timeout(Duration::from_secs(PEER_TIMEOUT_S), slave.wait_received())
        .await
        .map_err(err("receive timed out"))?
        .map_err(err("receive failed"))
}

/// Sends the first `length` bytes of the storage, already filled, and gives it back.
async fn transmit(slave: &mut SdioSlave<'_, Async>, storage: Storage, length: usize) -> Res<Storage> {
    let (descriptor, buffer) = storage;
    slave.arm_transmit(descriptor, buffer, length).map_err(err("arm_transmit"))?;
    slave.set_function1_ready().map_err(err("tx ready"))?;
    let sent = with_timeout(Duration::from_secs(PEER_TIMEOUT_S), slave.wait_transmitted())
        .await
        .map_err(err("transmit timed out"))?
        .map_err(err("transmit failed"))?;
    if sent != length {
        return Err("transmitted length differs");
    }
    slave.finish_transmit().map_err(err("finish_transmit"))
}

/// One packet from the P4: validated byte for byte, then the storage is handed back.
async fn receive_packet(
    slave: &mut SdioSlave<'_, Async>,
    storage: Storage,
    control: &Control,
    seq: u32,
    stats: &mut Stats,
) -> Res<Storage> {
    let length = receive(slave, storage).await?;
    match slave.received() {
        Some(bytes) if length == control.length as usize => {
            match verify(bytes, seq, salt(false, control.phase)) {
                Ok(()) => {
                    stats.packets_ok += 1;
                    stats.bytes_ok = stats.bytes_ok.wrapping_add(length as u32);
                }
                Err(byte) => stats.bad(seq, byte as u32),
            }
        }
        _ => stats.bad(seq, length as u32),
    }
    slave.finish_receive().map_err(err("finish_receive"))
}

/// One packet to the P4, filled with the sequenced pattern of the C6.
async fn send_packet(
    slave: &mut SdioSlave<'_, Async>,
    storage: Storage,
    control: &Control,
    seq: u32,
) -> Res<Storage> {
    let (descriptor, buffer) = storage;
    fill_words(buffer, control.length, seq, salt(true, control.phase));
    transmit(slave, (descriptor, buffer), control.length as usize).await
}

async fn send_summary(
    slave: &mut SdioSlave<'_, Async>,
    storage: Storage,
    phase: u32,
    stats: &Stats,
) -> Res<Storage> {
    let summary = Summary {
        phase,
        packets_ok: stats.packets_ok,
        bytes_ok: stats.bytes_ok,
        content_errors: stats.content_errors,
        first_error_byte: stats.first_error_byte,
        first_error_seq: stats.first_error_seq,
    };
    let (descriptor, buffer) = storage;
    let encoded = summary.encode();
    for (word, chunk) in buffer.iter_mut().zip(encoded.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    transmit(slave, (descriptor, buffer), CONTROL_LENGTH).await
}

/// The raw registers that describe the high speed configuration: `HINF_CFG_DATA1` (bit 2 SHS
/// advertised, bit 3 EHS selected by the host, and the sampling configuration) and `SLCHOST_CONF`
/// (bit 27 `HSPEED_CON_EN` and the edge force bits). Read only, outside every timed phase.
fn registers(slave: &SdioSlave<'_, Async>) -> (u32, u32, bool, bool) {
    let snapshot = slave.snapshot();
    let slchost_conf = esp_hal::peripherals::SLCHOST::regs().conf().read().bits();
    (snapshot.cfg_data1, slchost_conf, snapshot.highspeed_advertised(), snapshot.highspeed_selected())
}

async fn run_phase(
    slave: &mut SdioSlave<'_, Async>,
    mut storage: Storage,
    control: Control,
) -> Res<Storage> {
    let mut stats = Stats::new();
    for seq in 0..control.count {
        match control.mode {
            Mode::HostToSlave => {
                storage = receive_packet(slave, storage, &control, seq, &mut stats).await?;
            }
            Mode::SlaveToHost => {
                storage = send_packet(slave, storage, &control, seq).await?;
            }
            Mode::RoundTrip => {
                storage = receive_packet(slave, storage, &control, seq, &mut stats).await?;
                storage = send_packet(slave, storage, &control, seq).await?;
            }
        }
    }
    let storage = send_summary(slave, storage, control.phase, &stats).await?;
    // Printed after the phase: nothing is printed while packets flow.
    let (cfg_data1, slchost_conf, shs, ehs) = registers(slave);
    qa!(
        "bench_phase|id={}|mode={}|length={}|count={}|packets_ok={}|bytes_ok={}|content_errors={}|first_error_byte={}|first_error_seq={}|shs={}|ehs={}|hinf_cfg_data1={:#010x}|slchost_conf={:#010x}",
        control.phase,
        control.mode.name(),
        control.length,
        control.count,
        stats.packets_ok,
        stats.bytes_ok,
        stats.content_errors,
        stats.first_error_byte as i32,
        stats.first_error_seq as i32,
        shs as u8,
        ehs as u8,
        cfg_data1,
        slchost_conf
    );
    Ok(storage)
}

async fn serve(bus: &mut Bus, storage: Storage) -> Res<()> {
    let mut slave = bus.session();
    slave.enable_bus().map_err(err("enable_bus"))?;
    // Before the host has read anything: what the driver configured when it exposed the bus.
    let (cfg_data1, slchost_conf, shs, ehs) = registers(&slave);
    qa!(
        "bench_regs|when=exposed|shs={}|ehs={}|hinf_cfg_data1={:#010x}|slchost_conf={:#010x}",
        shs as u8,
        ehs as u8,
        cfg_data1,
        slchost_conf
    );
    let mut storage = storage;
    let mut first = true;
    loop {
        // The next phase description. Between two phases the C6 is idle and ready.
        if first {
            let (descriptor, buffer) = storage;
            slave.arm_receive(descriptor, buffer).map_err(err("arm_receive"))?;
            wait_enabled(&slave).await?;
            slave.set_function1_ready().map_err(err("rx ready"))?;
            first = false;
            let length = with_timeout(Duration::from_secs(PEER_TIMEOUT_S), slave.wait_received())
                .await
                .map_err(err("control timed out"))?
                .map_err(err("control failed"))?;
            let control = slave.received().and_then(Control::decode);
            storage = slave.finish_receive().map_err(err("finish_receive"))?;
            let Some(control) = control else {
                qa!("fail|bad control packet|length={}", length);
                return Err("bad control packet");
            };
            storage = run_phase(&mut slave, storage, control).await?;
            continue;
        }
        let length = receive(&mut slave, storage).await?;
        let control = slave.received().and_then(Control::decode);
        storage = slave.finish_receive().map_err(err("finish_receive"))?;
        let Some(control) = control else {
            qa!("fail|bad control packet|length={}", length);
            return Err("bad control packet");
        };
        storage = run_phase(&mut slave, storage, control).await?;
    }
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
    qa!("boot|firmware=sdio-bench-slave|plan={}", PLAN_NAME);
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
    if let Err(reason) = serve(&mut bus, storage).await {
        qa!("RESULT|bench|C6_FAIL|{}", reason);
    }
    // No retry: the P4 resets the C6 for the next configuration.
    loop {
        Timer::after_secs(1).await;
    }
}
