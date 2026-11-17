//! SDIO host QA firmware for the ESP32-P4, companion of `sdio-qa-slave` (ESP32-C6).
//!
//! The scenario is selected at build time with `SDIO_QA_SCENARIO` (`h1`, `h2`, `h3`, `h4`,
//! `h567`). See `examples/qa/SDIO-QA.md` for the contracts and the log format.
//!
//! The host drives the C6 enable line, enumerates, and follows the slave only through bus-visible
//! state: the cumulative receive credit (function 1, 0x44), the cumulative published length
//! (function 1, 0x60) and the CCCR `IOR1` bit. It never retries a transfer.
//!
//! Pins: slot 1 routed through the GPIO matrix, CLK GPIO18, CMD GPIO19, DATA0..3 GPIO14..17,
//! C6 EN on GPIO54, console UART0 TX on GPIO37 and RX on GPIO38.
//!
//! The C6 EN pin is wired to GPIO54: this firmware can reset the C6 by itself, and a reset of the
//! P4 reboots the C6. After a scenario (and from boot in the `c6ctl` scenario) it listens on the
//! console for line commands `C6 HOLD`, `C6 RELEASE`, `C6 RESET` and `C6 PING`, so that the C6
//! can be reset by software while its GPIO9 is grounded by hand to enter ROM download.

//% CHIP_FILTER: esp32p4

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, Ordering};

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
use qa_test::sdio_qa::{
    C6_RESET_MS,
    C6Command,
    CREDIT_MASK,
    LENGTH_MASK,
    ODD_LENGTH,
    PACKET_LENGTH,
    PEER_TIMEOUT_MS,
    SCENARIO,
    SCENARIO_NAME,
    SESSION_GAP_MS,
    Scenario,
    WINDOW_END,
    credit_add,
    first_mismatch,
    length_add,
    odd_byte,
    request_byte,
    response_byte,
};
use sdio::{Command, Response, ResponseLen};

mod voltage {
    //! Supply window passed to the SDIO crate's CMD5 constructor.

    /// The constructor shifts its 9-bit voltage argument into OCR bits 15..23.
    pub(super) const fn voltage_window(card_ocr: u32) -> u16 {
        const SUPPORTED: u32 = (1 << 20) | (1 << 21);
        ((card_ocr & SUPPORTED) >> 15) as u16
    }
}

// The H3 diagnostic relies on an explicit cache write-back before the DMA. Refuse to build if the
// chip does not report cached internal memory, instead of silently omitting it.
#[cfg(not(soc_internal_memory_cached))]
compile_error!("the QA host needs soc_internal_memory_cached for its cache write-back");

esp_bootloader_esp_idf::esp_app_desc!();

/// Error text of a command that got no response during a deliberate probe.
const NO_RESPONSE: &str = "no response from the card";

/// Set only while the host probes a bus that the slave is expected to have released.
static EXPECT_NO_RESPONSE: AtomicBool = AtomicBool::new(false);

type Host<'d> = Slot<'d, 1, Blocking>;
type Result<T> = core::result::Result<T, &'static str>;

fn now_ms() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

macro_rules! qa {
    ($($arg:tt)*) => {
        println!("QA|P4|t={}|{}", now_ms(), format_args!($($arg)*))
    };
}

fn fail<T>(step: &'static str, detail: impl core::fmt::Debug) -> Result<T> {
    qa!("fail|{}|{:?}", step, detail);
    Err(step)
}

/// Both cache lines of a transfer buffer belong to this allocation.
#[repr(C, align(64))]
struct Packet([u8; 128]);

/// A read buffer surrounded by guard lines. Every region is a whole number of cache lines and the
/// whole structure belongs to one transfer, so a write that reaches a guard came from outside the
/// 128-byte data area.
#[repr(C, align(64))]
struct Guarded {
    head: [u8; 64],
    data: [u8; 128],
    tail: [u8; 64],
}

const GUARD: u8 = 0xa5;

impl Guarded {
    fn new() -> Self {
        Self {
            head: [GUARD; 64],
            data: [0xff; 128],
            tail: [GUARD; 64],
        }
    }
}

