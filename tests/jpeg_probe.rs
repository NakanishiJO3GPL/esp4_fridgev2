// Standalone host tests, without ESP-IDF or a JPEG decoder:
// rustc --edition=2021 --test tests/jpeg_probe.rs -o target/jpeg_probe_tests
// ./target/jpeg_probe_tests
#[path = "../src/jpeg/probe.rs"]
mod jpeg_probe;

use jpeg_probe::{pair, score, HEIGHT, WIDTH};
use std::ops::Range;

// Independent semantic reference, not obtained from the production pair().
const TILES: [[i32; 4]; 4] = [
    [80, 96, 152, 160],
    [104, 160, 176, 96],
    [88, 96, 160, 96],
    [72, 160, 144, 160],
];
const DC_IDS: [usize; 3] = [3, 1, 2];
const AC_IDS: [usize; 3] = [2, 3, 1];
const Q_IDS: [usize; 3] = [2, 0, 3];

struct Options {
    swap_chroma: bool,
    reverse_luma: bool,
    omit_ac1: bool,
    quant: [u16; 3],
    wide_quant: bool,
    dc_bias: i32,
    dc_length: u8,
    ac_length: u8,
    scan_order: [usize; 3],
    // Additional luma AC symbols, after AC1; used to exercise stuffing/runs.
    tail: Vec<(u8, i32)>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            swap_chroma: false,
            reverse_luma: false,
            omit_ac1: false,
            quant: [1; 3],
            wide_quant: false,
            dc_bias: 0,
            dc_length: 4,
            ac_length: 3,
            scan_order: [0, 1, 2],
            tail: Vec::new(),
        }
    }
}

struct Fixture {
    jpeg: Vec<u8>,
    entropy: Range<usize>,
    padding: u8,
}

// These are deliberately different for every ID and for DC versus AC.
fn dc_symbols(id: usize) -> Vec<u8> {
    let mut symbols: Vec<u8> = (0..12).collect();
    symbols.rotate_left(id * 2);
    symbols
}

fn ac_symbols(id: usize) -> Vec<u8> {
    let mut symbols = vec![0, 1, 0xf0, 0xd1, 0xf1, 2, 0x0a];
    symbols.rotate_left(id);
    symbols
}

fn segment(jpeg: &mut Vec<u8>, marker: u8, payload: &[u8]) {
    jpeg.extend_from_slice(&[0xff, marker]);
    jpeg.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    jpeg.extend_from_slice(payload);
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    byte: u8,
    used: u8,
}

impl Writer {
    fn bits(&mut self, value: u32, count: u8) {
        for shift in (0..count).rev() {
            self.byte = (self.byte << 1) | ((value >> shift) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.bytes.push(self.byte);
                if self.byte == 0xff {
                    self.bytes.push(0);
                }
                self.byte = 0;
                self.used = 0;
            }
        }
    }

    fn amplitude(&mut self, value: i32, count: u8) {
        let encoded = if value < 0 {
            value + (1 << count) - 1
        } else {
            value
        };
        self.bits(encoded as u32, count);
    }

    fn dc(&mut self, id: usize, difference: i32, length: u8) {
        let size = (32 - difference.unsigned_abs().leading_zeros()) as u8;
        let code = dc_symbols(id)
            .iter()
            .position(|&symbol| symbol == size)
            .unwrap();
        self.bits(code as u32, length);
        self.amplitude(difference, size);
    }

    fn ac(&mut self, id: usize, symbol: u8, value: i32, length: u8) {
        let code = ac_symbols(id)
            .iter()
            .position(|&candidate| candidate == symbol)
            .unwrap();
        self.bits(code as u32, length);
        self.amplitude(value, symbol & 15);
    }

    fn finish(mut self) -> (Vec<u8>, u8) {
        let padding = (8 - self.used) % 8;
        self.bits((1 << padding) - 1, padding);
        (self.bytes, padding)
    }
}

