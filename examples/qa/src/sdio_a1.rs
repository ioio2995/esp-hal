//! Register-free helpers of the SDIO A1 qualification firmwares (P4 host, C6 slave).
//!
//! Nothing here touches hardware, so the host tests of `esp-hal-host-tests` compile this file
//! as well. Keep it free of `esp-hal` and `sdio` imports.
//!
//! Spec statements that the ESP32-C6 TRM does not give are marked **[SPEC]**: they come from the
//! SD/SDIO specifications, which were not available while writing this file, and must be
//! confirmed against them.

/// Result of one qualification step, as printed in the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The step was observed to succeed.
    Pass,
    /// The step was observed to fail.
    Fail,
    /// The step could not run, or its result cannot be established.
    Blocked,
    /// Information only: no verdict.
    Info,
}

impl Outcome {
    /// Text used in the log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Blocked => "BLOCKED",
            Outcome::Info => "INFO",
        }
    }
}

/// Host-side tally of the steps of one campaign.
///
/// A host-side result is **not** a qualification verdict: the C6 console log is the other half of
/// the evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostTally {
    /// Steps that passed.
    pub pass: u8,
    /// Steps that failed.
    pub fail: u8,
    /// Steps that were blocked.
    pub blocked: u8,
}

/// What the host side alone can conclude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSide {
    /// No step produced a verdict.
    NoEvidence,
    /// At least one step failed.
    Fail,
    /// No failure, at least one step blocked.
    Blocked,
    /// Every step that ran passed, none was blocked.
    Pass,
}

impl HostTally {
    /// Records the outcome of a step. [`Outcome::Info`] is ignored.
    pub fn record(&mut self, outcome: Outcome) {
        let slot = match outcome {
            Outcome::Pass => &mut self.pass,
            Outcome::Fail => &mut self.fail,
            Outcome::Blocked => &mut self.blocked,
            Outcome::Info => return,
        };
        *slot = slot.saturating_add(1);
    }

    /// Host-side conclusion. A failure outranks a blocked step, which outranks a pass.
    pub const fn host_side(&self) -> HostSide {
        if self.fail > 0 {
            HostSide::Fail
        } else if self.blocked > 0 {
            HostSide::Blocked
        } else if self.pass > 0 {
            HostSide::Pass
        } else {
            HostSide::NoEvidence
        }
    }
}

impl HostSide {
    /// Text used in the log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            HostSide::NoEvidence => "NO_EVIDENCE",
            HostSide::Fail => "FAIL",
            HostSide::Blocked => "BLOCKED",
            HostSide::Pass => "PASS",
        }
    }
}

/// Parses a decimal `u32` at compile time, for `option_env!` parameters.
///
/// Returns `default` when the text is missing, empty, not decimal or too large.
pub const fn parse_u32(text: Option<&str>, default: u32) -> u32 {
    let Some(text) = text else {
        return default;
    };
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return default;
    }
    let mut value: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            return default;
        }
        let digit = (b - b'0') as u32;
        value = match value.checked_mul(10) {
            Some(v) => match v.checked_add(digit) {
                Some(v) => v,
                None => return default,
            },
            None => return default,
        };
        i += 1;
    }
    value
}

// ---------------------------------------------------------------------------------------------
// C6 side: HINF_CFG_DATA1 (TRM Register 34.2)
// ---------------------------------------------------------------------------------------------

/// `HINF_SDIO_IOREADY1` in `HINF_CFG_DATA1_REG` (bit 1).
pub const HINF_CFG_DATA1_IOREADY1: u32 = 1 << 1;
/// `HINF_SDIO_IOREADY2` in `HINF_CFG_DATA1_REG` (bit 5).
pub const HINF_CFG_DATA1_IOREADY2: u32 = 1 << 5;
/// `HINF_IOENABLE1`, read-only, set by the host through CCCR `IOE1` (bit 11).
pub const HINF_CFG_DATA1_IOENABLE1: u32 = 1 << 11;
/// `HINF_IOENABLE2`, read-only, set by the host through CCCR `IOE2` (bit 7).
pub const HINF_CFG_DATA1_IOENABLE2: u32 = 1 << 7;