/// Reports what the transfer left around the useful bytes. `nonff_in_rounding` counts bytes
/// between the requested length and the word-rounded descriptor size, `nonff_beyond` those past
/// that size. The data area was written back to memory before the DMA, so any such byte was
/// written by the transfer, not brought back by a stale cache line. Returns whether the area
/// outside the descriptor size and both guards are untouched.
fn tail_report(label: &str, buffer: &Guarded, length: usize, invalidated: usize) -> bool {
    let rounded = (length + 3) & !3;
    let in_rounding = buffer.data[length..rounded]
        .iter()
        .filter(|byte| **byte != 0xff)
        .count();
    let beyond = &buffer.data[rounded..];
    let count = beyond.iter().filter(|byte| **byte != 0xff).count();
    let first = beyond.iter().position(|byte| *byte != 0xff).map(|i| i + rounded);
    let last = beyond.iter().rposition(|byte| *byte != 0xff).map(|i| i + rounded);
    let head_ok = buffer.head.iter().all(|byte| *byte == GUARD);
    let tail_ok = buffer.tail.iter().all(|byte| *byte == GUARD);
    qa!(
        "tail_check|{}|length={}|rounded={}|nonff_in_rounding={}|nonff_beyond={}|first_beyond={:?}|last_beyond={:?}|head_guard_ok={}|tail_guard_ok={}|writeback_before_dma={}|invalidated_after_dma={}|after_length={:02x?}",
        label,
        length,
        rounded,
        in_rounding,
        count,
        first,
        last,
        head_ok,
        tail_ok,
        cfg!(soc_internal_memory_cached),
        invalidated,
        &buffer.data[length..(length + 8).min(128)]
    );
    if let Some(first) = first {
        qa!(
            "tail_dump|{}|from={}|bytes={:02x?}",
            label,
            first,
            &buffer.data[first..(first + 16).min(128)]
        );
    }
    count == 0 && head_ok && tail_ok
}

/// Host-side accounting of what this session has consumed. Recreated for every session.
struct Accounting {
    /// Receive credits consumed, cumulative modulo 4096.
    credits: u32,
    /// Bytes consumed from the slave, cumulative modulo 2^20.
    bytes: u32,
}

fn command<'a, C: Command + 'a>(host: &mut Host<'_>, cmd: C) -> Result<C::Resp<'a>> {
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
        .map_err(|error| {
            // Only a card that does not answer at all is a legitimate result while the slave
            // is expected to have released the bus. A CRC or any other error never is.
            if EXPECT_NO_RESPONSE.load(Ordering::Relaxed)
                && matches!(error, BlockingError::Op(SdmmcError::ResponseTimeout))
            {
                qa!("probe_no_response|CMD{}|{:?}", C::INDEX, error);
                NO_RESPONSE
            } else {
                qa!("cmd_error|CMD{}|{:?}", C::INDEX, error);
                "command failed"
            }
        })?;
    Ok(<C::Resp<'a> as Response>::from_words(&words))
}

fn register(
    host: &mut Host<'_>,
    function: u8,
    address: u32,
    write: Option<u8>,
    log: bool,
) -> Result<u8> {
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
    if log {
        qa!(
            "cmd52|f={}|addr={:#x}|write={}|data_out={:#04x}|r5_data={:#04x}|r5_flags={:#04x}",
            function,
            address,
            write.is_some(),
            write.unwrap_or(0),
            response.data,
            response.flags
        );
    }
    if response.flags & sdio::SdioError::ALL != 0 {
        return fail("CMD52 R5 error", response.flags);
    }
    Ok(response.data)
}

fn register_u32(host: &mut Host<'_>, address: u32) -> Result<u32> {
    let mut bytes = [0; 4];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = register(host, 1, address + i as u32, None, false)?;
    }
    Ok(u32::from_le_bytes(bytes))
}

/// Cumulative receive credits announced by the slave (function 1, 0x44, bits 27..16).
fn credit_cum(host: &mut Host<'_>) -> Result<u32> {
    Ok((register_u32(host, 0x44)? >> 16) & CREDIT_MASK)
}

/// Cumulative length published by the slave (function 1, 0x60, 20 bits).
fn length_cum(host: &mut Host<'_>) -> Result<u32> {
    Ok(register_u32(host, 0x60)? & LENGTH_MASK)
}

/// CCCR `IOR1`: function 1 declared ready by the slave.
fn ior1(host: &mut Host<'_>) -> Result<bool> {
    Ok(register(host, 0, 3, None, false)? & 2 != 0)
}

/// One function-1 register read made while polling for a state change.
enum Probe<T> {
    Value(T),
    /// The slave refused the access with FUNCTION_NUMBER: function 1 is not ready at this instant.
    NotReady(u8),
}

/// Reads one function-1 byte for polling. Only the FUNCTION_NUMBER flag is a transient "not
/// ready" (the driver withdraws `IOR1` at each completion until the next explicit arm); every
/// other R5 error flag is blocking. It never repeats a CMD53 or a write.
fn probe_register(host: &mut Host<'_>, address: u32) -> Result<Probe<u8>> {
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
        Ok(Probe::NotReady(response.flags))
    } else {
        fail("CMD52 R5 error", response.flags)
    }
}

