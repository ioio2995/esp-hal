//! Pure parts of the SDIO throughput benchmark shared by `sdio-bench-host` (ESP32-P4) and
//! `sdio-bench-slave` (ESP32-C6): the plan tables, the sequenced packet pattern, the control and
//! summary packets, the error classes and the goodput arithmetic. Nothing here touches hardware,
//! so everything is checked on the host.
//!
//! The benchmark is separate from the H0-H7 qualification scenarios and never changes the driver.

/// Plan compiled into the firmware (`SDIO_BENCH_PLAN`, default `smoke`).
pub const PLAN_NAME: &str = match option_env!("SDIO_BENCH_PLAN") {
    Some(name) => name,
    None => "smoke",
};

/// Plan selected at build time.
pub const PLAN: Plan = match Plan::parse(PLAN_NAME) {
    Some(plan) => plan,
    None => panic!("SDIO_BENCH_PLAN must be one of: smoke, qualified, w1, w4, full"),
};

/// Which configurations a run measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The qualified configuration and 64-byte packets, one short window. A dry run of the whole
    /// mechanism, not a result.
    Smoke,
    /// The qualified configuration (400 kHz, 1 bit) with every packet size.
    Qualified,
    /// 1-bit bus, every frequency above the qualified one, every packet size.
    W1,
    /// 4-bit bus, every frequency, every packet size.
    W4,
    /// `Qualified`, `W1` and `W4` in this order.
    Full,
}

impl Plan {
    /// Parses a plan name in a const context.
    pub const fn parse(name: &str) -> Option<Self> {
        let name = name.as_bytes();
        if eq(name, b"smoke") {
            Some(Self::Smoke)
        } else if eq(name, b"qualified") {
            Some(Self::Qualified)
        } else if eq(name, b"w1") {
            Some(Self::W1)
        } else if eq(name, b"w4") {
            Some(Self::W4)
        } else if eq(name, b"full") {
            Some(Self::Full)
        } else {
            None
        }
    }

    /// `(measurements per direction, window in ms, confirmation in ms)`.
    pub const fn timing(self) -> (u32, u32, u32) {
        match self {
            Self::Smoke => (1, 1_000, 0),
            _ => (MEASUREMENTS, WINDOW_MS, CONFIRM_MS),
        }
    }
}

const fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Short measurements per configuration and direction, after the warm-up.
pub const MEASUREMENTS: u32 = 3;
/// Length of one short measurement.
pub const WINDOW_MS: u32 = 5_000;
/// Length of the confirmation of the best error-free configuration.
pub const CONFIRM_MS: u32 = 30_000;
/// Packets of the warm-up, which also gives the packet time used to size the measurements.
pub const WARMUP_PACKETS: u32 = 8;

/// Packet sizes, all multiples of the 64-byte cache line. Up to `BYTE_MODE_MAX` they travel in
/// CMD53 byte mode; above it in block mode with `BLOCK_SIZE`.
pub const SIZES: [u32; 4] = [64, 256, 1024, 4096];
pub const BYTE_MODE_MAX: u32 = 256;
pub const BLOCK_SIZE: u32 = 512;

/// Frequencies requested from the host, ascending. The first one is the qualified one.
pub const REQUESTED_HZ: [u32; 5] = [400_000, 5_000_000, 10_000_000, 20_000_000, 40_000_000];
/// Module clock of the P4 SDMMC controller with the default `esp-hal` configuration
/// (160 MHz PLL divided by 2).
pub const MODULE_HZ: u32 = 80_000_000;

/// The card clock the host really produces: `module / (2 * div)`, `div == 0` bypassing the
/// divider. A request that is not `module / (2 * n)` is rounded down to the nearest one.
pub const fn card_clock_hz(requested: u32) -> u32 {
    if requested == 0 || MODULE_HZ <= requested {
        return MODULE_HZ;
    }
    let mut div = MODULE_HZ.div_ceil(2 * requested);
    if div < 1 {
        div = 1;
    }
    if div > 255 {
        div = 255;
    }
    MODULE_HZ / (2 * div)
}

/// Bus width of a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    One,
    Four,
}

impl Width {
    pub const fn bits(self) -> u32 {
        match self {
            Self::One => 1,
            Self::Four => 4,
        }
    }
}

/// One benchmark configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub requested_hz: u32,
    pub width: Width,
    pub size: u32,
}