/// Function-ready bits of a `HINF_CFG_DATA1` value: `(IOREADY1, IOREADY2)`.
pub const fn ioready_bits(cfg_data1: u32) -> (bool, bool) {
    (
        cfg_data1 & HINF_CFG_DATA1_IOREADY1 != 0,
        cfg_data1 & HINF_CFG_DATA1_IOREADY2 != 0,
    )
}

/// Function-enable bits of a `HINF_CFG_DATA1` value: `(IOENABLE1, IOENABLE2)`.
pub const fn ioenable_bits(cfg_data1: u32) -> (bool, bool) {
    (
        cfg_data1 & HINF_CFG_DATA1_IOENABLE1 != 0,
        cfg_data1 & HINF_CFG_DATA1_IOENABLE2 != 0,
    )
}

// ---------------------------------------------------------------------------------------------
// Host side: decoding of responses and registers
// ---------------------------------------------------------------------------------------------

/// Decoded R4 response (CMD5), **[SPEC]** field layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct R4Info {
    /// Raw 32-bit response word.
    pub raw: u32,
    /// Card ready (bit 31).
    pub ready: bool,
    /// Number of I/O functions (bits 30:28).
    pub functions: u8,
    /// Memory present (bit 27).
    pub memory_present: bool,
    /// Card accepts 1.8 V signalling (bit 24).
    pub s18a: bool,
    /// I/O OCR voltage window (bits 23:0).
    pub ocr: u32,
}

/// Plausibility of an R4 word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum R4Class {
    /// Not all zeros or ones.
    Plausible,
    /// All zeros: no driven response, or a floating line read as 0.
    AllZero,
    /// All ones: a floating or pulled-up line.
    AllOnes,
}

impl R4Info {
    /// Decodes a raw R4 word.
    pub const fn decode(raw: u32) -> Self {
        Self {
            raw,
            ready: raw & (1 << 31) != 0,
            functions: ((raw >> 28) & 0x7) as u8,
            memory_present: raw & (1 << 27) != 0,
            s18a: raw & (1 << 24) != 0,
            ocr: raw & 0x00FF_FFFF,
        }
    }

    /// Classifies the raw word.
    pub const fn class(&self) -> R4Class {
        match self.raw {
            0 => R4Class::AllZero,
            0xFFFF_FFFF => R4Class::AllOnes,
            _ => R4Class::Plausible,
        }
    }

    /// The 9-bit voltage window (OCR bits 23:15) shared with a host window.
    pub const fn common_window(&self, host_window: u16) -> u16 {
        (((self.ocr >> 15) & 0x1FF) as u16) & (host_window & 0x1FF)
    }
}

/// Decoded R6 status (CMD3), **[SPEC]** bit positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct R6Info {
    /// Published relative card address.
    pub rca: u16,
    /// Raw status half-word.
    pub status: u16,
}

impl R6Info {
    /// `COM_CRC_ERROR` (status bit 15).
    pub const fn com_crc_error(&self) -> bool {
        self.status & (1 << 15) != 0
    }
    /// `ILLEGAL_COMMAND` (status bit 14).
    pub const fn illegal_command(&self) -> bool {
        self.status & (1 << 14) != 0
    }
    /// General `ERROR` (status bit 13).
    pub const fn error(&self) -> bool {
        self.status & (1 << 13) != 0
    }
    /// Whether one of the three error flags is set.
    pub const fn has_error(&self) -> bool {
        self.com_crc_error() || self.illegal_command() || self.error()
    }
}

/// Number of CCCR bytes read by the campaign: `0x00..=0x16`.
///
/// Addresses up to `0x13` are in TRM Table 34.5-1. `0x14..=0x16` (UHS-I support, driver strength,
/// interrupt extension) are **[SPEC]** SDIO 3.0 and are only reported.
pub const CCCR_LEN: usize = 0x17;

/// Decoded CCCR, function 0 address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cccr {
    /// Raw bytes `0x00..=0x16`.
    pub raw: [u8; CCCR_LEN],
}