fn probe_u32(host: &mut Host<'_>, address: u32) -> Result<Probe<u32>> {
    let mut bytes = [0; 4];
    for (i, byte) in bytes.iter_mut().enumerate() {
        match probe_register(host, address + i as u32)? {
            Probe::Value(value) => *byte = value,
            Probe::NotReady(flags) => return Ok(Probe::NotReady(flags)),
        }
    }
    Ok(Probe::Value(u32::from_le_bytes(bytes)))
}

/// Waits for the cumulative credit to become exactly `want`. Function 1 is only read while `IOR1`
/// (function 0, always accessible) is set; a FUNCTION_NUMBER refusal while polling is logged and
/// polled again inside the single global bound. The only accepted other value is the previous
/// count; anything else is an unexpected credit.
fn wait_credit(host: &mut Host<'_>, delay: &Delay, want: u32) -> Result<()> {
    let previous = credit_add(want, CREDIT_MASK);
    let started = now_ms();
    let mut not_ready = 0u32;
    while now_ms() - started < PEER_TIMEOUT_MS {
        if !ior1(host)? {
            delay.delay_millis(10);
            continue;
        }
        match probe_u32(host, 0x44)? {
            Probe::NotReady(flags) => {
                not_ready += 1;
                if not_ready <= 3 {
                    qa!("poll_not_ready|credit|r5_flags={:#04x}|ior1=true", flags);
                }
                delay.delay_millis(5);
            }
            Probe::Value(raw) => {
                let value = (raw >> 16) & CREDIT_MASK;
                if value == want {
                    qa!(
                        "credit_observed|cum={}|ior1={}|not_ready_polls={}",
                        value,
                        ior1(host)?,
                        not_ready
                    );
                    return Ok(());
                }
                if value != previous {
                    return fail("unexpected receive credit", (value, want));
                }
                delay.delay_millis(10);
            }
        }
    }
    fail("receive credit timed out", want)
}

fn wait_length(host: &mut Host<'_>, delay: &Delay, previous: u32, length: usize) -> Result<()> {
    let want = length_add(previous, length as u32);
    let started = now_ms();
    let mut not_ready = 0u32;
    while now_ms() - started < PEER_TIMEOUT_MS {
        if !ior1(host)? {
            delay.delay_millis(10);
            continue;
        }
        match probe_u32(host, 0x60)? {
            Probe::NotReady(flags) => {
                not_ready += 1;
                if not_ready <= 3 {
                    qa!("poll_not_ready|length|r5_flags={:#04x}|ior1=true", flags);
                }
                delay.delay_millis(5);
            }
            Probe::Value(raw) => {
                let value = raw & LENGTH_MASK;
                if value == want {
                    qa!(
                        "length_observed|cum={}|ior1={}|not_ready_polls={}",
                        value,
                        ior1(host)?,
                        not_ready
                    );
                    return Ok(());
                }
                if value != previous {
                    return fail("unexpected published length", (value, want));
                }
                delay.delay_millis(10);
            }
        }
    }
    fail("published length timed out", want)
}

