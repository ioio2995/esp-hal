use super::proto::*;

// ---- parameters and tally ----

#[test]
fn parse_u32_accepts_decimal_and_falls_back() {
    assert_eq!(parse_u32(Some("1500"), 7), 1500);
    assert_eq!(parse_u32(Some("0"), 7), 0);
    assert_eq!(parse_u32(None, 7), 7);
    assert_eq!(parse_u32(Some(""), 7), 7);
    assert_eq!(parse_u32(Some("12a"), 7), 7);
    assert_eq!(parse_u32(Some("-3"), 7), 7);
    assert_eq!(parse_u32(Some("99999999999"), 7), 7);
}

#[test]
fn tally_ranks_fail_over_blocked_over_pass() {
    let mut t = HostTally::default();
    assert_eq!(t.host_side(), HostSide::NoEvidence);
    t.record(Outcome::Info);
    assert_eq!(t.host_side(), HostSide::NoEvidence);
    t.record(Outcome::Pass);
    assert_eq!(t.host_side(), HostSide::Pass);
    t.record(Outcome::Blocked);
    assert_eq!(t.host_side(), HostSide::Blocked);
    t.record(Outcome::Fail);
    assert_eq!(t.host_side(), HostSide::Fail);
    assert_eq!((t.pass, t.blocked, t.fail), (1, 1, 1));
}

#[test]
fn tally_saturates() {
    let mut t = HostTally::default();
    for _ in 0..300 {
        t.record(Outcome::Pass);
    }
    assert_eq!(t.pass, 255);
}

#[test]
fn outcome_text() {
    assert_eq!(Outcome::Pass.as_str(), "PASS");
    assert_eq!(Outcome::Fail.as_str(), "FAIL");
    assert_eq!(Outcome::Blocked.as_str(), "BLOCKED");
    assert_eq!(Outcome::Info.as_str(), "INFO");
}

// ---- C6 side ----

#[test]
fn ioready_and_ioenable_bits_follow_trm_positions() {
    assert_eq!(ioready_bits(0), (false, false));
    assert_eq!(ioready_bits(1 << 1), (true, false));
    assert_eq!(ioready_bits(1 << 5), (false, true));
    // Reset value of HINF_CFG_DATA1: both function-ready bits clear.
    assert_eq!(ioready_bits(0x0023_2011), (false, false));
    assert_eq!(ioenable_bits(1 << 11), (true, false));
    assert_eq!(ioenable_bits(1 << 7), (false, true));
    assert_eq!(ioenable_bits(0x0023_2011), (false, false));
}

// ---- R4 / R6 ----

#[test]
fn r4_decodes_fields() {
    // ready, 1 function, no memory, window 0x1FF << 15
    let raw = 1 << 31 | 1 << 28 | 0x1FF << 15;
    let r = R4Info::decode(raw);
    assert!(r.ready);
    assert_eq!(r.functions, 1);
    assert!(!r.memory_present);
    assert!(!r.s18a);
    assert_eq!(r.ocr, 0x00FF_8000);
    assert_eq!(r.class(), R4Class::Plausible);
}

#[test]
fn r4_flags_implausible_words() {
    assert_eq!(R4Info::decode(0).class(), R4Class::AllZero);
    assert_eq!(R4Info::decode(0xFFFF_FFFF).class(), R4Class::AllOnes);
}

#[test]
fn r4_common_window_intersects() {
    let r = R4Info::decode(0x0030_0000 | 1 << 28); // bits 21,20 -> window bits 6,5
    assert_eq!(r.common_window(0x1FF), 0b0_0110_0000);
    assert_eq!(r.common_window(0b1_0000_0000), 0);
}

#[test]
fn r6_error_flags() {
    let ok = R6Info { rca: 0x0001, status: 0 };
    assert!(!ok.has_error());
    assert!(R6Info { rca: 1, status: 1 << 15 }.com_crc_error());
    assert!(R6Info { rca: 1, status: 1 << 14 }.illegal_command());
    assert!(R6Info { rca: 1, status: 1 << 13 }.error());
    assert!(R6Info { rca: 1, status: 1 << 13 }.has_error());
}

// ---- CCCR / FBR ----