impl Cccr {
    /// CCCR revision (`0x00` bits 3:0).
    pub const fn cccr_rev(&self) -> u8 {
        self.raw[0x00] & 0x0F
    }
    /// SDIO revision (`0x00` bits 7:4).
    pub const fn sdio_rev(&self) -> u8 {
        self.raw[0x00] >> 4
    }
    /// SD specification revision (`0x01` bits 3:0).
    pub const fn sd_rev(&self) -> u8 {
        self.raw[0x01] & 0x0F
    }
    /// I/O enable `IOEx` byte (`0x02`).
    pub const fn io_enable(&self) -> u8 {
        self.raw[0x02]
    }
    /// I/O ready `IORx` byte (`0x03`).
    pub const fn io_ready(&self) -> u8 {
        self.raw[0x03]
    }
    /// Interrupt enable (`0x04`).
    pub const fn int_enable(&self) -> u8 {
        self.raw[0x04]
    }
    /// Interrupt pending (`0x05`).
    pub const fn int_pending(&self) -> u8 {
        self.raw[0x05]
    }
    /// Bus interface control (`0x07`).
    pub const fn bus_ctrl(&self) -> u8 {
        self.raw[0x07]
    }
    /// Card capability (`0x08`).
    pub const fn card_cap(&self) -> u8 {
        self.raw[0x08]
    }
    /// Common CIS pointer (`0x09..=0x0B`, little endian).
    pub const fn cis_ptr(&self) -> u32 {
        (self.raw[0x09] as u32) | (self.raw[0x0A] as u32) << 8 | (self.raw[0x0B] as u32) << 16
    }
    /// High-speed register (`0x13`).
    pub const fn high_speed(&self) -> u8 {
        self.raw[0x13]
    }
    /// UHS-I support (`0x14`, **[SPEC]**).
    pub const fn uhs_support(&self) -> u8 {
        self.raw[0x14]
    }
    /// Driver strength (`0x15`, **[SPEC]**).
    pub const fn driver_strength(&self) -> u8 {
        self.raw[0x15]
    }
    /// Interrupt extension (`0x16`, **[SPEC]**).
    pub const fn int_extension(&self) -> u8 {
        self.raw[0x16]
    }

    /// Whether neither `IOR1` nor `IOR2` is set: the A1 contract.
    ///
    /// Only bits 2:1 are looked at; bit 0 is reserved and bits 7:3 read 0 (TRM Table 34.5-1).
    pub const fn function_ready_clear(&self) -> bool {
        self.io_ready() & 0b0000_0110 == 0
    }

    /// Whether the card advertises capabilities that the C6 driver does not implement.
    ///
    /// High speed, UHS-I modes, driver types and asynchronous interrupt are advertised by the
    /// hardware defaults (PAC reset values of `HINF_CFG_DATA7`) while A1 supports 3.3 V,
    /// 400 kHz identification mode only.
    pub const fn advertises_unimplemented(&self) -> bool {
        self.high_speed() & 0b01 != 0
            || self.uhs_support() & 0b111 != 0
            || self.driver_strength() & 0b111 != 0
            || self.int_extension() & 0b01 != 0
    }

    /// Compares the bytes with the values of TRM Table 34.5-1 that are fixed by hardware.
    pub const fn trm_deviations(&self) -> CccrDeviations {
        CccrDeviations {
            // 0x09..=0x0B: common CIS pointer is 0x001000.
            cis_ptr: self.cis_ptr() != 0x0000_1000,
            // 0x08: SDC, SMB, SRW and S4MI are 1; 4BLS, LSC and SBS are 0. E4MI is R/W.
            card_cap_fixed: self.card_cap() & 0b1101_1111 != 0b0001_0111,
            // 0x07: SCSI (bit 6) is 1.
            scsi: self.bus_ctrl() & (1 << 6) == 0,
            // 0x02/0x03: bits 7:3 and bit 0 read 0.
            io_reserved: (self.io_enable() | self.io_ready()) & 0b1111_1001 != 0,
        }
    }
}

/// Differences between a CCCR read and the fixed values of TRM Table 34.5-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CccrDeviations {
    /// The common CIS pointer is not 0x001000.
    pub cis_ptr: bool,
    /// A fixed card-capability bit differs.
    pub card_cap_fixed: bool,
    /// `SCSI` is not 1.
    pub scsi: bool,
    /// A reserved bit of `IOEx`/`IORx` is set.
    pub io_reserved: bool,
}

impl CccrDeviations {
    /// Whether any difference was found.
    pub const fn any(&self) -> bool {
        self.cis_ptr || self.card_cap_fixed || self.scsi || self.io_reserved
    }
}