fn build(options: Options) -> Fixture {
    let mut jpeg = vec![0xff, 0xd8];
    let mut dqt = Vec::new();
    for id in 0..4 {
        let q = Q_IDS
            .iter()
            .position(|&candidate| candidate == id)
            .map(|component| options.quant[component])
            .unwrap_or(1);
        dqt.push(if options.wide_quant { 0x10 } else { 0 } | id as u8);
        for _ in 0..64 {
            if options.wide_quant {
                dqt.extend_from_slice(&q.to_be_bytes());
            } else {
                dqt.push(u8::try_from(q).unwrap());
            }
        }
    }
    segment(&mut jpeg, 0xdb, &dqt);
    let mut dht = Vec::new();
    for id in 0..4 {
        dht.push(id as u8);
        let mut counts = [0; 16];
        counts[usize::from(options.dc_length - 1)] = 12;
        dht.extend_from_slice(&counts);
        dht.extend_from_slice(&dc_symbols(id));
        dht.push(0x10 | id as u8);
        counts = [0; 16];
        counts[usize::from(options.ac_length - 1)] = 7;
        dht.extend_from_slice(&counts);
        dht.extend_from_slice(&ac_symbols(id));
    }
    segment(&mut jpeg, 0xc4, &dht);
    // Frame components intentionally have a different order from the scan.
    let mut sof = vec![8, 0, 16, 0, 32, 3];
    for component in [2, 0, 1] {
        sof.extend_from_slice(&[
            component as u8 + 1,
            if component == 0 { 0x21 } else { 0x11 },
            Q_IDS[component] as u8,
        ]);
    }
    segment(&mut jpeg, 0xc0, &sof);
    let mut sos = vec![3];
    for component in options.scan_order {
        sos.extend_from_slice(&[
            component as u8 + 1,
            ((DC_IDS[component] << 4) | AC_IDS[component]) as u8,
        ]);
    }
    sos.extend_from_slice(&[0, 63, 0]);
    segment(&mut jpeg, 0xda, &sos);
    let mut writer = Writer::default();
    let mut predictors = [0; 3];
    for [y0, cb, y1, cr] in TILES {
        let averages = if options.swap_chroma {
            [(y0 + y1) / 2, cr, cb]
        } else {
            [(y0 + y1) / 2, cb, cr]
        };
        for component in options.scan_order {
            for _ in 0..if component == 0 { 2 } else { 1 } {
                let dc = (averages[component] - 128) * 8 / i32::from(options.quant[component])
                    + options.dc_bias;
                writer.dc(
                    DC_IDS[component],
                    dc - predictors[component],
                    options.dc_length,
                );
                predictors[component] = dc;
                let mut next = 1;
                if component == 0 && !options.omit_ac1 {
                    writer.ac(
                        AC_IDS[component],
                        1,
                        if options.reverse_luma { 1 } else { -1 },
                        options.ac_length,
                    );
                    next += 1;
                }
                if component == 0 {
                    for &(symbol, value) in &options.tail {
                        writer.ac(AC_IDS[component], symbol, value, options.ac_length);
                        next += if symbol == 0xf0 {
                            16
                        } else {
                            usize::from(symbol >> 4) + 1
                        };
                    }
                }
                if next < 64 {
                    writer.ac(AC_IDS[component], 0, 0, options.ac_length);
                }
            }
        }
    }
    let (entropy, padding) = writer.finish();
    let start = jpeg.len();
    jpeg.extend_from_slice(&entropy);
    let end = jpeg.len();
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    Fixture {
        jpeg,
        entropy: start..end,
        padding,
    }
}

fn payload(jpeg: &[u8], wanted: u8) -> Range<usize> {
    let mut offset = 2;
    loop {
        assert_eq!(jpeg[offset], 0xff);
        let length = u16::from_be_bytes([jpeg[offset + 2], jpeg[offset + 3]]) as usize;
        let range = offset + 4..offset + 2 + length;
        if jpeg[offset + 1] == wanted {
            return range;
        }
        offset = range.end;
    }
}

#[test]
fn semantic_pattern_and_dimensions() {
    assert_eq!((WIDTH, HEIGHT), (32, 16));
    for y in 0..16 {
        for x in 0..32 {
            let expected = TILES[(y / 8) * 2 + x / 16].map(|value| value as u8);
            assert_eq!(pair(x, y), expected);
            assert!(pair(x, y)[0] < pair(x, y)[2]);
        }
    }
}

