//! SDIO Slave A1 qualification firmware, ESP32-P4 side (SDMMC host).
//!
//! Holds the ESP32-C6 in reset through GPIO54 (EN), releases it, then enumerates it step by step
//! with the existing blocking SDMMC host driver on slot 1, at 400 kHz in 1-bit mode:
//!
//! | Step | Commands |
//! | ---- | -------- |
//! | H1   | CMD0, CMD5 (R4: OCR, number of functions) |
//! | H2   | CMD3 (R6: RCA) |
//! | H3   | CMD7 (select) |
//! | H4   | CMD52 reads of CCCR `0x00..=0x16`, `IOEx`, `IORx` |
//! | H5   | CMD52 reads of the FBR and bounded walks of the CIS of functions 0, 1 and 2 |
//!
//! Only CMD52 *reads* are issued. The firmware never writes a CCCR register, never changes the bus
//! width or the clock, never negotiates high speed or UHS, never enables a function and issues no
//! CMD53. It does not modify the host driver.
//!
//! The firmware reports host-side results only. It never prints a global qualification verdict:
//! that needs the C6 console log (`sdio-a1-c6-slave`) as well. A line says `PASS` only for a step
//! that was observed to succeed. A reset line says that the pin was commanded, not that the C6
//! level was measured, and the boot wait is a timer: nothing proves that the C6 application has
//! started except the C6 console and the CMD5 answer.
//!
//! Console: UART0, 115200 baud, selected with `esp-println/uart`.
//!
//! Pins (Waveshare ESP32-P4-WIFI6-POE-ETH, SDMMC slot 1, routed by the GPIO matrix):
//!
//! | Signal   | ESP32-P4 | ESP32-C6 |
//! | -------- | -------- | -------- |
//! | CLK      | GPIO18   | GPIO19   |
//! | CMD      | GPIO19   | GPIO18   |
//! | DATA0    | GPIO14   | GPIO20   |
//! | DATA1    | GPIO15   | GPIO21   |
//! | DATA2    | GPIO16   | GPIO22   |
//! | DATA3    | GPIO17   | GPIO23   |
//! | C6 reset | GPIO54   | EN       |
//!
//! Build-time parameters (environment variables, all optional, decimal):
//!
//! - `SDIO_A1_GIT_SHA`, `SDIO_A1_RUN_ID`: printed in the log lines.
//! - `SDIO_A1_RESET_HOLD_MS` (default 100): time EN is held low. Conservative: no datasheet
//!   minimum was consulted.
//! - `SDIO_A1_BOOT_WAIT_MS` (default 2000): wait after releasing EN. A timer, not proof of boot.
//! - `SDIO_A1_CMD5_TRIES` (default 10) and `SDIO_A1_CMD5_RETRY_MS` (default 200): attempts of the
//!   first CMD5.
//! - `SDIO_A1_ROUNDS` (default 1): campaigns in a row; each starts with a new reset pulse.
//! - `SDIO_A1_ROUND_GAP_MS` (default 500): pause between campaigns.
//! - `SDIO_A1_CLOCK_HZ` (default 400000): identification clock.

//% CHIP_FILTER: esp32p4
//% FEATURES: esp-println/uart

#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    Blocking,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    main,
    sdmmc::{
        BlockingError,
        BusWidth,
        CommandFlags,
        Config,
        Error,
        ResponseLen as HalResponseLen,
        SdHostController,
        Slot,
        SlotConfig,
    },
};
use esp_println::println;
use qa_test::sdio_a1::{
    CCCR_LEN,
    Cccr,
    CisEnd,
    CisLimits,
    CisReport,
    FBR_OFFSETS,
    Fbr,
    HostTally,
    Outcome,
    R4Class,
    R4Info,
    R6Info,
    fbr_base,
    parse_u32,
    walk_cis,
};
use sdio::{Command, Response, ResponseLen};

esp_bootloader_esp_idf::esp_app_desc!();