/// Waits for `IOR1` and returns how long it took, in milliseconds.
fn wait_ready(host: &mut Host<'_>, delay: &Delay) -> Result<u64> {
    let started = now_ms();
    for _ in 0..PEER_TIMEOUT_MS / 10 {
        if ior1(host)? {
            let waited = now_ms() - started;
            qa!("ready_observed|waited_ms={}", waited);
            return Ok(waited);
        }
        delay.delay_millis(10);
    }
    fail("function 1 readiness timed out", ())
}

fn wait_enumerated_ready(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    for _ in 0..200 {
        if register(host, 0, 3, None, false)? & 2 != 0 {
            return Ok(());
        }
        delay.delay_millis(10);
    }
    fail("function 1 enable timed out", ())
}

fn enumerate(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    // The SD physical layer needs at least 74 clocks before the first command. The slave may
    // otherwise decode the first command out of step and flag a CRC error on a later response.
    if let Err(error) = host.send_init_sequence() {
        qa!("cmd_error|init_clocks|{:?}", error);
        return Err("init clocks failed");
    }
    qa!("enum|init_clocks|clocks=80");
    command(host, sdio::common::idle())?;
    qa!("enum|CMD0|sent");
    let inquiry = command(host, sdio::sdio::io_send_op_cond(false, 0))?;
    qa!(
        "enum|CMD5|inquiry|ocr={:#010x}|functions={}|memory={}|ready={}",
        inquiry.ocr,
        (inquiry.ocr >> 28) & 7,
        (inquiry.ocr >> 27) & 1,
        inquiry.ocr >> 31
    );
    if inquiry.ocr == u32::MAX || (inquiry.ocr >> 28) & 7 == 0 {
        return fail("card does not announce an I/O function", inquiry.ocr);
    }
    let window = voltage::voltage_window(inquiry.ocr);
    if window == 0 {
        return fail("no common 3.3 V supply window", inquiry.ocr);
    }
    let mut ready = false;
    for attempt in 1..=40u32 {
        let response = command(host, sdio::sdio::io_send_op_cond(false, window))?;
        qa!(
            "enum|CMD5|try={}|window={:#x}|ocr={:#010x}|ready={}",
            attempt,
            window,
            response.ocr,
            response.ocr >> 31
        );
        if response.ocr & (1 << 31) != 0 {
            ready = true;
            break;
        }
        delay.delay_millis(50);
    }
    if !ready {
        return fail("card initialization timed out", ());
    }
    let relative = command(host, sdio::sd::Cmd3)?;
    // R6 card status: bit 15 COM_CRC_ERROR, 14 ILLEGAL_COMMAND, 13 ERROR. It describes the
    // command received before CMD3, not CMD3 itself.
    qa!(
        "enum|CMD3|rca={:#06x}|status={:#06x}|com_crc_error={}|illegal={}|error={}",
        relative.rca,
        relative.status,
        (relative.status >> 15) & 1,
        (relative.status >> 14) & 1,
        (relative.status >> 13) & 1
    );
    if relative.rca == 0 || relative.status & 0xe000 != 0 {
        return fail("invalid RCA or R6 status", relative.status);
    }
    command(host, sdio::common::select_card(relative.rca))?;
    register(host, 0, 2, Some(2), true)?;
    wait_enumerated_ready(host, delay)?;
    qa!("enum|done");
    Ok(())
}

/// Enumerates a session started by the slave after its own pause. Enumeration is not a card
/// mutation, so it may be attempted again while the slave is still recreating its driver.
fn enumerate_after_gap(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    delay.delay_millis((SESSION_GAP_MS + 1_500) as u32);
    for attempt in 1..=10 {
        // While the slave recreates its driver, no response is expected and not an error.
        EXPECT_NO_RESPONSE.store(true, Ordering::Relaxed);
        let outcome = enumerate(host, delay);
        EXPECT_NO_RESPONSE.store(false, Ordering::Relaxed);
        match outcome {
            Ok(()) => return Ok(()),
            Err(NO_RESPONSE) => {
                qa!("enum_attempt_failed|{}|{}", attempt, NO_RESPONSE);
                delay.delay_millis(300);
            }
            // Anything else is a real error, not a slave still recreating its driver.
            Err(other) => return Err(other),
        }
    }
    fail("enumeration of the new session failed", ())
}