#[test]
fn correct_signed_dc_and_negative_ac_score_zero() {
    assert_eq!(score(&build(Options::default()).jpeg), Ok(0));
}

#[test]
fn swapped_chroma_and_reversed_luma_are_distinguished() {
    let swapped = build(Options {
        swap_chroma: true,
        ..Options::default()
    });
    assert_eq!(score(&swapped.jpeg), Ok(16));
    let reversed = build(Options {
        reverse_luma: true,
        ..Options::default()
    });
    assert_eq!(score(&reversed.jpeg), Ok(32));
    let both = build(Options {
        swap_chroma: true,
        reverse_luma: true,
        ..Options::default()
    });
    assert_eq!(score(&both.jpeg), Ok(48));
    let absent = build(Options {
        omit_ac1: true,
        ..Options::default()
    });
    assert_eq!(score(&absent.jpeg), Ok(32));
}

#[test]
fn quantization_and_scan_selectors_come_from_headers() {
    for order in [[0, 1, 2], [2, 0, 1], [1, 2, 0]] {
        for wide_quant in [false, true] {
            let fixture = build(Options {
                quant: [2, 4, 8],
                wide_quant,
                scan_order: order,
                ..Options::default()
            });
            assert_eq!(score(&fixture.jpeg), Ok(0));
        }
    }
    // Exercise 16-bit values beyond the 8-bit table range.
    let fixture = build(Options {
        quant: [256, 512, 1024],
        wide_quant: true,
        ..Options::default()
    });
    assert_eq!(score(&fixture.jpeg), Ok(22));
}

#[test]
fn sixteen_bit_huffman_codes_and_dc_predictor_bounds() {
    let long = build(Options {
        dc_length: 16,
        ac_length: 16,
        ..Options::default()
    });
    assert_eq!(score(&long.jpeg), Ok(0));
    for bias in [767_i32, -768] {
        let fixture = build(Options {
            dc_bias: bias,
            ..Options::default()
        });
        assert_eq!(score(&fixture.jpeg), Ok(bias.unsigned_abs() / 8));
    }
    for bias in [768, -769] {
        let fixture = build(Options {
            dc_bias: bias,
            ..Options::default()
        });
        assert_eq!(
            score(&fixture.jpeg),
            Err("baseline DC predictor out of range")
        );
    }
    let large = build(Options {
        quant: [u16::MAX; 3],
        wide_quant: true,
        dc_bias: 1023,
        ..Options::default()
    });
    assert!(score(&large.jpeg).unwrap() > 1_000_000);
}

#[test]
fn fractional_dc_error_is_averaged_before_integer_rounding() {
    let fixture = build(Options {
        quant: [3; 3],
        dc_bias: 1,
        ..Options::default()
    });
    let mut error = 0.0_f64;
    for [y0, cb, y1, cr] in TILES {
        for (expected, count) in [((y0 + y1) / 2, 2), (cb, 1), (cr, 1)] {
            let dc = (expected - 128) * 8 / 3 + 1;
            error +=
                (128.0 + f64::from(dc) * 3.0 / 8.0 - f64::from(expected)).abs() * f64::from(count);
        }
    }
    assert_eq!(score(&fixture.jpeg), Ok((error / 16.0) as u32));
    let shifted = build(Options {
        dc_bias: 8,
        ..Options::default()
    });
    assert_eq!(score(&shifted.jpeg), Ok(1));
}

#[test]
fn entropy_ff_is_unstuffed_and_later_ac_is_ignored() {
    let fixture = build(Options {
        tail: vec![(0x0a, 1023)],
        ..Options::default()
    });
    assert!(fixture.jpeg[fixture.entropy.clone()]
        .windows(2)
        .any(|bytes| bytes == [0xff, 0]));
    assert_eq!(score(&fixture.jpeg), Ok(0));
    let mut broken = fixture.jpeg;
    let stuffed = fixture.entropy.start
        + broken[fixture.entropy.clone()]
            .windows(2)
            .position(|bytes| bytes == [0xff, 0])
            .unwrap();
    broken.remove(stuffed + 1);
    assert!(score(&broken).is_err());
}