const GIT_SHA: &str = match option_env!("SDIO_A1_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};
const RUN_ID: &str = match option_env!("SDIO_A1_RUN_ID") {
    Some(id) => id,
    None => "unset",
};
const RESET_HOLD_MS: u32 = parse_u32(option_env!("SDIO_A1_RESET_HOLD_MS"), 100);
const BOOT_WAIT_MS: u32 = parse_u32(option_env!("SDIO_A1_BOOT_WAIT_MS"), 2000);
const CMD5_TRIES: u32 = parse_u32(option_env!("SDIO_A1_CMD5_TRIES"), 10);
const CMD5_RETRY_MS: u32 = parse_u32(option_env!("SDIO_A1_CMD5_RETRY_MS"), 200);
const ROUNDS: u32 = parse_u32(option_env!("SDIO_A1_ROUNDS"), 1);
const ROUND_GAP_MS: u32 = parse_u32(option_env!("SDIO_A1_ROUND_GAP_MS"), 500);
const CLOCK_HZ: u32 = parse_u32(option_env!("SDIO_A1_CLOCK_HZ"), 400_000);

/// Attempts at the second CMD5, which waits for the card ready bit.
const CMD5_READY_TRIES: u32 = 8;
/// Pause between ready polls.
const CMD5_READY_GAP_MS: u32 = 50;
/// Host voltage window offered in CMD5: 3.3 V range bits only, never 1.8 V.
const HOST_WINDOW: u16 = 0x1FF;
/// SDIO functions the TRM gives the C6 (TRM chapter 34, section 34.2).
const EXPECTED_FUNCTIONS: u8 = 2;

/// The SDMMC slot used by this firmware.
type HostSlot<'a> = Slot<'a, 1, Blocking>;

/// One log line: `SDIO-A1 <MCU> <STEP> <RESULT> <details>`, recorded in the tally.
macro_rules! a1 {
    ($tally:expr, $step:expr, $outcome:expr, $($arg:tt)*) => {{
        let outcome: Outcome = $outcome;
        $tally.record(outcome);
        println!(
            "SDIO-A1 P4 {} {} run={} {}",
            $step,
            outcome.as_str(),
            RUN_ID,
            format_args!($($arg)*)
        );
    }};
}

/// Reset line of the C6: GPIO54 drives EN.
struct C6Reset<'d> {
    en: Output<'d>,
}

impl C6Reset<'_> {
    /// Drives EN low: the C6 is held in reset.
    fn assert(&mut self) {
        self.en.set_low();
    }

    /// Drives EN high: the C6 leaves reset and boots.
    fn release(&mut self) {
        self.en.set_high();
    }
}

/// Short name of a host error, for the logs.
fn error_name(error: &BlockingError) -> &'static str {
    match error {
        BlockingError::Busy => "engine_busy",
        BlockingError::Op(Error::ResponseTimeout) => "response_timeout",
        BlockingError::Op(Error::Timeout) => "timeout",
        BlockingError::Op(Error::ResponseCrc) => "response_crc",
        BlockingError::Op(Error::ResponseError) => "response_error",
        BlockingError::Op(Error::HardwareLocked) => "hardware_locked",
        BlockingError::Op(Error::NoCard) => "no_card",
        BlockingError::Op(_) => "other",
        _ => "other",
    }
}

/// Sends a typed `sdio` command through the blocking host driver and decodes the response.
///
/// The command flags follow the ones the driver itself uses for the same commands.
fn exec<'a, C: Command + 'a>(
    slot: &mut HostSlot<'_>,
    cmd: &C,
) -> Result<C::Resp<'a>, BlockingError> {
    let len = match <C::Resp<'a> as Response>::LEN {
        ResponseLen::Zero => HalResponseLen::None,
        ResponseLen::R48 => HalResponseLen::Short,
        ResponseLen::R136 => HalResponseLen::Long,
    };
    let flags = CommandFlags {
        wait_complete: true,
        stop_abort: false,
        busy: <C::Resp<'a> as Response>::BUSY,
    };
    let words = slot.command_blocking(
        C::INDEX,
        cmd.arg(),
        len,
        <C::Resp<'a> as Response>::CRC,
        flags,
    )?;
    Ok(<C::Resp<'a> as Response>::from_words(&words))
}

/// Why a CMD52 read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadError {
    /// The host driver reported an error.
    Host(&'static str),
    /// The card answered with R5 error flags.
    R5Flags(u8),
}