fn check_r5(step: &'static str, response: [u32; 4]) -> Result<()> {
    qa!("{}|r5={:#010x}", step, response[0]);
    if ((response[0] >> 8) as u8) & sdio::SdioError::ALL != 0 {
        return fail("CMD53 R5 error", response[0]);
    }
    Ok(())
}

fn cmd53_argument(length: usize) -> u32 {
    let address = WINDOW_END - length as u32;
    (1 << 28) | (1 << 26) | (address << 9) | length as u32
}

fn dma(buffer: &mut [u8]) -> Result<DmaAlignedMut<'_, [u8]>> {
    if buffer.len().is_multiple_of(64) {
        DmaAlignedMut::new(buffer).map_err(|_| "unaligned buffer")
    } else {
        // SAFETY: `buffer` is a prefix of a `Packet`, whose two cache lines are owned by this
        // transfer alone. The odd length is the purpose of the H3 scenario.
        Ok(unsafe { DmaAlignedMut::new_unchecked(buffer) })
    }
}

/// Writes one request to the slave, after its credit and readiness are observed.
fn send_request(
    host: &mut Host<'_>,
    delay: &Delay,
    accounting: &mut Accounting,
    k: u32,
) -> Result<()> {
    wait_credit(host, delay, credit_add(accounting.credits, 1))?;
    wait_ready(host, delay)?;
    let published = length_cum(host)?;
    if published != accounting.bytes {
        return fail(
            "length published without a response",
            (published, accounting.bytes),
        );
    }
    let mut packet = Packet([0; 128]);
    for (i, byte) in packet.0[..PACKET_LENGTH].iter_mut().enumerate() {
        *byte = request_byte(SCENARIO, k, i);
    }
    let response = host
        .write_blocks_blocking(
            53,
            cmd53_argument(PACKET_LENGTH) | (1 << 31),
            dma(&mut packet.0[..PACKET_LENGTH])?,
            PACKET_LENGTH as u16,
            1,
        )
        .map_err(|error| {
            qa!("cmd_error|CMD53 write|{:?}", error);
            "CMD53 write failed"
        })?;
    check_r5("cmd53_write", response)?;
    accounting.credits = credit_add(accounting.credits, 1);
    qa!(
        "request_sent|k={}|bytes={}|credits_consumed={}",
        k,
        PACKET_LENGTH,
        accounting.credits
    );
    Ok(())
}

/// Reads exactly `length` bytes published by the slave and compares each of them.
fn read_response(
    host: &mut Host<'_>,
    delay: &Delay,
    accounting: &mut Accounting,
    length: usize,
    expected: impl Fn(usize) -> u8,
    label: &'static str,
) -> Result<()> {
    wait_length(host, delay, accounting.bytes, length)?;
    wait_ready(host, delay)?;
    let credits = credit_cum(host)?;
    if credits != accounting.credits {
        return fail(
            "credit changed while the slave was transmitting",
            (credits, accounting.credits),
        );
    }
    let mut buffer = Guarded::new();
    // Write the guards and the 0xff fill back to memory before the DMA. The read path invalidates
    // the cache lines of the transfer afterwards; without this a stale line could bring old RAM
    // content back into the untouched tail.
    #[cfg(soc_internal_memory_cached)]
    {
        let mut whole = DmaAlignedMut::new(&mut buffer).map_err(|_| "unaligned guarded buffer")?;
        whole.writeback();
    }
    let response = host
        .read_blocks_blocking(
            53,
            cmd53_argument(length),
            dma(&mut buffer.data[..length])?,
            length as u16,
            1,
        )
        .map_err(|error| {
            qa!("cmd_error|CMD53 read|{:?}", error);
            "CMD53 read failed"
        })?;
    // The driver invalidates only the cache lines of the 65-byte view. Invalidate the whole
    // structure (data area and both guards) before any inspection, and never write back after the
    // DMA: that would overwrite what the transfer wrote with an older cached copy.
    #[cfg(soc_internal_memory_cached)]
    let invalidated = {
        let whole = DmaAlignedMut::new(&mut buffer).map_err(|_| "unaligned guarded buffer")?;
        whole.invalidate();
        core::mem::size_of::<Guarded>()
    };
    #[cfg(not(soc_internal_memory_cached))]
    let invalidated = 0usize;
    check_r5("cmd53_read", response)?;
    let expected_bytes: [u8; 65] = core::array::from_fn(expected);
    let mismatch = first_mismatch(&buffer.data[..length], &expected_bytes[..length]);
    qa!(
        "response_check|{}|length={}|first_mismatch={:?}|tail={:02x?}",
        label,
        length,
        mismatch,
        &buffer.data[length..(length + 4).min(128)]
    );
    if mismatch.is_some() {
        return fail("response payload mismatch", mismatch);
    }
    if !tail_report(label, &buffer, length, invalidated) {
        return fail("the transfer altered bytes outside its descriptor size or a guard", ());
    }
    accounting.bytes = length_add(accounting.bytes, length as u32);
    qa!(
        "response_consumed|{}|bytes={}|bytes_consumed_cum={}|length_cum={:?}",
        label,
        length,
        accounting.bytes,
        // The slave withdraws IOR1 when it completes the transfer: the read may be refused.
        match probe_u32(host, 0x60)? {
            Probe::Value(raw) => Some(raw & LENGTH_MASK),
            Probe::NotReady(_) => None,
        }
    );
    Ok(())
}