#[test]
fn valid_zero_runs_and_last_coefficient_without_eob() {
    let fixture = build(Options {
        tail: vec![(0xf0, 0), (0xf0, 0), (0xf0, 0), (0xd1, -1)],
        ..Options::default()
    });
    assert_eq!(score(&fixture.jpeg), Ok(0));
}

#[test]
fn ac_runs_cannot_overrun_a_block() {
    for tail in [vec![(0xf0, 0); 4], vec![(0xf1, 1); 4]] {
        assert!(score(
            &build(Options {
                tail,
                ..Options::default()
            })
            .jpeg
        )
        .is_err());
    }
}

#[test]
fn every_truncated_prefix_is_rejected_without_panicking() {
    let fixture = build(Options {
        tail: vec![(0x0a, 1023)],
        ..Options::default()
    });
    for end in 0..fixture.jpeg.len() {
        assert!(
            score(&fixture.jpeg[..end]).is_err(),
            "accepted prefix ending at {end}"
        );
    }
}

#[test]
fn entropy_codes_markers_padding_and_eoi_are_checked() {
    let fixture = build(Options::default());
    let mut bad_code = fixture.jpeg.clone();
    bad_code.splice(fixture.entropy.clone(), [0xff, 0, 0xff, 0]);
    assert_eq!(score(&bad_code), Err("invalid Huffman code"));
    for marker in [0xd0, 0xd9, 0xda] {
        let mut early_marker = fixture.jpeg.clone();
        early_marker.splice(fixture.entropy.start..fixture.entropy.start, [0xff, marker]);
        assert!(score(&early_marker).is_err());
    }
    let padded = build(Options {
        tail: vec![(2, 3)],
        ..Options::default()
    });
    assert!(padded.padding > 0);
    let mut bad_padding = padded.jpeg;
    bad_padding[padded.entropy.end - 1] &= !1;
    assert_eq!(score(&bad_padding), Err("invalid entropy padding"));
    let mut extra_entropy = fixture.jpeg.clone();
    extra_entropy.insert(fixture.entropy.end, 0);
    assert!(score(&extra_entropy).is_err());
    let mut missing_eoi = fixture.jpeg.clone();
    *missing_eoi.last_mut().unwrap() = 0xda;
    assert!(score(&missing_eoi).is_err());
    let mut trailing = fixture.jpeg;
    trailing.extend_from_slice(&[42, 0xff, 0]);
    assert_eq!(score(&trailing), Ok(0));
}

#[test]
fn rejects_wrong_frame_dimensions_components_sampling_and_progressive() {
    let jpeg = build(Options::default()).jpeg;
    let sof = payload(&jpeg, 0xc0);
    for (offset, value) in [
        (0, 12),
        (2, 8),
        (4, 16),
        (5, 2),
        (6, 4),
        (9, 3),
        (10, 0x22),
        (11, 4),
    ] {
        let mut invalid = jpeg.clone();
        invalid[sof.start + offset] = value;
        assert!(
            score(&invalid).is_err(),
            "accepted SOF mutation {offset}: {value}"
        );
    }
    for marker in [0xc1, 0xc2, 0xc3, 0xc9] {
        let mut invalid = jpeg.clone();
        invalid[sof.start - 3] = marker;
        assert!(score(&invalid).is_err());
    }
    let mut duplicate = jpeg.clone();
    duplicate.splice(
        sof.end..sof.end,
        jpeg[sof.start - 4..sof.end].iter().copied(),
    );
    assert!(score(&duplicate).is_err());
    let mut absent = jpeg.clone();
    absent.drain(sof.start - 4..sof.end);
    assert!(score(&absent).is_err());
}

#[test]
fn rejects_invalid_scan_headers_and_missing_tables() {
    let jpeg = build(Options::default()).jpeg;
    let sos = payload(&jpeg, 0xda);
    for (offset, value) in [
        (0, 1),
        (1, 0),
        (3, 1),
        (2, 0x40),
        (2, 0x34),
        (7, 1),
        (8, 62),
        (9, 1),
    ] {
        let mut invalid = jpeg.clone();
        invalid[sos.start + offset] = value;
        assert!(
            score(&invalid).is_err(),
            "accepted SOS mutation {offset}: {value}"
        );
    }
    for marker in [0xdb, 0xc4] {
        let mut missing = jpeg.clone();
        let range = payload(&missing, marker);
        missing.drain(range.start - 4..range.end);
        assert_eq!(score(&missing), Err("missing JPEG table"));
    }
}