/// Offsets read in each function basic register (FBR) block.
pub const FBR_OFFSETS: [u8; 8] = [0x00, 0x01, 0x02, 0x09, 0x0A, 0x0B, 0x10, 0x11];

/// Base address of the FBR of function `function` (1..=7): `0x100 * function`.
pub const fn fbr_base(function: u8) -> u32 {
    0x100 * function as u32
}

/// Decoded FBR of one function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fbr {
    /// Raw bytes at [`FBR_OFFSETS`].
    pub raw: [u8; 8],
}

impl Fbr {
    /// Standard SDIO function interface code (`0x00` bits 3:0).
    pub const fn interface_code(&self) -> u8 {
        self.raw[0] & 0x0F
    }
    /// Function supports Code Storage Area (`0x00` bit 6).
    pub const fn csa_supported(&self) -> bool {
        self.raw[0] & (1 << 6) != 0
    }
    /// CSA enabled (`0x00` bit 7).
    pub const fn csa_enabled(&self) -> bool {
        self.raw[0] & (1 << 7) != 0
    }
    /// Extended standard function interface code (`0x01`).
    pub const fn extended_interface_code(&self) -> u8 {
        self.raw[1]
    }
    /// Power selection (`0x02`).
    pub const fn power_select(&self) -> u8 {
        self.raw[2]
    }
    /// CIS pointer (`0x09..=0x0B`, little endian).
    pub const fn cis_ptr(&self) -> u32 {
        (self.raw[3] as u32) | (self.raw[4] as u32) << 8 | (self.raw[5] as u32) << 16
    }
    /// I/O block size (`0x10..=0x11`).
    pub const fn block_size(&self) -> u16 {
        (self.raw[6] as u16) | (self.raw[7] as u16) << 8
    }

    /// CIS pointer that TRM Tables 34.5-1/34.5-2 give for `function` (1 or 2), if any.
    pub const fn trm_cis_ptr(function: u8) -> Option<u32> {
        match function {
            1 => Some(0x0000_1100),
            2 => Some(0x0000_1200),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// CIS walk
// ---------------------------------------------------------------------------------------------

/// Tuples whose header is kept in a [`CisReport`].
pub const CIS_KEPT: usize = 16;

/// Bytes of tuple body kept per tuple.
pub const CIS_HEAD: usize = 4;

/// Tuple code `CISTPL_NULL`: a single byte with no link field.
pub const CISTPL_NULL: u8 = 0x00;
/// Tuple code `CISTPL_VERS_1`.
pub const CISTPL_VERS_1: u8 = 0x15;
/// Tuple code `CISTPL_MANFID`.
pub const CISTPL_MANFID: u8 = 0x20;
/// Tuple code `CISTPL_FUNCID`.
pub const CISTPL_FUNCID: u8 = 0x21;
/// Tuple code `CISTPL_FUNCE`.
pub const CISTPL_FUNCE: u8 = 0x22;
/// Tuple code `CISTPL_END`.
pub const CISTPL_END: u8 = 0xFF;

/// Bounds of a CIS walk. Both limits are enforced, so a corrupted CIS cannot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CisLimits {
    /// Maximum number of tuples examined.
    pub max_tuples: u8,
    /// Maximum number of CIS bytes covered (headers and bodies).
    pub max_bytes: u16,
}

impl CisLimits {
    /// Limits used by the campaign: 16 tuples, 256 bytes.
    pub const DEFAULT: Self = Self {
        max_tuples: 16,
        max_bytes: 256,
    };
}

/// Why a CIS walk stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CisEnd {
    /// `CISTPL_END` (0xFF) found where a tuple code was expected.
    EndTuple,
    /// A tuple had link 0xFF, which ends the chain.
    LinkTerminator,
    /// [`CisLimits::max_tuples`] reached without an end marker.
    TupleLimit,
    /// [`CisLimits::max_bytes`] reached without an end marker.
    ByteLimit,
    /// The byte reader failed.
    ReadError,
}

/// Header of one tuple.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CisTuple {
    /// Tuple code.
    pub code: u8,
    /// Link field: the body length.
    pub link: u8,
    /// First bytes of the body, zero padded.
    pub head: [u8; CIS_HEAD],
    /// Number of valid bytes in `head`.
    pub head_len: u8,
}

