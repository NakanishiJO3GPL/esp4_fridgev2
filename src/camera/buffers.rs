//! Two-buffer ownership and UYVY region luminance, independent of ESP-IDF.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

/// Rectangle in pixels, with (0, 0) at the frame's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LuminanceError {
    /// Invalid UYVY dimensions/stride, arithmetic overflow or truncated data.
    InvalidFrame,
    /// Empty, out-of-bounds or excessively large rectangle (zero-based index).
    InvalidRegion(usize),
}

impl core::fmt::Display for LuminanceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidFrame => write!(f, "invalid UYVY frame layout or data length"),
            Self::InvalidRegion(index) => write!(f, "invalid luminance region {index}"),
        }
    }
}

impl std::error::Error for LuminanceError {}

/// Calculate mean Y for exactly three rectangles in packed UYVY data.
///
/// Each pixel's Y is at byte `2 * x + 1`; chroma and row padding are ignored.
/// Regions may overlap and start at odd x coordinates. Results retain the raw
/// 8-bit Y scale (0..=255), without range normalization or color conversion.
/// No allocation or frame copy is performed. Call on a borrowed completed frame,
/// never from the ISR or on memory currently owned by DMA.
pub fn average_luminance(
    data: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    regions: &[Region; 3],
) -> Result<[f32; 3], LuminanceError> {
    let row_bytes = width.checked_mul(2).ok_or(LuminanceError::InvalidFrame)?;
    if width == 0 || width % 2 != 0 || height == 0 || stride < row_bytes {
        return Err(LuminanceError::InvalidFrame);
    }
    let required = (height - 1)
        .checked_mul(stride)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or(LuminanceError::InvalidFrame)?;
    if data.len() < required {
        return Err(LuminanceError::InvalidFrame);
    }

    let mut pixel_counts = [0u64; 3];
    for (index, region) in regions.iter().enumerate() {
        if region.width == 0
            || region.height == 0
            || region.x > width
            || region.y > height
            || region.width > width - region.x
            || region.height > height - region.y
        {
            pixel_counts[index] = 0;
        }
        else {
            pixel_counts[index] = region
                .width
                .checked_mul(region.height)
                .and_then(|count| u64::try_from(count).ok())
                .filter(|count| *count <= u64::MAX / 255)
                .ok_or(LuminanceError::InvalidRegion(index))?;
        }
    }

    let mut means = [0.0; 3];
    for (index, region) in regions.iter().enumerate() {
        if pixel_counts[index] != 0 {
            let mut sum = 0u64;
            for y in region.y..region.y + region.height {
                let start = y * stride + region.x * 2;
                let row = &data[start..start + region.width * 2];
                for pixel in row.chunks_exact(2) {
                    sum += u64::from(pixel[1]);
                }
            }
            means[index] = sum as f32 / pixel_counts[index] as f32;
        }
        else {
            means[index] = 0.0;
        }
    }
    Ok(means)
}

const FREE: u32 = 0;
const DMA: u32 = 1;
const READY: u32 = 2;
const HELD: u32 = 3;
const NONE: usize = 2;

pub(super) struct Buffers {
    states: [AtomicU32; 2],
    sequences: [AtomicU32; 2],
    active: AtomicUsize,
    completed: AtomicU32,
    dropped: AtomicU32,
}

impl Buffers {
    pub(super) fn new() -> Self {
        Self {
            states: [AtomicU32::new(FREE), AtomicU32::new(FREE)],
            sequences: [AtomicU32::new(0), AtomicU32::new(0)],
            active: AtomicUsize::new(NONE),
            completed: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        }
    }