#[test]
fn malformed_quantization_tables_are_rejected() {
    let jpeg = build(Options::default()).jpeg;
    let dqt = payload(&jpeg, 0xdb);
    for (offset, value) in [(0, 0x20), (0, 4), (1, 0), (64, 0)] {
        let mut invalid = jpeg.clone();
        invalid[dqt.start + offset] = value;
        assert!(score(&invalid).is_err());
    }
    let mut wide = build(Options {
        wide_quant: true,
        ..Options::default()
    })
    .jpeg;
    let range = payload(&wide, 0xdb);
    wide[range.start + 1..range.start + 3].fill(0);
    assert!(score(&wide).is_err());
}

#[test]
fn malformed_huffman_tables_are_rejected() {
    let jpeg = build(Options::default()).jpeg;
    let dht = payload(&jpeg, 0xc4);
    // First DC record is 29 bytes, followed by an AC record.
    for (offset, value) in [
        (0, 0x20),
        (0, 4),
        (1, 2),
        (1, 1),
        (17, 12),
        (18, 0),
        (29 + 17, 0x0b),
        (29 + 17, 0x10),
    ] {
        let mut invalid = jpeg.clone();
        invalid[dht.start + offset] = value;
        assert!(
            score(&invalid).is_err(),
            "accepted DHT mutation {offset}: {value}"
        );
    }
    let mut empty = jpeg.clone();
    empty[dht.start + 1..dht.start + 17].fill(0);
    assert!(score(&empty).is_err());
    let mut too_many = jpeg;
    too_many[dht.start + 1..dht.start + 17].fill(0);
    too_many[dht.start + 15] = 2;
    too_many[dht.start + 16] = 255;
    assert!(score(&too_many).is_err());
}

#[test]
fn marker_lengths_fill_bytes_metadata_and_restart_policy() {
    let jpeg = build(Options::default()).jpeg;
    for length in [0_u16, 1, u16::MAX] {
        let mut invalid = jpeg.clone();
        invalid[4..6].copy_from_slice(&length.to_be_bytes());
        assert!(score(&invalid).is_err());
    }
    for marker in [0xd8, 0xd9, 0xd0, 0x01, 0x00] {
        let mut invalid = jpeg.clone();
        invalid[3] = marker;
        assert!(score(&invalid).is_err());
    }
    let mut missing_soi = jpeg.clone();
    missing_soi[1] = 0;
    assert!(score(&missing_soi).is_err());
    let mut missing_prefix = jpeg.clone();
    missing_prefix[2] = 0;
    assert!(score(&missing_prefix).is_err());
    let mut filled = jpeg.clone();
    filled.insert(2, 0xff);
    assert_eq!(score(&filled), Ok(0));
    let mut metadata = vec![0xff, 0xd8];
    segment(&mut metadata, 0xe1, &[0xff, 0, 0xff, 0xd9]);
    segment(&mut metadata, 0xfe, b"coefficient probe");
    metadata.extend_from_slice(&jpeg[2..]);
    assert_eq!(score(&metadata), Ok(0));
    for interval in [vec![0, 0], vec![0, 1], vec![0], vec![0, 0, 0]] {
        let mut with_dri = vec![0xff, 0xd8];
        segment(&mut with_dri, 0xdd, &interval);
        with_dri.extend_from_slice(&jpeg[2..]);
        if interval == [0, 0] {
            assert_eq!(score(&with_dri), Ok(0));
        } else {
            assert!(score(&with_dri).is_err());
        }
    }
}

#[test]
fn malformed_byte_mutations_do_not_panic() {
    let jpeg = build(Options::default()).jpeg;
    for position in 0..jpeg.len() {
        for value in [0, 0xff, 0x80] {
            let mut mutated = jpeg.clone();
            mutated[position] = value;
            let _ = score(&mutated);
        }
    }
}