/// Reads one byte of function `function` with CMD52. This never writes.
fn cmd52_read(slot: &mut HostSlot<'_>, function: u8, addr: u32) -> Result<u8, ReadError> {
    let cmd = sdio::sdio::Cmd52 {
        write: false,
        function,
        raw: false,
        addr,
        data: 0,
    };
    let r5 = exec(slot, &cmd).map_err(|e| ReadError::Host(error_name(&e)))?;
    if r5.flags & sdio::SdioError::ALL != 0 {
        return Err(ReadError::R5Flags(r5.flags));
    }
    Ok(r5.data)
}

/// Prints the reason of a failed read.
fn describe(error: ReadError) -> &'static str {
    match error {
        ReadError::Host(name) => name,
        ReadError::R5Flags(flags) => {
            if flags & sdio::SdioError::FLAG_COM_CRC_ERROR != 0 {
                "r5_com_crc_error"
            } else if flags & sdio::SdioError::FLAG_ILLEGAL_COMMAND != 0 {
                "r5_illegal_command"
            } else if flags & sdio::SdioError::FLAG_ERROR != 0 {
                "r5_error"
            } else if flags & sdio::SdioError::FLAG_FUNCTION_NUMBER != 0 {
                "r5_function_number"
            } else {
                "r5_out_of_range"
            }
        }
    }
}

/// H1: CMD0, then CMD5 until the card is ready. Returns the R4 of the last answer.
fn h1_cmd5(slot: &mut HostSlot<'_>, delay: &Delay, tally: &mut HostTally) -> Option<R4Info> {
    match exec(slot, &sdio::common::idle()) {
        Ok(_) => a1!(tally, "CMD0", Outcome::Pass, "sent; no response expected"),
        Err(e) => {
            a1!(tally, "CMD0", Outcome::Fail, "error={}", error_name(&e));
            return None;
        }
    }

    // Inquiry: CMD5 with a zero argument returns the OCR without starting the card.
    let mut inquiry: Option<R4Info> = None;
    let mut last_error = "none";
    for attempt in 1..=CMD5_TRIES {
        match exec(slot, &sdio::sdio::io_send_op_cond(false, 0)) {
            Ok(r4) => {
                let info = R4Info::decode(r4.ocr);
                println!(
                    "SDIO-A1 P4 CMD5_TRY INFO run={} attempt={}/{} raw={:#010x} class={:?}",
                    RUN_ID, attempt, CMD5_TRIES, info.raw, info.class()
                );
                if info.class() == R4Class::Plausible {
                    inquiry = Some(info);
                    break;
                }
                last_error = "invalid_response";
            }
            Err(e) => {
                last_error = match e {
                    BlockingError::Op(Error::ResponseTimeout) => "no_response",
                    other => error_name(&other),
                };
                println!(
                    "SDIO-A1 P4 CMD5_TRY INFO run={} attempt={}/{} error={}",
                    RUN_ID, attempt, CMD5_TRIES, last_error
                );
            }
        }
        delay.delay_millis(CMD5_RETRY_MS);
    }
    let Some(inquiry) = inquiry else {
        a1!(
            tally,
            "CMD5",
            Outcome::Fail,
            "no valid R4 after {} tries last_error={}",
            CMD5_TRIES,
            last_error
        );
        return None;
    };

    a1!(
        tally,
        "CMD5_INQUIRY",
        Outcome::Info,
        "ocr={:#08x} ready={} functions={} memory={} s18a={}",
        inquiry.ocr,
        inquiry.ready as u8,
        inquiry.functions,
        inquiry.memory_present as u8,
        inquiry.s18a as u8
    );

    let window = inquiry.common_window(HOST_WINDOW);
    if window == 0 {
        a1!(
            tally,
            "CMD5",
            Outcome::Fail,
            "no common 3.3 V window: card ocr={:#08x}",
            inquiry.ocr
        );
        return None;
    }

    // Offer the window (never 1.8 V) and wait for the ready bit.
    let mut last = inquiry;
    for attempt in 1..=CMD5_READY_TRIES {
        match exec(slot, &sdio::sdio::io_send_op_cond(false, window)) {
            Ok(r4) => {
                last = R4Info::decode(r4.ocr);
                if last.ready {
                    a1!(
                        tally,
                        "CMD5",
                        Outcome::Pass,
                        "ocr={:#08x} ready=1 functions={} memory={} s18a={} polls={}",
                        last.ocr,
                        last.functions,
                        last.memory_present as u8,
                        last.s18a as u8,
                        attempt
                    );
                    return Some(last);
                }
            }
            Err(e) => {
                a1!(tally, "CMD5", Outcome::Fail, "error={} while polling ready", error_name(&e));
                return None;
            }
        }
        delay.delay_millis(CMD5_READY_GAP_MS);
    }
    a1!(
        tally,
        "CMD5",
        Outcome::Fail,
        "card never reported ready after {} polls ocr={:#08x}",
        CMD5_READY_TRIES,
        last.ocr
    );
    None
}