    /// IDF asks for the next buffer BEFORE publishing the previous completion.
    /// Only the serialized CSI callback/start path may call this method.
    pub(super) fn next_dma(&self) -> usize {
        let current = self.active.load(Ordering::Relaxed);
        let candidate = if current == NONE { 0 } else { 1 - current };
        let state = self.states[candidate].load(Ordering::Acquire);
        if (state == FREE || state == READY)
            && self.states[candidate]
                .compare_exchange(state, DMA, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            if state == READY {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.active.store(candidate, Ordering::Relaxed);
            candidate
        } else {
            // The other buffer is borrowed or leased. Reuse the current DMA buffer and
            // never expose its just-completed contents to the CPU.
            current
        }
    }

    /// Returns true only after publishing a frame that a waiting task can acquire.
    pub(super) fn finish(&self, index: usize, valid: bool) -> bool {
        let sequence = self
            .completed
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        if index == self.active.load(Ordering::Relaxed) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if valid {
            self.sequences[index].store(sequence, Ordering::Relaxed);
            self.states[index].store(READY, Ordering::Release);
            true
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.states[index].store(FREE, Ordering::Release);
            false
        }
    }

    pub(super) fn acquire(&self) -> Option<(usize, u32)> {
        for index in 0..2 {
            if self.states[index]
                .compare_exchange(READY, HELD, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some((index, self.sequences[index].load(Ordering::Relaxed)));
            }
        }
        None
    }

    pub(super) fn release(&self, index: usize) {
        self.states[index].store(FREE, Ordering::Release);
    }

    pub(super) fn counts(&self) -> (u32, u32) {
        (
            self.completed.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// Task-side pool interface. Implementations keep these atomic states and their
/// frame allocations alive until the last pool reference is dropped.
pub(super) trait LeasePool {
    fn buffers(&self) -> &Buffers;
}

/// One guard per READY -> HELD transition. Clone the Arc, not the guard: only
/// its final drop may return the buffer to DMA. No reference counting runs in ISR.
pub(super) struct FrameLease<P: LeasePool> {
    pool: Arc<P>,
    index: usize,
    sequence: u32,
}

impl<P: LeasePool> FrameLease<P> {
    pub(super) fn acquire(pool: &Arc<P>) -> Option<Arc<Self>> {
        pool.buffers().acquire().map(|(index, sequence)| {
            Arc::new(Self {
                pool: Arc::clone(pool),
                index,
                sequence,
            })
        })
    }

    pub(super) fn pool(&self) -> &P {
        &self.pool
    }

    pub(super) fn index(&self) -> usize {
        self.index
    }

    pub(super) fn sequence(&self) -> u32 {
        self.sequence
    }
}

impl<P: LeasePool> Drop for FrameLease<P> {
    fn drop(&mut self) {
        // Release precedes dropping our pool reference, so the states remain
        // allocated even when the camera has already shut down.
        self.pool.buffers().release(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_three_regions_using_only_y_with_padding_and_odd_x() {
        // Four pixels per row, with two bytes of padding between rows.
        let data = [
            250, 10, 251, 20, 252, 30, 253, 40, 255, 255, 240, 50, 241, 60, 242, 70, 243, 81,
        ];
        let regions = [
            Region {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            Region {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
            Region {
                x: 3,
                y: 1,
                width: 1,
                height: 1,
            },
        ];
        assert_eq!(
            average_luminance(&data, 4, 2, 10, &regions),
            Ok([45.125, 45.0, 81.0])
        );
    }

    #[test]
    fn overlapping_regions_preserve_raw_y_values() {
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        for y in [0, 16, 235, 255] {
            assert_eq!(
                average_luminance(&[127, y, 128, y], 2, 1, 4, &[region; 3]),
                Ok([f32::from(y); 3])
            );
        }
    }

    #[test]
    fn full_hd_average_does_not_overflow() {
        let data = vec![255; 1920 * 1080 * 2];
        let region = Region {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            average_luminance(&data, 1920, 1080, 3840, &[region; 3]),
            Ok([255.0; 3])
        );
    }

    #[test]
    fn invalid_regions_report_their_index() {
        let valid = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let invalid = [
            Region { width: 0, ..valid },
            Region { height: 0, ..valid },
            Region { x: 1, ..valid },
            Region { y: 1, ..valid },
            Region {
                x: usize::MAX,
                ..valid
            },
            Region {
                width: usize::MAX,
                ..valid
            },
            Region {
                height: usize::MAX,
                ..valid
            },
        ];
        for index in 0..3 {
            for region in invalid {
                let mut regions = [valid; 3];
                regions[index] = region;
                assert_eq!(
                    average_luminance(&[0; 8], 2, 2, 4, &regions),
                    Err(LuminanceError::InvalidRegion(index))
                );
            }
        }
    }

    #[test]
    fn malformed_frame_layouts_are_rejected() {
        let region = Region {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        for (width, height, stride) in [
            (0, 1, 4),
            (2, 0, 4),
            (3, 1, 6),
            (2, 1, 3),
            (usize::MAX - 1, 1, usize::MAX),
            (2, 3, usize::MAX),
        ] {
            assert_eq!(
                average_luminance(&[0; 8], width, height, stride, &[region; 3]),
                Err(LuminanceError::InvalidFrame)
            );
        }
        assert_eq!(
            average_luminance(&[0; 7], 2, 2, 4, &[region; 3]),
            Err(LuminanceError::InvalidFrame)
        );
    }

    #[test]
    fn alternates_and_publishes_only_completed_buffers() {
        let buffers = Buffers::new();
        assert_eq!(buffers.acquire(), None);
        assert_eq!(buffers.next_dma(), 0);
        assert_eq!(buffers.next_dma(), 1);
        assert_eq!(buffers.acquire(), None);
        buffers.finish(0, true);
        assert_eq!(buffers.acquire(), Some((0, 1)));
        buffers.release(0);
        assert_eq!(buffers.next_dma(), 0);
        buffers.finish(1, true);
        assert_eq!(buffers.acquire(), Some((1, 2)));
        assert_eq!(buffers.counts(), (2, 0));
    }

    #[test]
    fn held_frame_is_never_selected_by_dma() {
        let buffers = Buffers::new();
        buffers.next_dma();
        buffers.next_dma();
        buffers.finish(0, true);
        let (held, _) = buffers.acquire().unwrap();
        assert_eq!(held, 0);
        for _ in 0..100 {
            assert_eq!(buffers.next_dma(), 1);
            assert!(!buffers.finish(1, true));
            assert_eq!(buffers.acquire(), None);
        }
        assert_eq!(buffers.counts(), (101, 100));
        buffers.release(held);
        assert_eq!(buffers.next_dma(), 0);
        buffers.finish(1, true);
        assert_eq!(buffers.acquire(), Some((1, 102)));
    }

    #[test]
    fn stale_ready_frame_is_replaced_without_a_third_buffer() {
        let buffers = Buffers::new();
        buffers.next_dma();
        buffers.next_dma();
        buffers.finish(0, true);
        assert_eq!(buffers.next_dma(), 0);
        buffers.finish(1, true);
        assert_eq!(buffers.acquire(), Some((1, 2)));
        assert_eq!(buffers.counts(), (2, 1));
    }

    #[test]
    fn invalid_frame_is_not_published() {
        let buffers = Buffers::new();
        buffers.next_dma();
        buffers.next_dma();
        assert!(!buffers.finish(0, false));
        assert_eq!(buffers.acquire(), None);
        assert_eq!(buffers.counts(), (1, 1));
        assert_eq!(buffers.next_dma(), 0);
    }

    #[test]
    fn wakeup_is_requested_only_after_a_frame_is_ready() {
        let buffers = Buffers::new();
        buffers.next_dma();
        assert!(!buffers.finish(0, true));
        assert_eq!(buffers.acquire(), None);
        assert_eq!(buffers.next_dma(), 1);
        assert!(buffers.finish(0, true));
        assert_eq!(buffers.acquire(), Some((0, 2)));
        assert_eq!(buffers.next_dma(), 1);
        assert!(!buffers.finish(1, true));
        buffers.release(0);
        assert_eq!(buffers.next_dma(), 0);
        assert!(!buffers.finish(1, false));
        assert_eq!(buffers.acquire(), None);
    }

    #[test]
    fn acquisition_racing_dma_never_claims_the_same_buffer() {
        use std::sync::{Arc, Barrier};
        for _ in 0..100 {
            let buffers = Arc::new(Buffers::new());
            buffers.next_dma();
            buffers.next_dma();
            buffers.finish(0, true);
            let barrier = Arc::new(Barrier::new(2));
            let reader_buffers = buffers.clone();
            let reader_barrier = barrier.clone();
            let reader = std::thread::spawn(move || {
                reader_barrier.wait();
                reader_buffers.acquire()
            });
            barrier.wait();
            let dma = buffers.next_dma();
            if let Some((held, _)) = reader.join().unwrap() {
                assert_ne!(dma, held);
                buffers.release(held);
            }
        }
    }
}