fn trm_default_cccr() -> Cccr {
    let mut raw = [0u8; CCCR_LEN];
    raw[0x00] = 0x32; // SDIO rev 3, CCCR rev 2 (HINF_SDIO_VER = 0x232)
    raw[0x01] = 0x02;
    raw[0x07] = 0x40; // SCSI
    raw[0x08] = 0x17; // S4MI, SRW, SMB, SDC
    raw[0x09] = 0x00;
    raw[0x0A] = 0x10;
    raw[0x0B] = 0x00;
    Cccr { raw }
}

#[test]
fn cccr_decodes_revisions_and_pointer() {
    let c = trm_default_cccr();
    assert_eq!(c.cccr_rev(), 2);
    assert_eq!(c.sdio_rev(), 3);
    assert_eq!(c.sd_rev(), 2);
    assert_eq!(c.cis_ptr(), 0x1000);
    assert!(c.function_ready_clear());
    assert!(!c.trm_deviations().any());
}

#[test]
fn cccr_detects_function_ready() {
    let mut c = trm_default_cccr();
    c.raw[0x03] = 1 << 1;
    assert!(!c.function_ready_clear());
    c.raw[0x03] = 1 << 2;
    assert!(!c.function_ready_clear());
    c.raw[0x03] = 0;
    assert!(c.function_ready_clear());
}

#[test]
fn cccr_deviations_are_reported() {
    let mut c = trm_default_cccr();
    c.raw[0x0A] = 0x20;
    assert!(c.trm_deviations().cis_ptr);
    let mut c = trm_default_cccr();
    c.raw[0x08] = 0x00;
    assert!(c.trm_deviations().card_cap_fixed);
    let mut c = trm_default_cccr();
    c.raw[0x08] |= 1 << 5; // E4MI is read/write: not a deviation
    assert!(!c.trm_deviations().card_cap_fixed);
    let mut c = trm_default_cccr();
    c.raw[0x07] = 0;
    assert!(c.trm_deviations().scsi);
    let mut c = trm_default_cccr();
    c.raw[0x02] = 1 << 4;
    assert!(c.trm_deviations().io_reserved);
}

#[test]
fn cccr_unimplemented_capabilities() {
    let mut c = trm_default_cccr();
    assert!(!c.advertises_unimplemented());
    c.raw[0x13] = 1;
    assert!(c.advertises_unimplemented());
    let mut c = trm_default_cccr();
    c.raw[0x14] = 0b100;
    assert!(c.advertises_unimplemented());
    let mut c = trm_default_cccr();
    c.raw[0x16] = 1;
    assert!(c.advertises_unimplemented());
}

#[test]
fn fbr_decodes_fields() {
    let f = Fbr {
        raw: [0x40 | 0x0, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x02],
    };
    assert_eq!(f.interface_code(), 0);
    assert!(f.csa_supported());
    assert!(!f.csa_enabled());
    assert_eq!(f.cis_ptr(), 0x1100);
    assert_eq!(f.block_size(), 512);
    assert_eq!(Fbr::trm_cis_ptr(1), Some(0x1100));
    assert_eq!(Fbr::trm_cis_ptr(2), Some(0x1200));
    assert_eq!(Fbr::trm_cis_ptr(3), None);
    assert_eq!(fbr_base(1), 0x100);
    assert_eq!(fbr_base(2), 0x200);
}

// ---- CIS walk ----

fn reader(bytes: &'static [u8]) -> impl FnMut(u16) -> Result<u8, ()> {
    move |off| bytes.get(off as usize).copied().ok_or(())
}

#[test]
fn cis_normal_chain_ends_on_end_tuple() {
    // VERS_1 (len 3), MANFID (len 4), END
    static CIS: &[u8] = &[
        0x15, 0x03, 0x01, 0x01, 0xFF, // VERS_1 (body 3 bytes)
        0x20, 0x04, 0x34, 0x12, 0x78, 0x56, // MANFID
        0xFF,
    ];
    let r = walk_cis(CisLimits::DEFAULT, reader(CIS));
    assert_eq!(r.end, CisEnd::EndTuple);
    assert_eq!(r.seen, 2);
    assert_eq!(r.kept, 2);
    assert!(r.is_well_formed());
    assert!(r.starts_with_vers1());
    assert_eq!(r.manfid(), Some((0x1234, 0x5678)));
}

#[test]
fn cis_null_tuples_are_single_bytes() {
    static CIS: &[u8] = &[0x00, 0x00, 0x21, 0x02, 0x0C, 0x00, 0xFF];
    let r = walk_cis(CisLimits::DEFAULT, reader(CIS));
    assert_eq!(r.end, CisEnd::EndTuple);
    assert_eq!(r.seen, 3);
    assert_eq!(r.kept, 1);
    assert_eq!(r.tuples[0].code, CISTPL_FUNCID);
}