/// Functions announced by the card versus the TRM.
fn check_function_count(tally: &mut HostTally, r4: &R4Info) {
    let outcome = if r4.functions == EXPECTED_FUNCTIONS {
        Outcome::Pass
    } else {
        Outcome::Fail
    };
    a1!(
        tally,
        "CMD5_FUNCTIONS",
        outcome,
        "announced={} expected={} (TRM 34.2) memory_present={}",
        r4.functions,
        EXPECTED_FUNCTIONS,
        r4.memory_present as u8
    );
}

/// H2: CMD3.
fn h2_cmd3(slot: &mut HostSlot<'_>, tally: &mut HostTally) -> Option<u16> {
    match exec(slot, &sdio::sd::Cmd3) {
        Ok(r6) => {
            let info = R6Info {
                rca: r6.rca,
                status: r6.status,
            };
            if info.has_error() {
                a1!(
                    tally,
                    "CMD3",
                    Outcome::Fail,
                    "rca={:#06x} status={:#06x} crc={} illegal={} error={}",
                    info.rca,
                    info.status,
                    info.com_crc_error() as u8,
                    info.illegal_command() as u8,
                    info.error() as u8
                );
                None
            } else if info.rca == 0 {
                a1!(
                    tally,
                    "CMD3",
                    Outcome::Fail,
                    "rca=0 status={:#06x}: a card at RCA 0 cannot be selected",
                    info.status
                );
                None
            } else {
                a1!(tally, "CMD3", Outcome::Pass, "rca={:#06x} status={:#06x}", info.rca, info.status);
                Some(info.rca)
            }
        }
        Err(e) => {
            a1!(tally, "CMD3", Outcome::Fail, "error={}", error_name(&e));
            None
        }
    }
}

/// H3: CMD7.
fn h3_cmd7(slot: &mut HostSlot<'_>, tally: &mut HostTally, rca: u16) -> bool {
    match exec(slot, &sdio::common::select_card(rca)) {
        Ok(r1) => {
            a1!(
                tally,
                "CMD7",
                Outcome::Pass,
                "rca={:#06x} r1_status={:#010x} (selection state is not decoded: SDIO R1 layout \
                 not confirmed)",
                rca,
                r1.status
            );
            true
        }
        Err(e) => {
            a1!(tally, "CMD7", Outcome::Fail, "rca={:#06x} error={}", rca, error_name(&e));
            false
        }
    }
}