fn round(host: &mut Host<'_>, delay: &Delay, accounting: &mut Accounting, k: u32) -> Result<()> {
    qa!("round_begin|k={}", k);
    send_request(host, delay, accounting, k)?;
    read_response(
        host,
        delay,
        accounting,
        PACKET_LENGTH,
        |i| response_byte(SCENARIO, k, i),
        "response",
    )?;
    qa!(
        "round_end|k={}|credits_consumed={}|bytes_consumed={}",
        k,
        accounting.credits,
        accounting.bytes
    );
    Ok(())
}

fn fresh() -> Accounting {
    Accounting {
        credits: 0,
        bytes: 0,
    }
}

/// Observes the slave stopping the bus: the pads are released, so CMD52 stops answering or
/// `IOR1` clears.
fn watch_stop(host: &mut Host<'_>, delay: &Delay, tag: &'static str) -> Result<()> {
    EXPECT_NO_RESPONSE.store(true, Ordering::Relaxed);
    let outcome = watch_stop_inner(host, delay, tag);
    EXPECT_NO_RESPONSE.store(false, Ordering::Relaxed);
    outcome
}

fn watch_stop_inner(host: &mut Host<'_>, delay: &Delay, tag: &'static str) -> Result<()> {
    for _ in 0..PEER_TIMEOUT_MS / 20 {
        match ior1(host) {
            Ok(true) => delay.delay_millis(20),
            Ok(false) => {
                qa!("stop_observed|{}|via=ior1_clear", tag);
                return Ok(());
            }
            Err(NO_RESPONSE) => {
                qa!("stop_observed|{}|via=no_bus_response", tag);
                return Ok(());
            }
            // A CRC or any other error is not proof that the bus was released.
            Err(other) => return fail("unexpected error while observing the stop", other),
        }
    }
    fail("stop of the slave bus was not observed", tag)
}

fn run_h1(host: &mut Host<'_>, delay: &Delay, en: &mut Output<'_>) -> Result<()> {
    for session in 1..=3u32 {
        if session > 1 {
            reset_c6(en, delay);
        }
        qa!("session_begin|h1|{}", session);
        enumerate(host, delay)?;
        round(host, delay, &mut fresh(), 0)?;
        qa!("SESSION|h1|{}|PASS", session);
    }
    Ok(())
}

fn run_h2(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    enumerate(host, delay)?;
    let mut accounting = fresh();
    for k in 1..=3u32 {
        round(host, delay, &mut accounting, k)?;
        qa!(
            "progress|k={}|credits_consumed={}|bytes_consumed={}",
            k,
            accounting.credits,
            accounting.bytes
        );
    }
    if accounting.credits != 3 || accounting.bytes != 3 * PACKET_LENGTH as u32 {
        return fail(
            "unexpected consumed totals",
            (accounting.credits, accounting.bytes),
        );
    }
    Ok(())
}