/// Number of configurations of a plan.
pub const fn config_count(plan: Plan) -> usize {
    match plan {
        Plan::Smoke => 1,
        Plan::Qualified => SIZES.len(),
        Plan::W1 => (REQUESTED_HZ.len() - 1) * SIZES.len(),
        Plan::W4 => REQUESTED_HZ.len() * SIZES.len(),
        Plan::Full => {
            SIZES.len() + (REQUESTED_HZ.len() - 1) * SIZES.len() + REQUESTED_HZ.len() * SIZES.len()
        }
    }
}

/// The `n`-th configuration of a plan, in execution order: ascending frequency, then ascending
/// size, so that the ladder rule can skip what a failed lower frequency already excludes.
pub const fn config(plan: Plan, n: usize) -> Option<Config> {
    if n >= config_count(plan) {
        return None;
    }
    let sizes = SIZES.len();
    Some(match plan {
        Plan::Smoke => Config {
            requested_hz: REQUESTED_HZ[0],
            width: Width::One,
            size: SIZES[0],
        },
        Plan::Qualified => Config {
            requested_hz: REQUESTED_HZ[0],
            width: Width::One,
            size: SIZES[n],
        },
        Plan::W1 => Config {
            requested_hz: REQUESTED_HZ[1 + n / sizes],
            width: Width::One,
            size: SIZES[n % sizes],
        },
        Plan::W4 => Config {
            requested_hz: REQUESTED_HZ[n / sizes],
            width: Width::Four,
            size: SIZES[n % sizes],
        },
        Plan::Full => {
            let q = SIZES.len();
            let w1 = (REQUESTED_HZ.len() - 1) * sizes;
            if n < q {
                Config {
                    requested_hz: REQUESTED_HZ[0],
                    width: Width::One,
                    size: SIZES[n],
                }
            } else if n < q + w1 {
                let m = n - q;
                Config {
                    requested_hz: REQUESTED_HZ[1 + m / sizes],
                    width: Width::One,
                    size: SIZES[m % sizes],
                }
            } else {
                let m = n - q - w1;
                Config {
                    requested_hz: REQUESTED_HZ[m / sizes],
                    width: Width::Four,
                    size: SIZES[m % sizes],
                }
            }
        }
    })
}

/// Direction of a measured phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// P4 to C6. The C6 validates every packet.
    HostToSlave = 1,
    /// C6 to P4. The P4 validates every packet.
    SlaveToHost = 2,
    /// P4 to C6, then the same length back from the C6. Both ends validate.
    RoundTrip = 3,
}

impl Mode {
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::HostToSlave),
            2 => Some(Self::SlaveToHost),
            3 => Some(Self::RoundTrip),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::HostToSlave => "p4_to_c6",
            Self::SlaveToHost => "c6_to_p4",
            Self::RoundTrip => "round_trip",
        }
    }
}

pub const MODES: [Mode; 3] = [Mode::HostToSlave, Mode::SlaveToHost, Mode::RoundTrip];

/// Salt of the packets sent by the P4 and of those sent by the C6. The phase identifier is mixed
/// in so that a packet of an earlier phase can never validate.
pub const fn salt(response: bool, phase: u32) -> u32 {
    let base = if response { 0xa3a3_a3a3 } else { 0x5151_5151 };
    base ^ phase.wrapping_mul(0x9e37_79b9)
}

/// Word `index` of the packet number `seq`. Word 0 carries the sequence number and word 1 the
/// length in bytes, so a truncated or shifted packet cannot match.
pub const fn word(seq: u32, salt: u32, index: usize, length: u32) -> u32 {
    match index {
        0 => seq,
        1 => length,
        _ => {
            salt ^ seq.wrapping_mul(0x9e37_79b1)
                ^ (index as u32).wrapping_mul(0x85eb_ca6b).rotate_left((index & 31) as u32)
        }
    }
}

/// Fills `buffer` (whose length is a multiple of 4, at least 8) with the packet `seq`.
pub fn fill(buffer: &mut [u8], seq: u32, salt: u32) {
    let length = buffer.len() as u32;
    for (index, chunk) in buffer.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        *chunk = word(seq, salt, index, length).to_le_bytes();
    }
}

/// Same packet as [`fill`], written as little-endian words (the target is little-endian), for
/// buffers the DMA API hands out as `&mut [u32]`. Only the first `length / 4` words are written.
pub fn fill_words(buffer: &mut [u32], length: u32, seq: u32, salt: u32) {
    for (index, slot) in buffer.iter_mut().take(length as usize / 4).enumerate() {
        *slot = u32::from_le(word(seq, salt, index, length));
    }
}