/// H4: CCCR. Returns the decoded register block.
fn h4_cccr(slot: &mut HostSlot<'_>, tally: &mut HostTally) -> Option<Cccr> {
    let mut raw = [0u8; CCCR_LEN];
    for (addr, byte) in raw.iter_mut().enumerate() {
        match cmd52_read(slot, 0, addr as u32) {
            Ok(value) => *byte = value,
            Err(e) if addr <= 0x13 => {
                a1!(
                    tally,
                    "CCCR_READ",
                    Outcome::Fail,
                    "addr={:#04x} error={}",
                    addr,
                    describe(e)
                );
                return None;
            }
            Err(e) => {
                // 0x14..=0x16 are SDIO 3.0 registers, outside TRM Table 34.5-1: informational.
                a1!(
                    tally,
                    "CCCR_READ_EXT",
                    Outcome::Info,
                    "addr={:#04x} error={} (optional register, left at 0)",
                    addr,
                    describe(e)
                );
            }
        }
    }
    let cccr = Cccr { raw };
    a1!(tally, "CCCR_READ", Outcome::Pass, "bytes=0x00..=0x{:02x}", CCCR_LEN - 1);
    a1!(
        tally,
        "CCCR_VERSION",
        Outcome::Info,
        "cccr_rev={} sdio_rev={} sd_rev={} bus_ctrl={:#04x} card_cap={:#04x} cis_ptr={:#08x}",
        cccr.cccr_rev(),
        cccr.sdio_rev(),
        cccr.sd_rev(),
        cccr.bus_ctrl(),
        cccr.card_cap(),
        cccr.cis_ptr()
    );
    a1!(
        tally,
        "CCCR_IO",
        Outcome::Info,
        "ioe={:#04x} ior={:#04x} ien={:#04x} intpend={:#04x}",
        cccr.io_enable(),
        cccr.io_ready(),
        cccr.int_enable(),
        cccr.int_pending()
    );
    a1!(
        tally,
        "CCCR_IOR",
        if cccr.function_ready_clear() { Outcome::Pass } else { Outcome::Fail },
        "ior={:#04x} (IOR1 and IOR2 must read 0 in A1)",
        cccr.io_ready()
    );
    let dev = cccr.trm_deviations();
    a1!(
        tally,
        "CCCR_TRM",
        Outcome::Info,
        "deviates={} cis_ptr={} card_cap_fixed={} scsi={} io_reserved={}",
        dev.any() as u8,
        dev.cis_ptr as u8,
        dev.card_cap_fixed as u8,
        dev.scsi as u8,
        dev.io_reserved as u8
    );
    a1!(
        tally,
        "CCCR_CAPS",
        Outcome::Info,
        "high_speed={:#04x} uhs={:#04x} driver_strength={:#04x} int_ext={:#04x} \
         advertises_unimplemented={} (A1 uses none of them)",
        cccr.high_speed(),
        cccr.uhs_support(),
        cccr.driver_strength(),
        cccr.int_extension(),
        cccr.advertises_unimplemented() as u8
    );
    Some(cccr)
}

/// H5: the FBR of `function`. Returns its decoded content.
fn h5_fbr(slot: &mut HostSlot<'_>, tally: &mut HostTally, function: u8) -> Option<Fbr> {
    let base = fbr_base(function);
    let mut raw = [0u8; FBR_OFFSETS.len()];
    for (byte, offset) in raw.iter_mut().zip(FBR_OFFSETS) {
        match cmd52_read(slot, 0, base + offset as u32) {
            Ok(value) => *byte = value,
            Err(e) => {
                a1!(
                    tally,
                    "FBR_READ",
                    Outcome::Fail,
                    "function={} addr={:#06x} error={}",
                    function,
                    base + offset as u32,
                    describe(e)
                );
                return None;
            }
        }
    }
    let fbr = Fbr { raw };
    a1!(
        tally,
        "FBR_READ",
        Outcome::Pass,
        "function={} interface={:#x} ext_interface={:#04x} csa_supported={} csa_enabled={} \
         cis_ptr={:#08x} block_size={}",
        function,
        fbr.interface_code(),
        fbr.extended_interface_code(),
        fbr.csa_supported() as u8,
        fbr.csa_enabled() as u8,
        fbr.cis_ptr(),
        fbr.block_size()
    );
    if let Some(expected) = Fbr::trm_cis_ptr(function)
        && fbr.cis_ptr() != expected
    {
        a1!(
            tally,
            "FBR_TRM",
            Outcome::Info,
            "function={} cis_ptr={:#08x} differs from TRM value {:#08x}",
            function,
            fbr.cis_ptr(),
            expected
        );
    }
    Some(fbr)
}

