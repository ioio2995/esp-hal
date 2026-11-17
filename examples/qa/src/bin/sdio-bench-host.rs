//! SDIO throughput benchmark, ESP32-P4 host side. Companion of `sdio-bench-slave` (ESP32-C6).
//!
//! For every configuration of the compiled plan (`SDIO_BENCH_PLAN`) the host resets the C6,
//! enumerates it at the qualified 400 kHz / 1-bit setting, reads the card capabilities, then
//! switches the bus width and the clock and measures three directions: P4 to C6, C6 to P4 and
//! round trip. Each direction gets a warm-up, which sizes the measurements, then short windows of
//! about five seconds, and the best error-free configuration is confirmed for thirty seconds.
//!
//! Every packet carries a sequence number and a pattern that depends on the phase and the
//! direction, and is validated byte for byte by its receiver. The timed loops print nothing.
//! Time covers the whole explicit cycle: pattern generation, waits for the credit or the
//! published length, the CMD53, the validation and the C6 re-arming. No error is retried; a
//! configuration with an error is never kept as a reliable maximum, and the ladder rule skips
//! the higher frequencies of the same width and size.
//!
//! The host driver and the slave driver are the production ones and are not modified.
//!
//! Pins: slot 1 through the GPIO matrix, CLK GPIO18, CMD GPIO19, DATA0..3 GPIO14..17, C6 EN on
//! GPIO54, console UART0 TX GPIO37 and RX GPIO38. After the run the firmware listens for the
//! `C6 HOLD` / `C6 RELEASE` / `C6 RESET` / `C6 PING` console commands like the QA firmware.

//% CHIP_FILTER: esp32p4

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    Blocking,
    delay::Delay,
    dma::aligned::DmaAlignedMut,
    gpio::{Level, Output, OutputConfig},
    main,
    sdmmc::{
        BlockingError,
        BusWidth,
        CommandFlags,
        Config,
        Error as SdmmcError,
        ResponseLen as HalResponseLen,
        SdHostController,
        Slot,
        SlotConfig,
    },
    time::Instant,
    uart::{Config as UartConfig, Uart},
};
use esp_println::println;
use qa_test::{
    sdio_bench::{
        BLOCK_SIZE,
        BYTE_MODE_MAX,
        CONTROL_LENGTH,
        Config as BenchConfig,
        Control,
        Errors,
        Ladder,
        MODES,
        Mode,
        PLAN,
        PLAN_NAME,
        Summary,
        WARMUP_PACKETS,
        Width,
        bytes_per_second,
        card_clock_hz,
        config,
        config_count,
        fill,
        packets_for,
        salt,
        verify,
    },
    sdio_qa::{
        C6_RESET_MS,
        C6Command,
        CREDIT_MASK,
        LENGTH_MASK,
        WINDOW_END,
        credit_add,
        length_add,
    },
};
use sdio::{Command, Response, ResponseLen};

esp_bootloader_esp_idf::esp_app_desc!();

mod voltage {
    //! Supply window passed to the SDIO crate's CMD5 constructor.

    /// The constructor shifts its 9-bit voltage argument into OCR bits 15..23.
    pub(super) const fn voltage_window(card_ocr: u32) -> u16 {
        const SUPPORTED: u32 = (1 << 20) | (1 << 21);
        ((card_ocr & SUPPORTED) >> 15) as u16
    }
}

/// Bound, in microseconds, of every wait on the C6.
const WAIT_US: u64 = 10_000_000;

fn now_us() -> u64 {
    Instant::now().duration_since_epoch().as_micros()
}

macro_rules! qa {
    ($($arg:tt)*) => {
        println!("QA|P4|t={}|{}", now_us() / 1000, format_args!($($arg)*))
    };
}

/// Why a step failed, by the class the report counts.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// A packet did not carry the expected content, or the C6 reported one that did not.
    Content,
    /// A response or data CRC error of the host controller.
    Crc,
    /// A non-zero R5 error flag other than a transient FUNCTION_NUMBER while polling.
    R5(u8),
    /// No response or a timeout, of the controller or of a wait on the C6.
    Timeout,
    /// Anything else, with a static reason.
    Other(&'static str),
}

type Res<T> = Result<T, Failure>;

impl Failure {
    fn count(self, errors: &mut Errors) {
        match self {
            Failure::Content => errors.content += 1,
            Failure::Crc => errors.crc += 1,
            Failure::R5(_) => errors.r5 += 1,
            Failure::Timeout => errors.timeout += 1,
            Failure::Other(_) => errors.other += 1,
        }
    }
}