/// Checks the whole packet. Returns the index of the first differing byte.
pub fn verify(buffer: &[u8], seq: u32, salt: u32) -> Result<(), usize> {
    let length = buffer.len() as u32;
    for (index, chunk) in buffer.as_chunks::<4>().0.iter().enumerate() {
        let expected = word(seq, salt, index, length).to_le_bytes();
        if *chunk != expected {
            let offset = chunk.iter().zip(expected).position(|(a, b)| *a != b).unwrap_or(0);
            return Err(index * 4 + offset);
        }
    }
    if !buffer.len().is_multiple_of(4) {
        return Err(buffer.len() & !3);
    }
    Ok(())
}

pub const CONTROL_MAGIC: u32 = 0x4843_4e42; // "BNCH"
pub const SUMMARY_MAGIC: u32 = 0x4d55_5342; // "BSUM"
pub const CONTROL_LENGTH: usize = 64;

fn xor_checksum(words: &[u32]) -> u32 {
    words.iter().fold(0x1357_9bdf, |acc, w| acc.rotate_left(5) ^ w)
}

/// Phase description sent by the P4 in its own 64-byte packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    pub phase: u32,
    pub mode: Mode,
    pub length: u32,
    pub count: u32,
}

impl Control {
    pub fn encode(&self) -> [u8; CONTROL_LENGTH] {
        let mut words = [0u32; CONTROL_LENGTH / 4];
        words[0] = CONTROL_MAGIC;
        words[1] = self.phase;
        words[2] = self.mode as u32;
        words[3] = self.length;
        words[4] = self.count;
        words[15] = xor_checksum(&words[..15]);
        let mut bytes = [0u8; CONTROL_LENGTH];
        for (chunk, w) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *chunk = w.to_le_bytes();
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let words = words_of(bytes)?;
        if words[0] != CONTROL_MAGIC || words[15] != xor_checksum(&words[..15]) {
            return None;
        }
        let length = words[3];
        if length < 8 || !length.is_multiple_of(4) || words[4] == 0 {
            return None;
        }
        Some(Self {
            phase: words[1],
            mode: Mode::from_u32(words[2])?,
            length,
            count: words[4],
        })
    }
}

/// What the C6 observed during one phase, sent back in a 64-byte packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub phase: u32,
    pub packets_ok: u32,
    pub bytes_ok: u32,
    pub content_errors: u32,
    /// Index of the first wrong byte of the first bad packet, or `u32::MAX`.
    pub first_error_byte: u32,
    /// Sequence number of the first bad packet, or `u32::MAX`.
    pub first_error_seq: u32,
}

impl Summary {
    pub fn encode(&self) -> [u8; CONTROL_LENGTH] {
        let mut words = [0u32; CONTROL_LENGTH / 4];
        words[0] = SUMMARY_MAGIC;
        words[1] = self.phase;
        words[2] = self.packets_ok;
        words[3] = self.bytes_ok;
        words[4] = self.content_errors;
        words[5] = self.first_error_byte;
        words[6] = self.first_error_seq;
        words[15] = xor_checksum(&words[..15]);
        let mut bytes = [0u8; CONTROL_LENGTH];
        for (chunk, w) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            *chunk = w.to_le_bytes();
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let words = words_of(bytes)?;
        if words[0] != SUMMARY_MAGIC || words[15] != xor_checksum(&words[..15]) {
            return None;
        }
        Some(Self {
            phase: words[1],
            packets_ok: words[2],
            bytes_ok: words[3],
            content_errors: words[4],
            first_error_byte: words[5],
            first_error_seq: words[6],
        })
    }
}

fn words_of(bytes: &[u8]) -> Option<[u32; CONTROL_LENGTH / 4]> {
    if bytes.len() != CONTROL_LENGTH {
        return None;
    }
    let mut words = [0u32; CONTROL_LENGTH / 4];
    for (w, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*chunk);
    }
    Some(words)
}

/// Errors seen by the host during one measurement, by class. No error is ever retried.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Errors {
    pub content: u32,
    pub crc: u32,
    pub r5: u32,
    pub timeout: u32,
    pub other: u32,
}

impl Errors {
    pub const fn total(&self) -> u32 {
        self.content + self.crc + self.r5 + self.timeout + self.other
    }
}

/// Useful bytes per second. `micros` is the effective duration of the window.
pub const fn bytes_per_second(bytes: u64, micros: u64) -> u64 {
    match (bytes * 1_000_000).checked_div(micros) {
        Some(rate) => rate,
        None => 0,
    }
}