/// H5: a bounded walk of the CIS that starts at `ptr` in the function 0 address space.
fn h5_cis(slot: &mut HostSlot<'_>, tally: &mut HostTally, name: &str, ptr: u32) {
    // The CIS lives in the 17-bit function 0 address space.
    if ptr >= 0x2_0000 {
        a1!(tally, name, Outcome::Fail, "cis_ptr={:#08x} is outside the 17-bit address space", ptr);
        return;
    }
    let mut last_error: Option<ReadError> = None;
    let report: CisReport = walk_cis(CisLimits::DEFAULT, |offset| {
        let addr = ptr + offset as u32;
        if addr >= 0x2_0000 {
            last_error = Some(ReadError::Host("address_overflow"));
            return Err(());
        }
        cmd52_read(slot, 0, addr).map_err(|e| {
            last_error = Some(e);
        })
    });

    for (i, tuple) in report.tuples[..report.kept as usize].iter().enumerate() {
        println!(
            "SDIO-A1 P4 {} INFO run={} tuple={} code={:#04x} link={} head={:02x?}",
            name,
            RUN_ID,
            i,
            tuple.code,
            tuple.link,
            &tuple.head[..tuple.head_len as usize]
        );
    }
    let (outcome, why) = match report.end {
        CisEnd::EndTuple | CisEnd::LinkTerminator if report.is_well_formed() => {
            (Outcome::Pass, "well_formed")
        }
        CisEnd::EndTuple | CisEnd::LinkTerminator => (
            Outcome::Fail,
            "empty_cis (no tuple before the end marker; reset CIS content is an open point)",
        ),
        CisEnd::TupleLimit => (Outcome::Fail, "tuple_limit_reached_without_end"),
        CisEnd::ByteLimit => (Outcome::Fail, "byte_limit_reached_without_end"),
        CisEnd::ReadError => (
            Outcome::Fail,
            last_error.map(describe).unwrap_or("read_error"),
        ),
    };
    a1!(
        tally,
        name,
        outcome,
        "ptr={:#08x} end={:?} tuples={} bytes={} vers1_first={} manfid={:x?} reason={}",
        ptr,
        report.end,
        report.seen,
        report.bytes,
        report.starts_with_vers1() as u8,
        report.manfid(),
        why
    );
}

/// One complete campaign: reset, boot wait, then H1 to H5.
fn run_round(
    round: u32,
    slot: &mut HostSlot<'_>,
    reset: &mut C6Reset<'_>,
    delay: &Delay,
) -> HostTally {
    let mut tally = HostTally::default();
    println!(
        "SDIO-A1 P4 ROUND_START INFO run={} round={}/{}",
        RUN_ID, round, ROUNDS
    );

    // Whatever the C6 was doing, start from a reset.
    reset.assert();
    a1!(
        tally,
        "RESET_ASSERTED",
        Outcome::Pass,
        "gpio54=low hold_ms={} (commanded, level not measured)",
        RESET_HOLD_MS
    );
    delay.delay_millis(RESET_HOLD_MS);
    reset.release();
    a1!(tally, "RESET_RELEASED", Outcome::Pass, "gpio54=high (commanded, level not measured)");
    println!(
        "SDIO-A1 P4 BOOT_WAIT INFO run={} ms={} (timer only: does not show that the C6 booted)",
        RUN_ID, BOOT_WAIT_MS
    );
    delay.delay_millis(BOOT_WAIT_MS);

    match slot.send_init_sequence() {
        Ok(()) => a1!(tally, "INIT_CLOCKS", Outcome::Pass, "80 clocks at {} Hz, 1-bit", CLOCK_HZ),
        Err(e) => {
            a1!(tally, "INIT_CLOCKS", Outcome::Fail, "error={}", error_name(&e));
            blocked(&mut tally, "CMD0..CIS", "init clocks failed");
            return tally;
        }
    }

    let Some(r4) = h1_cmd5(slot, delay, &mut tally) else {
        blocked(&mut tally, "CMD3..CIS", "CMD5 did not complete");
        return tally;
    };
    check_function_count(&mut tally, &r4);

    let Some(rca) = h2_cmd3(slot, &mut tally) else {
        blocked(&mut tally, "CMD7..CIS", "CMD3 did not complete");
        return tally;
    };
    if !h3_cmd7(slot, &mut tally, rca) {
        blocked(&mut tally, "CCCR..CIS", "CMD7 did not complete");
        return tally;
    }

    let Some(cccr) = h4_cccr(slot, &mut tally) else {
        blocked(&mut tally, "FBR..CIS", "CCCR read failed");
        return tally;
    };

    h5_cis(slot, &mut tally, "CIS_FN0", cccr.cis_ptr());
    let functions = r4.functions.min(EXPECTED_FUNCTIONS);
    for function in 1..=functions {
        if let Some(fbr) = h5_fbr(slot, &mut tally, function) {
            let name = if function == 1 { "CIS_FN1" } else { "CIS_FN2" };
            h5_cis(slot, &mut tally, name, fbr.cis_ptr());
        }
    }
    tally
}