fn classify(error: BlockingError) -> Failure {
    match error {
        BlockingError::Op(SdmmcError::ResponseCrc | SdmmcError::DataCrc) => Failure::Crc,
        BlockingError::Op(
            SdmmcError::ResponseTimeout | SdmmcError::DataTimeout | SdmmcError::Timeout,
        ) => Failure::Timeout,
        BlockingError::Op(_) => Failure::Other("controller error"),
        BlockingError::Busy => Failure::Other("controller busy"),
        _ => Failure::Other("unknown controller error"),
    }
}

type Host<'d> = Slot<'d, 1, Blocking>;

/// A transfer buffer. Its size and alignment are whole cache lines, as the DMA view requires.
#[repr(C, align(64))]
struct Buf([u8; 4096]);

fn command<'a, C: Command + 'a>(host: &mut Host<'_>, cmd: C) -> Res<C::Resp<'a>> {
    let response_len = match <C::Resp<'a> as Response>::LEN {
        ResponseLen::Zero => HalResponseLen::None,
        ResponseLen::R48 => HalResponseLen::Short,
        ResponseLen::R136 => HalResponseLen::Long,
    };
    let flags = CommandFlags {
        wait_complete: true,
        stop_abort: false,
        busy: <C::Resp<'a> as Response>::BUSY,
    };
    let words = host
        .command_blocking(
            C::INDEX,
            cmd.arg(),
            response_len,
            <C::Resp<'a> as Response>::CRC,
            flags,
        )
        .map_err(classify)?;
    Ok(<C::Resp<'a> as Response>::from_words(&words))
}

/// One CMD52 read or write. Any R5 error flag is a failure.
fn register(host: &mut Host<'_>, function: u8, address: u32, write: Option<u8>) -> Res<u8> {
    let response = command(
        host,
        sdio::sdio::Cmd52 {
            write: write.is_some(),
            function,
            raw: false,
            addr: address,
            data: write.unwrap_or(0),
        },
    )?;
    let errors = response.flags & sdio::SdioError::ALL;
    if errors != 0 {
        return Err(Failure::R5(response.flags));
    }
    Ok(response.data)
}

/// Outcome of one function-1 poll: a value, or the transient refusal of a function that is not
/// ready at this instant (counted and reported, never silently ignored).
enum Probe<T> {
    Value(T),
    NotReady,
}

fn probe_byte(host: &mut Host<'_>, address: u32) -> Res<Probe<u8>> {
    let response = command(
        host,
        sdio::sdio::Cmd52 {
            write: false,
            function: 1,
            raw: false,
            addr: address,
            data: 0,
        },
    )?;
    let errors = response.flags & sdio::SdioError::ALL;
    if errors == 0 {
        Ok(Probe::Value(response.data))
    } else if errors == sdio::SdioError::FLAG_FUNCTION_NUMBER {
        Ok(Probe::NotReady)
    } else {
        Err(Failure::R5(response.flags))
    }
}

/// Everything a measurement needs.
struct Ctx<'a, 'd> {
    host: &'a mut Host<'d>,
    /// Receive credits consumed in this session, modulo 4096.
    credits: u32,
    /// Bytes consumed from the C6 in this session, modulo 2^20.
    bytes: u32,
    /// Transient "function 1 not ready" refusals seen while polling, in this measurement.
    not_ready: u32,
    /// Polls whose multi-byte counter was read across an update (a torn value), in this
    /// measurement. They are polled again; only the same unexpected value twice in a row fails.
    unstable: u32,
    tx: &'a mut Buf,
    rx: &'a mut Buf,
}

impl Ctx<'_, '_> {
    fn ior1(&mut self) -> Res<bool> {
        Ok(register(self.host, 0, 3, None)? & 2 != 0)
    }

    /// Cumulative credit: function 1 bytes 0x46 and 0x47 (bits 27..16 of register 0x44).
    fn poll_credit(&mut self) -> Res<Option<u32>> {
        let Probe::Value(low) = probe_byte(self.host, 0x46)? else {
            self.not_ready += 1;
            return Ok(None);
        };
        let Probe::Value(high) = probe_byte(self.host, 0x47)? else {
            self.not_ready += 1;
            return Ok(None);
        };
        Ok(Some((((high as u32) << 8) | low as u32) & CREDIT_MASK))
    }

    /// Cumulative published length: function 1 bytes 0x60..0x62 (20 bits).
    fn poll_length(&mut self) -> Res<Option<u32>> {
        let mut value = 0u32;
        for (i, address) in [0x60u32, 0x61, 0x62].into_iter().enumerate() {
            let Probe::Value(byte) = probe_byte(self.host, address)? else {
                self.not_ready += 1;
                return Ok(None);
            };
            value |= (byte as u32) << (8 * i);
        }
        Ok(Some(value & LENGTH_MASK))
    }

    /// Waits for the cumulative credit to reach `want`. Function 1 is read only while `IOR1` is
    /// set. The previous count is the only other accepted value.
    ///
    /// The credit is read byte by byte while the C6 may update it: a value read across an update
    /// (for example `0x0ff` becoming `0x100`) is neither the old nor the new one. A value that is
    /// not expected is therefore read again, and only the same unexpected value twice in a row is
    /// an error. This stabilizes a poll; no transfer or command that changes state is repeated.
    fn wait_credit(&mut self, want: u32) -> Res<()> {
        let previous = credit_add(want, CREDIT_MASK);
        let started = now_us();
        let mut suspect: Option<u32> = None;
        while now_us() - started < WAIT_US {
            if !self.ior1()? {
                continue;
            }
            match self.poll_credit()? {
                Some(value) if value == want => return Ok(()),
                Some(value) if value == previous => suspect = None,
                Some(value) if suspect == Some(value) => {
                    return Err(Failure::Other("unexpected receive credit"));
                }
                Some(value) => {
                    suspect = Some(value);
                    self.unstable += 1;
                }
                None => {}
            }
        }
        Err(Failure::Timeout)
    }

    /// Waits for the cumulative published length to reach `want`. The only accepted other value
    /// is "nothing new since the last packet consumed". Torn reads are handled as for the credit.
    fn wait_length(&mut self, want: u32) -> Res<()> {
        let started = now_us();
        let mut suspect: Option<u32> = None;
        while now_us() - started < WAIT_US {
            if !self.ior1()? {
                continue;
            }
            match self.poll_length()? {
                Some(value) if value == want => return Ok(()),
                Some(value) if value == self.bytes => suspect = None,
                Some(value) if suspect == Some(value) => {
                    return Err(Failure::Other("unexpected published length"));
                }
                Some(value) => {
                    suspect = Some(value);
                    self.unstable += 1;
                }
                None => {}
            }
        }
        Err(Failure::Timeout)
    }

    /// CMD53 argument, block size and block count for a transfer of `length` bytes.
    fn cmd53(length: u32, write: bool) -> (u32, u16, u32) {
        let address = WINDOW_END - length;
        let (mode, size, blocks) = if length <= BYTE_MODE_MAX {
            (length, length as u16, 1)
        } else {
            ((1 << 27) | (length / BLOCK_SIZE), BLOCK_SIZE as u16, length / BLOCK_SIZE)
        };
        let argument = (1 << 28) | (1 << 26) | (address << 9) | mode;
        (if write { argument | (1 << 31) } else { argument }, size, blocks)
    }

    fn check_r5(response: [u32; 4]) -> Res<()> {
        if ((response[0] >> 8) as u8) & sdio::SdioError::ALL != 0 {
            return Err(Failure::R5((response[0] >> 8) as u8));
        }
        Ok(())
    }

    /// Writes the first `length` bytes of `tx` to the C6 as one packet.
    fn write_packet(&mut self, length: u32) -> Res<()> {
        let (argument, size, blocks) = Self::cmd53(length, true);
        let view = DmaAlignedMut::new(&mut self.tx.0[..length as usize])
            .map_err(|_| Failure::Other("unaligned transmit buffer"))?;
        let response = self
            .host
            .write_blocks_blocking(53, argument, view, size, blocks)
            .map_err(classify)?;
        Self::check_r5(response)?;
        self.credits = credit_add(self.credits, 1);
        Ok(())
    }

    /// Reads one packet of `length` bytes from the C6 into `rx`.
    fn read_packet(&mut self, length: u32) -> Res<()> {
        let (argument, size, blocks) = Self::cmd53(length, false);
        let view = DmaAlignedMut::new(&mut self.rx.0[..length as usize])
            .map_err(|_| Failure::Other("unaligned receive buffer"))?;
        let response = self
            .host
            .read_blocks_blocking(53, argument, view, size, blocks)
            .map_err(classify)?;
        Self::check_r5(response)?;
        self.bytes = length_add(self.bytes, length);
        Ok(())
    }

    /// Sends one packet: waits for the credit and for readiness, then writes it.
    fn send(&mut self, length: u32) -> Res<()> {
        self.wait_credit(credit_add(self.credits, 1))?;
        self.write_packet(length)
    }

    /// Receives one packet: waits for its published length, then reads it.
    fn receive(&mut self, length: u32) -> Res<()> {
        self.wait_length(length_add(self.bytes, length))?;
        self.read_packet(length)
    }

    /// Phase description for the C6, in its own 64-byte packet.
    fn send_control(&mut self, control: &Control) -> Res<()> {
        self.tx.0[..CONTROL_LENGTH].copy_from_slice(&control.encode());
        self.send(CONTROL_LENGTH as u32)
    }

    /// The C6 summary of a phase, read after the timed section.
    fn read_summary(&mut self, phase: u32) -> Res<Summary> {
        self.receive(CONTROL_LENGTH as u32)?;
        match Summary::decode(&self.rx.0[..CONTROL_LENGTH]) {
            Some(summary) if summary.phase == phase => Ok(summary),
            _ => Err(Failure::Other("bad summary packet")),
        }
    }
}

/// What one phase measured.
#[derive(Clone, Copy, Default)]
struct Measurement {
    packets: u32,
    /// Useful bytes validated by a receiver: both directions for a round trip.
    bytes: u64,
    micros: u64,
    lat_min: u64,
    lat_max: u64,
    lat_sum: u64,
    not_ready: u32,
    unstable: u32,
    c6_packets_ok: u32,
    c6_bytes_ok: u32,
}

impl Measurement {
    fn goodput(&self) -> u64 {
        bytes_per_second(self.bytes, self.micros)
    }

    fn packet_us(&self) -> u64 {
        self.micros.checked_div(self.packets as u64).unwrap_or(0)
    }
}

/// Runs one phase and returns its measurement. `done` receives the packets completed, so an
/// error report can say how far the phase got.
fn run_phase(
    ctx: &mut Ctx<'_, '_>,
    mode: Mode,
    length: u32,
    count: u32,
    phase: u32,
    done: &mut u32,
) -> Res<Measurement> {
    ctx.not_ready = 0;
    ctx.unstable = 0;
    *done = 0;
    ctx.send_control(&Control {
        phase,
        mode,
        length,
        count,
    })?;
    let mut m = Measurement {
        lat_min: u64::MAX,
        ..Measurement::default()
    };
    let started = now_us();
    for seq in 0..count {
        let begin = now_us();
        if matches!(mode, Mode::HostToSlave | Mode::RoundTrip) {
            fill(&mut ctx.tx.0[..length as usize], seq, salt(false, phase));
            ctx.send(length)?;
        }
        if matches!(mode, Mode::SlaveToHost | Mode::RoundTrip) {
            ctx.receive(length)?;
            if verify(&ctx.rx.0[..length as usize], seq, salt(true, phase)).is_err() {
                return Err(Failure::Content);
            }
        }
        let lap = now_us() - begin;
        m.lat_min = m.lat_min.min(lap);
        m.lat_max = m.lat_max.max(lap);
        m.lat_sum += lap;
        *done = seq + 1;
    }
    // The C6 publishes its summary once it has handled the last packet: the window ends there.
    ctx.wait_length(length_add(ctx.bytes, CONTROL_LENGTH as u32))?;
    m.micros = now_us() - started;
    let summary = ctx.read_summary(phase)?;
    m.packets = count;
    m.bytes = u64::from(count) * u64::from(length) * if mode == Mode::RoundTrip { 2 } else { 1 };
    m.not_ready = ctx.not_ready;
    m.unstable = ctx.unstable;
    m.c6_packets_ok = summary.packets_ok;
    m.c6_bytes_ok = summary.bytes_ok;
    let expects_c6_packets = matches!(mode, Mode::HostToSlave | Mode::RoundTrip);
    if summary.content_errors != 0
        || (expects_c6_packets
            && (summary.packets_ok != count || summary.bytes_ok != count.wrapping_mul(length)))
    {
        return Err(Failure::Content);
    }
    Ok(m)
}

fn enumerate(host: &mut Host<'_>, delay: &Delay) -> Res<()> {
    host.send_init_sequence().map_err(classify)?;
    command(host, sdio::common::idle())?;
    let inquiry = command(host, sdio::sdio::io_send_op_cond(false, 0))?;
    if inquiry.ocr == u32::MAX || (inquiry.ocr >> 28) & 7 == 0 {
        return Err(Failure::Other("card does not announce an I/O function"));
    }
    let window = voltage::voltage_window(inquiry.ocr);
    if window == 0 {
        return Err(Failure::Other("no common 3.3 V supply window"));
    }
    let mut ready = false;
    for _ in 0..40 {
        if command(host, sdio::sdio::io_send_op_cond(false, window))?.ocr & (1 << 31) != 0 {
            ready = true;
            break;
        }
        delay.delay_millis(50);
    }
    if !ready {
        return Err(Failure::Timeout);
    }
    let relative = command(host, sdio::sd::Cmd3)?;
    if relative.rca == 0 || relative.status & 0xe000 != 0 {
        return Err(Failure::Other("invalid RCA or R6 status"));
    }
    command(host, sdio::common::select_card(relative.rca))?;
    register(host, 0, 2, Some(2))?;
    for _ in 0..200 {
        if register(host, 0, 3, None)? & 2 != 0 {
            return Ok(());
        }
        delay.delay_millis(10);
    }
    Err(Failure::Timeout)
}

/// Why a configuration could not be measured, apart from a transfer error.
enum ConfigError {
    /// The card does not offer what the configuration needs: not an error of the transfer.
    Unsupported(&'static str),
    Failed(Failure),
}

impl From<Failure> for ConfigError {
    fn from(failure: Failure) -> Self {
        ConfigError::Failed(failure)
    }
}

/// Reads the card capabilities at the qualified setting: CCCR 0x00..0x14, function 1 basic
/// registers and block size. Read-only.
fn log_capabilities(host: &mut Host<'_>) -> Res<()> {
    let mut cccr = [0u8; 0x15];
    for (i, byte) in cccr.iter_mut().enumerate() {
        *byte = register(host, 0, i as u32, None)?;
    }
    let mut fbr = [0u8; 0x12];
    for (i, byte) in fbr.iter_mut().enumerate() {
        *byte = register(host, 0, 0x100 + i as u32, None)?;
    }
    qa!("bench_caps|cccr_00_14={:02x?}", cccr);
    qa!("bench_caps|fbr1_100_111={:02x?}", fbr);
    qa!(
        "bench_caps|sdio_revision={:#04x}|card_capability={:#04x}|bus_interface={:#04x}|bus_speed={:#04x}|fn1_block_size={}",
        cccr[0],
        cccr[8],
        cccr[7],
        cccr[0x13],
        u16::from(fbr[0x10]) | (u16::from(fbr[0x11]) << 8)
    );
    Ok(())
}

/// Switches the card and the host to the configuration, with read-back of every register.
fn configure(host: &mut Host<'_>, cfg: BenchConfig) -> Result<(), ConfigError> {
    if cfg.size > BYTE_MODE_MAX {
        register(host, 0, 0x110, Some((BLOCK_SIZE & 0xff) as u8))?;
        register(host, 0, 0x111, Some((BLOCK_SIZE >> 8) as u8))?;
        let size = u32::from(register(host, 0, 0x110, None)?)
            | (u32::from(register(host, 0, 0x111, None)?) << 8);
        if size != BLOCK_SIZE {
            return Err(ConfigError::Unsupported("function 1 block size not accepted"));
        }
    }
    let actual = card_clock_hz(cfg.requested_hz);
    if actual > 25_000_000 {
        let speed = register(host, 0, 0x13, None)?;
        if speed & 1 == 0 {
            qa!("bench_hs|requested_hz={}|size={}|cccr13_before={:#04x}|result=not_advertised", cfg.requested_hz, cfg.size, speed);
            return Err(ConfigError::Unsupported("card does not advertise high speed"));
        }
        register(host, 0, 0x13, Some(speed | 0x02))?;
        let after = register(host, 0, 0x13, None)?;
        qa!(
            "bench_hs|requested_hz={}|size={}|cccr13_before={:#04x}|cccr13_after={:#04x}|result={}",
            cfg.requested_hz,
            cfg.size,
            speed,
            after,
            if after & 0x02 == 0 { "not_accepted" } else { "accepted" }
        );
        if after & 0x02 == 0 {
            return Err(ConfigError::Unsupported("high speed not accepted"));
        }
    }
    let width = match cfg.width {
        Width::One => BusWidth::Bit1,
        Width::Four => {
            let bus = register(host, 0, 0x07, None)?;
            register(host, 0, 0x07, Some((bus & !0x03) | 0x02))?;
            if register(host, 0, 0x07, None)? & 0x03 != 0x02 {
                return Err(ConfigError::Unsupported("4-bit bus width not accepted"));
            }
            BusWidth::Bit4
        }
    };
    host.set_bus_low_level(width, cfg.requested_hz)
        .map_err(|e| Failure::Other(if matches!(e, SdmmcError::Unsupported) { "host clock unsupported" } else { "host clock" }))?;
    // A first access at the new setting: a failure here is a failure of the configuration.
    register(host, 0, 3, None)?;
    Ok(())
}

/// Holds the C6 in reset, releases it and waits for its firmware to boot.
fn reset_c6(en: &mut Output<'_>, delay: &Delay) {
    en.set_low();
    delay.delay_millis(500);
    en.set_high();
    delay.delay_millis(2_000);
}

struct Report {
    id: u32,
    cfg: BenchConfig,
}

impl Report {
    fn header(&self, cmd53_mode: &str) {
        qa!(
            "bench_config|cfg={}|plan={}|requested_hz={}|actual_hz={}|width={}|size={}|cmd53={}|block_size={}",
            self.id,
            PLAN_NAME,
            self.cfg.requested_hz,
            card_clock_hz(self.cfg.requested_hz),
            self.cfg.width.bits(),
            self.cfg.size,
            cmd53_mode,
            if self.cfg.size > BYTE_MODE_MAX { BLOCK_SIZE } else { self.cfg.size }
        );
    }

    fn result(&self, tag: &str, mode: Mode, window: u32, phase: u32, m: &Measurement) {
        let packets_per_s = bytes_per_second(u64::from(m.packets), m.micros);
        let lat_avg = m.lat_sum.checked_div(u64::from(m.packets)).unwrap_or(0);
        qa!(
            "{}|cfg={}|requested_hz={}|actual_hz={}|width={}|size={}|mode={}|window={}|phase={}|packets={}|useful_bytes={}|duration_us={}|goodput_bps={}|packets_per_s={}|lat_min_us={}|lat_avg_us={}|lat_max_us={}|err_content=0|err_crc=0|err_r5=0|err_timeout=0|err_other=0|not_ready_polls={}|unstable_polls={}|c6_packets_ok={}|c6_bytes_ok={}",
            tag,
            self.id,
            self.cfg.requested_hz,
            card_clock_hz(self.cfg.requested_hz),
            self.cfg.width.bits(),
            self.cfg.size,
            mode.name(),
            window,
            phase,
            m.packets,
            m.bytes,
            m.micros,
            m.goodput(),
            packets_per_s,
            if m.lat_min == u64::MAX { 0 } else { m.lat_min },
            lat_avg,
            m.lat_max,
            m.not_ready,
            m.unstable,
            m.c6_packets_ok,
            m.c6_bytes_ok
        );
    }

    fn error(&self, mode: Mode, window: u32, failure: Failure, done: u32) {
        let mut errors = Errors::default();
        failure.count(&mut errors);
        let detail = match failure {
            Failure::R5(flags) => flags as u32,
            _ => 0,
        };
        let reason = match failure {
            Failure::Other(text) => text,
            _ => "",
        };
        qa!(
            "bench_error|cfg={}|requested_hz={}|actual_hz={}|width={}|size={}|mode={}|window={}|packets_before_error={}|err_content={}|err_crc={}|err_r5={}|err_timeout={}|err_other={}|r5_flags={:#04x}|reason={}",
            self.id,
            self.cfg.requested_hz,
            card_clock_hz(self.cfg.requested_hz),
            self.cfg.width.bits(),
            self.cfg.size,
            mode.name(),
            window,
            done,
            errors.content,
            errors.crc,
            errors.r5,
            errors.timeout,
            errors.other,
            detail,
            reason
        );
    }
}

/// Result of measuring one configuration.
struct Outcome {
    /// Every direction measured without error.
    clean: bool,
    /// The card does not offer what the configuration needs (reported, not an error).
    unsupported: bool,
    /// Mean goodput of the round trip, bytes per second.
    round_trip_bps: u64,
    /// Mean packet time of each direction, microseconds, to size the confirmation.
    packet_us: [u64; 3],
}

/// Measures one configuration from a fresh C6 session. `confirm_ms` is `Some` for the
/// confirmation of the best configuration: one long window per direction instead of the short
/// ones.
#[allow(clippy::too_many_arguments)]
fn measure(
    host: &mut Host<'_>,
    delay: &Delay,
    en: &mut Output<'_>,
    tx: &mut Buf,
    rx: &mut Buf,
    id: u32,
    cfg: BenchConfig,
    phase: &mut u32,
    first_session: &mut bool,
    confirm_us: Option<[u64; 3]>,
) -> Outcome {
    let (windows, window_ms, confirm_ms) = PLAN.timing();
    let report = Report { id, cfg };
    let mut outcome = Outcome {
        clean: false,
        unsupported: false,
        round_trip_bps: 0,
        packet_us: [0; 3],
    };
    // Fresh session at the qualified setting.
    if host.set_bus_low_level(BusWidth::Bit1, 400_000).is_err() {
        qa!("bench_error|cfg={}|reason=qualified clock refused", id);
        return outcome;
    }
    reset_c6(en, delay);
    report.header(if cfg.size > BYTE_MODE_MAX { "block" } else { "byte" });
    if let Err(failure) = enumerate(host, delay) {
        report.error(Mode::HostToSlave, 0, failure, 0);
        return outcome;
    }
    if *first_session {
        *first_session = false;
        if let Err(failure) = log_capabilities(host) {
            report.error(Mode::HostToSlave, 0, failure, 0);
            return outcome;
        }
    }
    match configure(host, cfg) {
        Ok(()) => {}
        Err(ConfigError::Unsupported(reason)) => {
            qa!(
                "bench_unsupported|cfg={}|requested_hz={}|width={}|size={}|reason={}",
                id,
                cfg.requested_hz,
                cfg.width.bits(),
                cfg.size,
                reason
            );
            outcome.unsupported = true;
            return outcome;
        }
        Err(ConfigError::Failed(failure)) => {
            report.error(Mode::HostToSlave, 0, failure, 0);
            return outcome;
        }
    }
    let mut ctx = Ctx {
        host,
        credits: 0,
        bytes: 0,
        not_ready: 0,
        unstable: 0,
        tx,
        rx,
    };
    let mut total_rt = 0u64;
    let mut rt_windows = 0u64;
    for (index, mode) in MODES.into_iter().enumerate() {
        // Warm-up: a few packets, also the packet time that sizes the windows.
        *phase += 1;
        let mut done = 0;
        let warm = match run_phase(&mut ctx, mode, cfg.size, WARMUP_PACKETS, *phase, &mut done) {
            Ok(m) => m,
            Err(failure) => {
                report.error(mode, 0, failure, done);
                return outcome;
            }
        };
        report.result("bench_warmup", mode, 0, *phase, &warm);
        let packet_us = warm.packet_us().max(1);
        let planned = match confirm_us {
            Some(known) => known[index].max(1),
            None => packet_us,
        };
        let (count_windows, length_ms) = match confirm_us {
            Some(_) => (1, confirm_ms),
            None => (windows, window_ms),
        };
        let count = packets_for(u64::from(length_ms) * 1_000, planned);
        let mut mean_packet_us = 0u64;
        for window in 1..=count_windows {
            *phase += 1;
            let mut done = 0;
            match run_phase(&mut ctx, mode, cfg.size, count, *phase, &mut done) {
                Ok(m) => {
                    let tag = if confirm_us.is_some() { "bench_confirm" } else { "bench_result" };
                    report.result(tag, mode, window, *phase, &m);
                    mean_packet_us += m.packet_us();
                    if mode == Mode::RoundTrip {
                        total_rt += m.goodput();
                        rt_windows += 1;
                    }
                }
                Err(failure) => {
                    report.error(mode, window, failure, done);
                    return outcome;
                }
            }
        }
        outcome.packet_us[index] = mean_packet_us / u64::from(count_windows);
    }
    outcome.clean = true;
    outcome.round_trip_bps = total_rt.checked_div(rt_windows).unwrap_or(0);
    outcome
}

/// The best configuration found so far.
#[derive(Clone, Copy)]
struct Best {
    id: u32,
    cfg: BenchConfig,
    round_trip_bps: u64,
    packet_us: [u64; 3],
}

/// Level of GPIO54 as text for the acknowledgements.
fn en_text(en: &Output<'_>) -> &'static str {
    if en.is_set_high() { "high" } else { "low" }
}

fn c6ctl_apply(command: C6Command, en: &mut Output<'_>, delay: &Delay) {
    match command {
        C6Command::Hold => en.set_low(),
        C6Command::Release => en.set_high(),
        C6Command::Reset => {
            en.set_low();
            delay.delay_millis(C6_RESET_MS);
            en.set_high();
        }
        C6Command::Ping => {}
    }
    qa!("c6ctl|ack={}|en={}", command.name(), en_text(en));
}

/// Never returns: the C6 stays controllable from the console after the run.
fn c6ctl_loop(console: &mut Uart<'_, Blocking>, en: &mut Output<'_>, delay: &Delay) -> ! {
    qa!("c6ctl|ready|en={}", en_text(en));
    let mut line = [0u8; 32];
    let mut length = 0;
    loop {
        let mut chunk = [0u8; 16];
        let count = console.read_buffered(&mut chunk).unwrap_or(0);
        for &byte in &chunk[..count] {
            if byte == b'\n' || byte == b'\r' {
                if length > 0 {
                    match C6Command::parse(&line[..length]) {
                        Some(command) => c6ctl_apply(command, en, delay),
                        None => qa!("c6ctl|unknown_command|length={}", length),
                    }
                    length = 0;
                }
            } else if length < line.len() {
                line[length] = byte;
                length += 1;
            } else {
                qa!("c6ctl|line_too_long");
                length = 0;
            }
        }
        delay.delay_millis(5);
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();
    let mut c6_enable = Output::new(peripherals.GPIO54, Level::Low, OutputConfig::default());
    let mut console = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(115_200),
    )
    .unwrap()
    .with_tx(peripherals.GPIO37)
    .with_rx(peripherals.GPIO38);
    qa!("boot|firmware=sdio-bench-host|plan={}", PLAN_NAME);
    let controller = SdHostController::new(peripherals.SDHOST, Config::default()).unwrap();
    let mut host = controller
        .slot::<1>(SlotConfig::default())
        .unwrap()
        .with_clk(peripherals.GPIO18)
        .with_cmd(peripherals.GPIO19)
        .with_data0(peripherals.GPIO14)
        .with_data1(peripherals.GPIO15)
        .with_data2(peripherals.GPIO16)
        .with_data3(peripherals.GPIO17);
    qa!(
        "bench_plan|name={}|configurations={}|module_hz={}|measurements={}|window_ms={}|confirm_ms={}|warmup_packets={}",
        PLAN_NAME,
        config_count(PLAN),
        qa_test::sdio_bench::MODULE_HZ,
        PLAN.timing().0,
        PLAN.timing().1,
        PLAN.timing().2,
        WARMUP_PACKETS
    );
    let mut tx = Buf([0; 4096]);
    let mut rx = Buf([0; 4096]);
    let mut ladder = Ladder::new();
    let mut best: Option<Best> = None;
    let mut phase = 0u32;
    let mut first_session = true;
    let (mut measured, mut failed, mut skipped, mut unsupported) = (0u32, 0u32, 0u32, 0u32);
    for index in 0..config_count(PLAN) {
        let Some(cfg) = config(PLAN, index) else { break };
        let id = index as u32;
        if ladder.skips(cfg) {
            skipped += 1;
            qa!(
                "bench_skip|cfg={}|requested_hz={}|width={}|size={}|reason=lower frequency failed",
                id,
                cfg.requested_hz,
                cfg.width.bits(),
                cfg.size
            );
            continue;
        }
        let outcome = measure(
            &mut host, &delay, &mut c6_enable, &mut tx, &mut rx, id, cfg, &mut phase,
            &mut first_session, None,
        );
        if outcome.clean {
            measured += 1;
            if best.is_none_or(|b| outcome.round_trip_bps > b.round_trip_bps) {
                best = Some(Best {
                    id,
                    cfg,
                    round_trip_bps: outcome.round_trip_bps,
                    packet_us: outcome.packet_us,
                });
            }
        } else if outcome.unsupported {
            unsupported += 1;
        } else {
            failed += 1;
            ladder.record_failure(cfg);
        }
    }
    qa!(
        "bench_summary|measured_clean={}|failed={}|skipped={}|unsupported={}|best_cfg={:?}",
        measured,
        failed,
        skipped,
        unsupported,
        best.map(|b| b.id)
    );
    let mut confirmed = None;
    if let Some(best) = best
        && PLAN.timing().2 > 0
    {
        qa!(
            "bench_best|cfg={}|requested_hz={}|actual_hz={}|width={}|size={}|round_trip_goodput_bps={}",
            best.id,
            best.cfg.requested_hz,
            card_clock_hz(best.cfg.requested_hz),
            best.cfg.width.bits(),
            best.cfg.size,
            best.round_trip_bps
        );
        let again = measure(
            &mut host, &delay, &mut c6_enable, &mut tx, &mut rx, best.id, best.cfg, &mut phase,
            &mut first_session, Some(best.packet_us),
        );
        confirmed = Some(again.clean);
        qa!("bench_confirmed|cfg={}|clean={}", best.id, again.clean);
    }
    qa!(
        "RESULT|bench|P4_DONE|plan={}|measured_clean={}|failed={}|skipped={}|confirmed={:?}",
        PLAN_NAME,
        measured,
        failed,
        skipped,
        confirmed
    );
    c6ctl_loop(&mut console, &mut c6_enable, &delay)
}
