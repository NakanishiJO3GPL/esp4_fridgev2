//! In-place UYVY byte repacking; ISP BT.709 full-range sample values are unchanged.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Packing {
    Yuyv,
    Uyvy,
    Yvyu,
    Vyuy,
}

// With strict score improvement, native UYVY wins equal-score probe results.
pub(super) const PROBE_ORDER: [Packing; 4] =
    [Packing::Uyvy, Packing::Yuyv, Packing::Yvyu, Packing::Vyuy];

impl Packing {
    /// Input is semantic [Y0, Cb, Y1, Cr], not a hardware byte layout.
    pub(super) fn pack(self, [y0, cb, y1, cr]: [u8; 4]) -> [u8; 4] {
        match self {
            Self::Yuyv => [y0, cb, y1, cr],
            Self::Uyvy => [cb, y0, cr, y1],
            Self::Yvyu => [y0, cr, y1, cb],
            Self::Vyuy => [cr, y0, cb, y1],
        }
    }

    #[cfg(test)]
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Yuyv => "YUYV",
            Self::Uyvy => "UYVY",
            Self::Yvyu => "YVYU",
            Self::Vyuy => "VYUY",
        }
    }
}

/// Repack one row in place, using only per-pair scalar temporaries.
///
/// Input bytes: [U, Y0, V, Y1]. Output packing is selected by the hardware
/// coefficient self-test, not the ambiguous ESP-IDF YUV422 format enum name.
///
/// No range expansion or color matrix is applied. JPEG retains BT.709 samples;
/// conventional BT.601 JPEG decoders may therefore render different colors.
///
/// # Panics
/// Panics unless the row contains complete four-byte UYVY pairs.
pub(super) fn repack_row(row: &mut [u8], packing: Packing) {
    assert_eq!(row.len() % 4, 0, "row must contain complete UYVY pairs");
    if packing == Packing::Uyvy {
        return;
    }
    for pair in row.chunks_exact_mut(4) {
        let [u, y0, v, y1] = [pair[0], pair[1], pair[2], pair[3]];
        pair.copy_from_slice(&packing.pack([y0, u, y1, v]));
    }
}