#[test]
fn cis_link_ff_terminates_the_chain() {
    static CIS: &[u8] = &[0x15, 0xFF, 0x00, 0x00];
    let r = walk_cis(CisLimits::DEFAULT, reader(CIS));
    assert_eq!(r.end, CisEnd::LinkTerminator);
    assert_eq!(r.seen, 0);
    assert!(!r.is_well_formed());
}

#[test]
fn cis_all_ones_is_an_immediate_end_tuple_without_tuples() {
    // Reset value 0xffffffff of HINF_CIS_CONF_Wn read as bytes.
    static CIS: &[u8] = &[0xFF; 8];
    let r = walk_cis(CisLimits::DEFAULT, reader(CIS));
    assert_eq!(r.end, CisEnd::EndTuple);
    assert_eq!(r.seen, 0);
    assert!(!r.is_well_formed(), "an empty CIS is not well formed");
}

#[test]
fn cis_tuple_limit_stops_an_endless_chain() {
    // Endless chain of empty tuples (code 0x30, link 0).
    let mut count = 0u32;
    let limits = CisLimits { max_tuples: 5, max_bytes: 4096 };
    let r = walk_cis(limits, |off| {
        count += 1;
        Ok::<_, ()>(if off % 2 == 0 { 0x30 } else { 0x00 })
    });
    assert_eq!(r.end, CisEnd::TupleLimit);
    assert_eq!(r.seen, 5);
    assert!(count <= 10 + 1, "bounded reads: {count}");
}

#[test]
fn cis_byte_limit_stops_a_long_chain() {
    let mut count = 0u32;
    let limits = CisLimits { max_tuples: 255, max_bytes: 16 };
    let r = walk_cis(limits, |off| {
        count += 1;
        Ok::<_, ()>(if off % 2 == 0 { 0x30 } else { 0x00 })
    });
    assert_eq!(r.end, CisEnd::ByteLimit);
    assert!(count <= 16, "never reads past max_bytes: {count}");
}

#[test]
fn cis_body_longer_than_the_limit_is_rejected_without_reading_it() {
    let mut max_off = 0u16;
    let limits = CisLimits { max_tuples: 8, max_bytes: 32 };
    let r = walk_cis(limits, |off| {
        max_off = max_off.max(off);
        Ok::<_, ()>(if off == 0 { 0x15 } else { 0xF0 }) // link 0xF0 = 240 body bytes
    });
    assert_eq!(r.end, CisEnd::ByteLimit);
    assert!(max_off < 32);
}

#[test]
fn cis_read_error_is_reported_with_its_offset() {
    static CIS: &[u8] = &[0x15, 0x03, 0x01];
    let r = walk_cis(CisLimits::DEFAULT, reader(CIS));
    assert_eq!(r.end, CisEnd::ReadError);
    assert_eq!(r.error_offset, Some(3));
}

#[test]
fn cis_reader_only_sees_head_bytes_of_long_bodies() {
    // One tuple with a 20-byte body, then END at offset 22.
    let mut cis = [0u8; 23];
    cis[0] = 0x16;
    cis[1] = 20;
    cis[22] = 0xFF;
    let mut reads = 0u32;
    let r = walk_cis(CisLimits::DEFAULT, |off| {
        reads += 1;
        Ok::<_, ()>(cis[off as usize])
    });
    assert_eq!(r.end, CisEnd::EndTuple);
    // code + link + 4 head bytes, then the end tuple.
    assert_eq!(reads, 2 + 4 + 1);
    assert_eq!(r.bytes, 2 + 20 + 1);
}

#[test]
fn cis_keeps_only_the_first_tuples() {
    // 20 tuples (code 0x30, link 0) then END.
    let mut cis = [0u8; 41];
    for i in 0..20 {
        cis[2 * i] = 0x30;
    }
    cis[40] = 0xFF;
    let limits = CisLimits { max_tuples: 32, max_bytes: 256 };
    let r = walk_cis(limits, |off| Ok::<_, ()>(cis[off as usize]));
    assert_eq!(r.end, CisEnd::EndTuple);
    assert_eq!(r.seen, 20);
    assert_eq!(r.kept as usize, CIS_KEPT);
}