/// Packets that fill about `window_us`, given the time of one packet. At least one.
pub const fn packets_for(window_us: u64, packet_us: u64) -> u32 {
    if packet_us == 0 {
        return 1;
    }
    let n = window_us / packet_us;
    if n < 1 {
        1
    } else if n > u32::MAX as u64 {
        u32::MAX
    } else {
        n as u32
    }
}

/// Ladder rule: once a frequency fails for a width and a size, higher frequencies of the same
/// width and size are not attempted (they are reported as skipped, not as passed).
pub struct Ladder {
    failed: [(u8, u32, u32); 64],
    len: usize,
}

impl Default for Ladder {
    fn default() -> Self {
        Self::new()
    }
}

impl Ladder {
    pub const fn new() -> Self {
        Self {
            failed: [(0, 0, 0); 64],
            len: 0,
        }
    }

    pub fn record_failure(&mut self, config: Config) {
        if self.len < self.failed.len() {
            self.failed[self.len] = (config.width.bits() as u8, config.requested_hz, config.size);
            self.len += 1;
        }
    }

    pub fn skips(&self, config: Config) -> bool {
        self.failed[..self.len].iter().any(|(width, hz, size)| {
            *width == config.width.bits() as u8 && *size == config.size && *hz < config.requested_hz
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_names_and_timing() {
        assert_eq!(Plan::parse("smoke"), Some(Plan::Smoke));
        assert_eq!(Plan::parse("full"), Some(Plan::Full));
        assert_eq!(Plan::parse("w3"), None);
        assert_eq!(Plan::Smoke.timing(), (1, 1_000, 0));
        assert_eq!(Plan::W4.timing(), (3, 5_000, 30_000));
    }

    #[test]
    fn card_clock_is_the_one_the_divider_gives() {
        assert_eq!(card_clock_hz(400_000), 400_000);
        assert_eq!(card_clock_hz(5_000_000), 5_000_000);
        assert_eq!(card_clock_hz(10_000_000), 10_000_000);
        assert_eq!(card_clock_hz(20_000_000), 20_000_000);
        assert_eq!(card_clock_hz(40_000_000), 40_000_000);
        // 25 MHz is not module / (2 * n): the controller produces 20 MHz.
        assert_eq!(card_clock_hz(25_000_000), 20_000_000);
        assert_eq!(card_clock_hz(100_000_000), MODULE_HZ);
    }

    #[test]
    fn plans_enumerate_every_configuration_once_in_order() {
        for plan in [Plan::Smoke, Plan::Qualified, Plan::W1, Plan::W4, Plan::Full] {
            let count = config_count(plan);
            assert!(config(plan, count).is_none());
            let all: Vec<_> = (0..count).map(|n| config(plan, n).unwrap()).collect();
            for (i, a) in all.iter().enumerate() {
                assert!(!all[i + 1..].contains(a), "{plan:?} repeats {a:?}");
            }
            for pair in all.windows(2) {
                if pair[0].width == pair[1].width {
                    assert!(
                        (pair[0].requested_hz, pair[0].size) < (pair[1].requested_hz, pair[1].size)
                            || plan == Plan::Full,
                        "{plan:?} is not ascending"
                    );
                }
            }
        }
        assert_eq!(config_count(Plan::Full), 4 + 16 + 20);
        let first = config(Plan::Qualified, 0).unwrap();
        assert_eq!((first.requested_hz, first.width, first.size), (400_000, Width::One, 64));
        let w1_first = config(Plan::W1, 0).unwrap();
        assert_eq!(w1_first.requested_hz, 5_000_000);
        let w4_first = config(Plan::W4, 0).unwrap();
        assert_eq!((w4_first.requested_hz, w4_first.width), (400_000, Width::Four));
    }

    #[test]
    fn sizes_are_cache_line_multiples() {
        for size in SIZES {
            assert_eq!(size % 64, 0);
            if size > BYTE_MODE_MAX {
                assert_eq!(size % BLOCK_SIZE, 0, "block mode needs whole blocks");
            }
        }
    }

    #[test]
    fn pattern_round_trips_and_detects_any_single_byte_change() {
        for size in SIZES {
            let mut buffer = vec![0u8; size as usize];
            fill(&mut buffer, 17, salt(false, 3));
            assert_eq!(verify(&buffer, 17, salt(false, 3)), Ok(()));
            for index in [0usize, 7, 8, size as usize / 2, size as usize - 1] {
                let mut bad = buffer.clone();
                bad[index] ^= 0x01;
                assert_eq!(verify(&bad, 17, salt(false, 3)), Err(index), "size {size} byte {index}");
            }
        }
    }

    #[test]
    fn word_fill_equals_byte_fill() {
        for size in SIZES {
            let mut bytes = vec![0u8; size as usize];
            fill(&mut bytes, 123, salt(true, 7));
            let mut words = vec![0u32; size as usize / 4 + 3];
            fill_words(&mut words, size, 123, salt(true, 7));
            let as_bytes: Vec<u8> = words[..size as usize / 4].iter().flat_map(|w| w.to_le_bytes()).collect();
            assert_eq!(bytes, as_bytes);
            assert_eq!(words[size as usize / 4..], [0, 0, 0], "nothing is written past the length");
        }
    }

    #[test]
    fn pattern_rejects_an_old_packet_a_shifted_packet_and_another_phase() {
        let mut a = vec![0u8; 256];
        fill(&mut a, 5, salt(false, 1));
        assert!(verify(&a, 6, salt(false, 1)).is_err(), "previous sequence number");
        assert!(verify(&a, 5, salt(false, 2)).is_err(), "other phase");
        assert!(verify(&a, 5, salt(true, 1)).is_err(), "other direction");
        let mut shifted = a.clone();
        shifted.rotate_left(4);
        assert!(verify(&shifted, 5, salt(false, 1)).is_err());
        assert!(verify(&a[..252], 5, salt(false, 1)).is_err(), "truncated length");
    }

    #[test]
    fn control_and_summary_round_trip_and_reject_corruption() {
        let control = Control {
            phase: 9,
            mode: Mode::RoundTrip,
            length: 1024,
            count: 321,
        };
        let bytes = control.encode();
        assert_eq!(Control::decode(&bytes), Some(control));
        let mut bad = bytes;
        bad[17] ^= 0x10;
        assert_eq!(Control::decode(&bad), None);
        assert_eq!(Control::decode(&bytes[..63]), None);
        let summary = Summary {
            phase: 9,
            packets_ok: 321,
            bytes_ok: 328_704,
            content_errors: 0,
            first_error_byte: u32::MAX,
            first_error_seq: u32::MAX,
        };
        assert_eq!(Summary::decode(&summary.encode()), Some(summary));
        assert_eq!(Summary::decode(&control.encode()), None, "a control is not a summary");
    }

    #[test]
    fn control_refuses_nonsense_fields() {
        let mut c = Control {
            phase: 1,
            mode: Mode::HostToSlave,
            length: 64,
            count: 1,
        };
        c.count = 0;
        assert_eq!(Control::decode(&c.encode()), None);
        c.count = 1;
        c.length = 66;
        assert_eq!(Control::decode(&c.encode()), None);
    }

    #[test]
    fn goodput_arithmetic() {
        assert_eq!(bytes_per_second(1_000_000, 1_000_000), 1_000_000);
        assert_eq!(bytes_per_second(64 * 1000, 500_000), 128_000);
        assert_eq!(bytes_per_second(5, 0), 0);
        assert_eq!(packets_for(5_000_000, 1_000), 5_000);
        assert_eq!(packets_for(5_000_000, 9_000_000), 1);
        assert_eq!(packets_for(5_000_000, 0), 1);
    }

    #[test]
    fn ladder_skips_only_higher_frequencies_of_the_same_width_and_size() {
        let mut ladder = Ladder::new();
        let failing = Config {
            requested_hz: 10_000_000,
            width: Width::Four,
            size: 1024,
        };
        assert!(!ladder.skips(failing));
        ladder.record_failure(failing);
        let higher = Config {
            requested_hz: 20_000_000,
            ..failing
        };
        assert!(ladder.skips(higher));
        assert!(!ladder.skips(Config { size: 256, ..higher }), "another size");
        assert!(!ladder.skips(Config { width: Width::One, ..higher }), "another width");
        assert!(!ladder.skips(Config { requested_hz: 5_000_000, ..failing }), "lower frequency");
        assert!(!ladder.skips(failing), "the failed configuration itself is not 'skipped'");
    }

    #[test]
    fn errors_total() {
        let e = Errors {
            content: 1,
            crc: 2,
            r5: 3,
            timeout: 4,
            other: 5,
        };
        assert_eq!(e.total(), 15);
        assert_eq!(Errors::default().total(), 0);
    }
}
