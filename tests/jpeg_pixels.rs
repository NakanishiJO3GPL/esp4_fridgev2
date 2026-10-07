// Run on the development host without ESP-IDF:
// rustc --edition=2021 --test tests/jpeg_pixels.rs -o target/jpeg_pixels_tests
// ./target/jpeg_pixels_tests
#[path = "../src/jpeg_pixels.rs"]
mod jpeg_pixels;

use jpeg_pixels::{repack_row, Packing, PROBE_ORDER};

#[test]
fn native_uyvy_has_priority_in_equal_score_probes() {
    assert_eq!(
        PROBE_ORDER,
        [Packing::Uyvy, Packing::Yuyv, Packing::Yvyu, Packing::Vyuy]
    );
}

#[test]
fn neutral_black_and_white_are_not_range_expanded() {
    let mut row = [128, 16, 128, 235, 128, 0, 128, 255];
    repack_row(&mut row, Packing::Yvyu);
    assert_eq!(row, [16, 128, 235, 128, 0, 128, 255, 128]);
}

#[test]
fn unequal_luma_and_chroma_have_requested_yvyu_order() {
    let mut row = [160, 64, 192, 160];
    repack_row(&mut row, Packing::Yvyu);
    assert_eq!(row, [64, 192, 160, 160]);
}

#[test]
fn every_sample_value_is_preserved_in_all_packings() {
    for value in 0..=255u8 {
        let [u, y0, v, y1] = [value, value.wrapping_add(37), 255 - value, value ^ 0x55];
        for (packing, expected) in [
            (Packing::Yuyv, [y0, u, y1, v]),
            (Packing::Uyvy, [u, y0, v, y1]),
            (Packing::Yvyu, [y0, v, y1, u]),
            (Packing::Vyuy, [v, y0, u, y1]),
        ] {
            let mut row = [u, y0, v, y1];
            repack_row(&mut row, packing);
            assert_eq!(row, expected, "packing={packing:?}, value={value}");
        }
    }
}

#[test]
fn multiple_pairs_are_repacked_in_place_without_cross_pair_changes() {
    let mut row = [128, 16, 128, 235, 160, 64, 192, 160];
    let address = row.as_ptr();
    repack_row(&mut row, Packing::Yvyu);
    assert_eq!(row.as_ptr(), address);
    assert_eq!(row, [16, 128, 235, 128, 64, 192, 160, 160]);
}

#[test]
fn selectable_packing_keeps_semantic_luma_and_chroma() {
    for (packing, name, expected) in [
        (Packing::Yuyv, "YUYV", [64, 160, 160, 192]),
        (Packing::Uyvy, "UYVY", [160, 64, 192, 160]),
        (Packing::Yvyu, "YVYU", [64, 192, 160, 160]),
        (Packing::Vyuy, "VYUY", [192, 64, 160, 160]),
    ] {
        let mut row = [160, 64, 192, 160];
        repack_row(&mut row, packing);
        assert_eq!(row, expected);
        assert_eq!(packing.name(), name);
    }
}

#[test]
fn raw_packing_does_not_apply_camera_color_conversion() {
    let semantic = [32, 64, 192, 224];
    assert_eq!(Packing::Yuyv.pack(semantic), [32, 64, 192, 224]);
    assert_eq!(Packing::Uyvy.pack(semantic), [64, 32, 224, 192]);
    assert_eq!(Packing::Yvyu.pack(semantic), [32, 224, 192, 64]);
    assert_eq!(Packing::Vyuy.pack(semantic), [224, 32, 64, 192]);
}

#[test]
fn empty_row_is_valid() {
    for packing in [Packing::Yuyv, Packing::Uyvy, Packing::Yvyu, Packing::Vyuy] {
        repack_row(&mut [], packing);
    }
}

#[test]
fn rejects_incomplete_uyvy_pairs_before_mutation() {
    for packing in [Packing::Yuyv, Packing::Uyvy, Packing::Yvyu, Packing::Vyuy] {
        for length in [1, 2, 3, 5, 6, 7] {
            let mut row = [99; 8];
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                repack_row(&mut row[..length], packing);
            }));
            assert!(result.is_err(), "accepted row length {length}");
            assert_eq!(row, [99; 8], "modified output before validating length");
        }
    }
}