/// Result of a CIS walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CisReport {
    /// First [`CIS_KEPT`] tuples.
    pub tuples: [CisTuple; CIS_KEPT],
    /// Number of tuples stored in `tuples`.
    pub kept: u8,
    /// Number of tuples examined, including those not kept.
    pub seen: u8,
    /// Number of CIS bytes covered.
    pub bytes: u16,
    /// Why the walk stopped.
    pub end: CisEnd,
    /// CIS offset of the failed read, when `end` is [`CisEnd::ReadError`].
    pub error_offset: Option<u16>,
}

impl CisReport {
    /// Whether the walk reached a normal end marker after at least one tuple.
    pub const fn is_well_formed(&self) -> bool {
        self.seen > 0 && matches!(self.end, CisEnd::EndTuple | CisEnd::LinkTerminator)
    }

    /// Manufacturer and card ID from the first `CISTPL_MANFID` tuple, if kept and long enough.
    pub fn manfid(&self) -> Option<(u16, u16)> {
        self.tuples[..self.kept as usize]
            .iter()
            .find(|t| t.code == CISTPL_MANFID && t.head_len >= 4)
            .map(|t| {
                (
                    u16::from_le_bytes([t.head[0], t.head[1]]),
                    u16::from_le_bytes([t.head[2], t.head[3]]),
                )
            })
    }

    /// Whether the first tuple is `CISTPL_VERS_1`, which the specification requires first in the
    /// function 0 CIS (**[SPEC]**).
    pub fn starts_with_vers1(&self) -> bool {
        self.kept > 0 && self.tuples[0].code == CISTPL_VERS_1
    }
}

/// Walks a CIS with explicit bounds.
///
/// `read` returns the byte at a CIS offset, counted from the start of the walk. Tuples are read as
/// `code`, `link`, then `link` body bytes. `CISTPL_NULL` is a single byte. The walk never reads
/// more than [`CisLimits::max_bytes`] bytes of header and body together, and the body bytes beyond
/// [`CIS_HEAD`] are skipped, not read.
pub fn walk_cis<E>(
    limits: CisLimits,
    mut read: impl FnMut(u16) -> Result<u8, E>,
) -> CisReport {
    let mut report = CisReport {
        tuples: [CisTuple::default(); CIS_KEPT],
        kept: 0,
        seen: 0,
        bytes: 0,
        end: CisEnd::TupleLimit,
        error_offset: None,
    };
    let mut offset: u16 = 0;

    macro_rules! read_at {
        ($off:expr) => {
            match read($off) {
                Ok(byte) => byte,
                Err(_) => {
                    report.end = CisEnd::ReadError;
                    report.error_offset = Some($off);
                    return report;
                }
            }
        };
    }

    loop {
        if report.seen >= limits.max_tuples {
            report.end = CisEnd::TupleLimit;
            return report;
        }
        if offset >= limits.max_bytes {
            report.end = CisEnd::ByteLimit;
            return report;
        }

        let code = read_at!(offset);
        report.bytes = report.bytes.saturating_add(1);
        if code == CISTPL_END {
            report.end = CisEnd::EndTuple;
            return report;
        }
        if code == CISTPL_NULL {
            offset += 1;
            report.seen += 1;
            continue;
        }
        if offset + 1 >= limits.max_bytes {
            report.end = CisEnd::ByteLimit;
            return report;
        }

        let link = read_at!(offset + 1);
        report.bytes = report.bytes.saturating_add(1);
        if link == 0xFF {
            report.end = CisEnd::LinkTerminator;
            return report;
        }

        let body_end = offset as u32 + 2 + link as u32;
        if body_end > limits.max_bytes as u32 {
            report.end = CisEnd::ByteLimit;
            return report;
        }

        let mut tuple = CisTuple {
            code,
            link,
            ..CisTuple::default()
        };
        let head_len = (link as usize).min(CIS_HEAD);
        for i in 0..head_len {
            tuple.head[i] = read_at!(offset + 2 + i as u16);
        }
        tuple.head_len = head_len as u8;
        report.bytes = report.bytes.saturating_add(link as u16);

        if (report.kept as usize) < CIS_KEPT {
            report.tuples[report.kept as usize] = tuple;
            report.kept += 1;
        }
        report.seen += 1;
        offset = body_end as u16;
    }
}