fn run_h3(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    enumerate(host, delay)?;
    let mut accounting = fresh();
    read_response(
        host,
        delay,
        &mut accounting,
        ODD_LENGTH,
        odd_byte,
        "odd_response",
    )?;
    if accounting.bytes != ODD_LENGTH as u32 {
        return fail("unexpected consumed total", accounting.bytes);
    }
    Ok(())
}

fn run_h4(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    enumerate(host, delay)?;
    round(host, delay, &mut fresh(), 0)
}

fn run_h567(host: &mut Host<'_>, delay: &Delay) -> Result<()> {
    // H5: the slave arms a receive that this host never consumes, then stops the bus.
    enumerate(host, delay)?;
    wait_credit(host, delay, 1)?;
    wait_ready(host, delay)?;
    qa!(
        "h5|armed|credit_cum={}|length_cum={}",
        credit_cum(host)?,
        length_cum(host)?
    );
    watch_stop(host, delay, "h5")?;
    qa!("h5|host_did_not_consume_credit|credits_consumed=0");

    // H6: the slave publishes a packet that this host never reads, then stops the bus.
    enumerate_after_gap(host, delay)?;
    let credits = credit_cum(host)?;
    if credits > 1 {
        return fail("receive credit survived the new session", credits);
    }
    wait_length(host, delay, 0, PACKET_LENGTH)?;
    wait_ready(host, delay)?;
    qa!(
        "h6|published|credit_cum={}|length_cum={}",
        credit_cum(host)?,
        length_cum(host)?
    );
    watch_stop(host, delay, "h6")?;
    qa!("h6|host_did_not_read|bytes_consumed=0");

    // H7: a new session with fresh host accounting. No old credit or length may reappear.
    enumerate_after_gap(host, delay)?;
    qa!(
        "h7|fresh_session|credit_cum={}|length_cum={}",
        credit_cum(host)?,
        length_cum(host)?
    );
    round(host, delay, &mut fresh(), 7)
}

/// Holds the C6 in reset, then releases it and waits for its firmware to boot.
fn reset_c6(en: &mut Output<'_>, delay: &Delay) {
    en.set_low();
    qa!("c6_en|low");
    delay.delay_millis(500);
    en.set_high();
    qa!("c6_en|high");
    delay.delay_millis(2000);
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

/// Never returns: reads console lines and drives the C6 EN line. The only state it changes is
/// GPIO54; every command is acknowledged on the console, an unknown line is reported.
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
    // The reference board routes this pin to C6 EN. A scenario keeps the C6 in reset during
    // setup; the control firmware leaves it running so that it does not disturb a C6 held in ROM
    // download by hand.
    let initial = if SCENARIO == Scenario::C6Ctl { Level::High } else { Level::Low };
    let mut c6_enable = Output::new(peripherals.GPIO54, initial, OutputConfig::default());
    let mut console = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(115_200),
    )
    .unwrap()
    .with_tx(peripherals.GPIO37)
    .with_rx(peripherals.GPIO38);
    qa!("boot|firmware=sdio-qa-host|scenario={}", SCENARIO_NAME);
    if SCENARIO == Scenario::C6Ctl {
        c6ctl_loop(&mut console, &mut c6_enable, &delay);
    }
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
    host.set_bus_low_level(BusWidth::Bit1, 400_000).unwrap();
    delay.delay_millis(100);
    c6_enable.set_high();
    qa!("c6_en|high");
    delay.delay_millis(2000);
    let result = match SCENARIO {
        Scenario::H1 => run_h1(&mut host, &delay, &mut c6_enable),
        Scenario::H2 => run_h2(&mut host, &delay),
        Scenario::H3 => run_h3(&mut host, &delay),
        Scenario::H4 => run_h4(&mut host, &delay),
        Scenario::H567 => run_h567(&mut host, &delay),
        Scenario::C6Ctl => Ok(()),
    };
    match result {
        Ok(()) => qa!("RESULT|{}|P4_PASS", SCENARIO_NAME),
        Err(reason) => qa!("RESULT|{}|P4_FAIL|{}", SCENARIO_NAME, reason),
    }
    // No retry or further reset: a failed scenario requires a fresh session. The C6 can still be
    // controlled from the console.
    c6ctl_loop(&mut console, &mut c6_enable, &delay)
}