/// Records the steps that cannot run because an earlier one failed.
fn blocked(tally: &mut HostTally, steps: &str, why: &str) {
    a1!(tally, "REMAINING", Outcome::Blocked, "steps={} not run: {}", steps, why);
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    // First action: hold the C6 in reset, whatever state it was in.
    let mut reset = C6Reset {
        en: Output::new(peripherals.GPIO54, Level::Low, OutputConfig::default()),
    };

    println!(
        "SDIO-A1 P4 BOOT INFO run={} sha={} fw=sdio-a1-p4-host scope=A1 mode=1bit hz={} slot=1 \
         writes=none",
        RUN_ID, GIT_SHA, CLOCK_HZ
    );
    println!(
        "SDIO-A1 P4 PARAMS INFO run={} reset_hold_ms={} boot_wait_ms={} cmd5_tries={} \
         cmd5_retry_ms={} rounds={} round_gap_ms={}",
        RUN_ID, RESET_HOLD_MS, BOOT_WAIT_MS, CMD5_TRIES, CMD5_RETRY_MS, ROUNDS, ROUND_GAP_MS
    );

    let mut tally = HostTally::default();

    let controller = match SdHostController::new(peripherals.SDHOST, Config::default()) {
        Ok(controller) => controller,
        Err(e) => {
            a1!(tally, "HOST_INIT", Outcome::Fail, "controller error={:?}", e);
            idle(&delay);
        }
    };
    let slot = match controller.slot::<1>(SlotConfig::default()) {
        Ok(slot) => slot,
        Err(e) => {
            a1!(tally, "HOST_INIT", Outcome::Fail, "slot error={:?}", e);
            idle(&delay);
        }
    };
    let mut slot = slot
        .with_clk(peripherals.GPIO18)
        .with_cmd(peripherals.GPIO19)
        .with_data0(peripherals.GPIO14)
        .with_data1(peripherals.GPIO15)
        .with_data2(peripherals.GPIO16)
        .with_data3(peripherals.GPIO17);

    match slot.set_bus_low_level(BusWidth::Bit1, CLOCK_HZ) {
        Ok(()) => a1!(
            tally,
            "HOST_INIT",
            Outcome::Pass,
            "slot=1 width=1bit hz={} pins=clk18 cmd19 d0..3=14..17",
            CLOCK_HZ
        ),
        Err(e) => {
            a1!(tally, "HOST_INIT", Outcome::Fail, "bus config error={:?}", e);
            idle(&delay);
        }
    }

    let mut total = HostTally::default();
    for round in 1..=ROUNDS {
        let round_tally = run_round(round, &mut slot, &mut reset, &delay);
        println!(
            "SDIO-A1 P4 ROUND_SUMMARY INFO run={} round={}/{} host_side={} pass={} fail={} \
             blocked={}",
            RUN_ID,
            round,
            ROUNDS,
            round_tally.host_side().as_str(),
            round_tally.pass,
            round_tally.fail,
            round_tally.blocked
        );
        total.pass = total.pass.saturating_add(round_tally.pass);
        total.fail = total.fail.saturating_add(round_tally.fail);
        total.blocked = total.blocked.saturating_add(round_tally.blocked);
        if round < ROUNDS {
            delay.delay_millis(ROUND_GAP_MS);
        }
    }

    println!(
        "SDIO-A1 P4 CAMPAIGN_SUMMARY INFO run={} host_side={} pass={} fail={} blocked={} \
         note=host-side only; the qualification verdict needs the C6 console log",
        RUN_ID,
        total.host_side().as_str(),
        total.pass,
        total.fail,
        total.blocked
    );
    idle(&delay)
}

/// Parks the firmware. Never busy-loops.
fn idle(delay: &Delay) -> ! {
    loop {
        delay.delay_millis(1000);
    }
}
