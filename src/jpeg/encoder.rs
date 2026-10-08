//! Triggered full-frame JPEG encoding on CPU1 using read-only CSI buffer leases.
//!
//! Verified UYVY goes directly to JPEG DMA without copying or pixel processing.
//! Other verified layouts fall back to one exclusive snapshot for repacking.

use crate::{
    camera::{
        capture::{self as camera, Frame, OwnedFrame, FRAME_BYTES, HEIGHT, STRIDE, WIDTH},
        thread::TaskWatchdog,
        trigger::JpegTriggerNotification,
    },
    pthread_config::PthreadConfigGuard,
};
use esp_idf_sys as sys;
use std::{
    io, ptr, slice,
    sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError},
    thread::{self, JoinHandle},
};

use super::{pixels, probe};

const STACK_BYTES: usize = 8192;

const ENCODE_TIMEOUT_MS: i32 = 1000;
const OUTPUT_CAPACITY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Hardware JPEG quality, 1..=100.
    pub quality: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { quality: 80 }
    }
}

/// Valid only for the duration of the worker's callback. Copy/save data there
/// if it must outlive the callback; the output buffer is reused for later JPEGs.
pub struct EncodedImage<'a> {
    pub trigger: JpegTriggerNotification,
    pub width: usize,
    pub height: usize,
    pub data: &'a [u8],
}

/// Single-producer handle, intended to live in the camera callback.
/// One availability token bounds queued/in-flight JPEG work to one frame.
pub struct CaptureSender {
    free_snapshot: Receiver<Option<Memory>>,
    requests: SyncSender<Request>,
}

impl CaptureSender {
    /// Lease the triggering frame without copying when UYVY was verified.
    /// Other verified layouts copy into the preallocated fallback snapshot.
    /// Returns false while the single job token is queued/in use.
    pub fn try_submit(
        &self,
        frame: &Frame<'_>,
        trigger: JpegTriggerNotification,
    ) -> camera::Result<bool> {
        if trigger.frame_sequence != frame.sequence() {
            return Err(error(
                "JPEG trigger/frame sequence mismatch",
                sys::ESP_ERR_INVALID_ARG,
            ));
        }
        let snapshot = match self.free_snapshot.try_recv() {
            Ok(snapshot) => snapshot,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => {
                return Err(error("JPEG worker stopped", sys::ESP_ERR_INVALID_STATE));
            }
        };
        let input = match snapshot {
            None => Input::Frame(frame.lease()),
            Some(mut snapshot) => {
                snapshot.bytes_mut()[..FRAME_BYTES].copy_from_slice(frame.data());
                Input::Snapshot(snapshot)
            }
        };
        self.requests
            .try_send(Request { input, trigger })
            .map_err(|_| error("submit JPEG snapshot", sys::ESP_ERR_INVALID_STATE))?;
        Ok(true)
    }
}

struct Request {
    input: Input,
    trigger: JpegTriggerNotification,
}

/// Retains exclusive snapshot ownership or a read-only HELD camera buffer
/// until JPEG DMA has completed. Frame input must never be repacked in place.
enum Input {
    Snapshot(Memory),
    Frame(OwnedFrame),
}

impl Input {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Snapshot(memory) => memory.bytes(),
            Self::Frame(frame) => frame.data(),
        }
    }

    fn release(self) -> Option<Memory> {
        match self {
            Self::Snapshot(memory) => Some(memory),
            Self::Frame(frame) => {
                drop(frame);
                None
            }
        }
    }
}

/// Start the CPU1-pinned, priority-4 worker and wait for resource initialization.
/// One job token bounds memory/backlog; new triggers while busy are skipped.
/// The callback runs on CPU1 and must return promptly. Worker errors are returned
/// by join(); dropping CaptureSender shuts down the worker after its current job.
/// Requires 8 MiB output PSRAM; non-UYVY fallback needs a further 4 MiB snapshot.
pub fn spawn<F>(
    config: Config,
    mut on_jpeg: F,
) -> io::Result<(CaptureSender, JoinHandle<camera::Result<()>>)>
where
    F: FnMut(&EncodedImage<'_>) + Send + 'static,
{
    if !(1..=100).contains(&config.quality) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JPEG quality must be 1..=100",
        ));
    }
    let (requests_tx, requests_rx) = sync_channel::<Request>(1);
    let (free_tx, free_rx) = sync_channel(1);
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
    let _thread_config = PthreadConfigGuard::configure(1, 4, STACK_BYTES, c"jpeg_encode")?;
    let worker = thread::Builder::new()
        .name("jpeg_encode".into())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let resources = (|| -> camera::Result<_> {
                let mut encoder = Encoder::new(config)?;
                // Calibration needs only a small pattern, not an FHD allocation.
                drop(encoder.calibrate(Memory::encoder(probe::WIDTH * probe::HEIGHT * 2, false)?)?);
                let snapshot = if encoder.packing == pixels::Packing::Uyvy {
                    None
                } else {
                    Some(Memory::snapshot()?)
                };
                Ok((encoder, snapshot))
            })();
            let (mut encoder, snapshot) = match resources {
                Ok(resources) => resources,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return Err(error);
                }
            };
            if free_tx.send(snapshot).is_err() || ready_tx.send(Ok(())).is_err() {
                return Ok(());
            }

            let result = (|| -> camera::Result<()> {
                while let Ok(Request { mut input, trigger }) = requests_rx.recv() {
                    // An idle worker waits indefinitely without a WDT subscription.
                    // Subscribe only for real work; feed after conversion/encoding,
                    // never from a timer that could hide a stalled encoder.
                    let watchdog = TaskWatchdog::subscribe()?;
                    // Native UYVY skips repacking and conversion yields entirely.
                    if let Input::Snapshot(snapshot) = &mut input {
                        for (row_index, row) in snapshot.bytes_mut()[..FRAME_BYTES]
                            .chunks_exact_mut(STRIDE)
                            .enumerate()
                        {
                            pixels::repack_row(row, encoder.packing);
                            if (row_index + 1) % 32 == 0 {
                                // Sub-tick POSIX sleeps busy-wait on IDF. Block for idle1.
                                unsafe { sys::vTaskDelay(1) };
                            }
                        }
                    }
                    watchdog.reset()?;
                    let (input, size) = encoder.encode(input)?;
                    // Success guarantees JPEG DMA is done. Release the camera lease
                    // before publication/network copying to resume capture promptly.
                    let snapshot = input.release();
                    watchdog.reset()?;

                    on_jpeg(&EncodedImage {
                        trigger,
                        width: WIDTH,
                        height: HEIGHT,
                        data: &encoder.output.bytes()[..size],
                    });
                    watchdog.reset()?;
                    drop(watchdog);
                    // Return availability only after encoding/callback finish.
                    if free_tx.send(snapshot).is_err() {
                        break;
                    }
                    unsafe { sys::vTaskDelay(1) };
                }
                Ok(())
            })();
            if let Err(error) = &result {
                // Report immediately even while main is joining camera capture.
                log::error!("JPEG worker stopped: {error}");
            }
            result
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((
            CaptureSender {
                free_snapshot: free_rx,
                requests: requests_tx,
            },
            worker,
        )),
        ready => {
            // The worker cannot wait for requests if initialization failed.
            drop(requests_tx);
            let _ = worker.join();
            let message = match ready {
                Ok(Err(message)) => message,
                _ => "JPEG worker exited during initialization".into(),
            };
            Err(io::Error::other(message))
        }
    }
}

/// Exclusive ownership, never shared with another task while being read/written.
struct Memory {
    pointer: *mut u8,
    size: usize,
}

// Channels transfer the allocation, not aliases. The snapshot crosses task
// boundaries only while DMA is quiescent; Encoder owns it during hardware use.
unsafe impl Send for Memory {}

impl Memory {
    fn snapshot() -> camera::Result<Self> {
        // Allocated only for a non-native packing fallback.
        Self::encoder(FRAME_BYTES, false)
    }

    fn encoder(size: usize, output: bool) -> camera::Result<Self> {
        let config = sys::jpeg_encode_memory_alloc_cfg_t {
            buffer_direction: if output {
                sys::jpeg_enc_buffer_alloc_direction_t_JPEG_ENC_ALLOC_OUTPUT_BUFFER
            } else {
                sys::jpeg_enc_buffer_alloc_direction_t_JPEG_ENC_ALLOC_INPUT_BUFFER
            },
        };
        let mut allocated_size = 0;
        let pointer =
            unsafe { sys::jpeg_alloc_encoder_mem(size, &config, &mut allocated_size) }.cast();
        Self::checked(pointer, allocated_size)
    }

    fn checked(pointer: *mut u8, size: usize) -> camera::Result<Self> {
        if pointer.is_null() {
            Err(error("allocate JPEG PSRAM buffer", sys::ESP_ERR_NO_MEM))
        } else {
            Ok(Self { pointer, size })
        }
    }

    fn bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.pointer, self.size) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.pointer, self.size) }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { sys::heap_caps_free(self.pointer.cast()) };
    }
}

struct Encoder {
    handle: sys::jpeg_encoder_handle_t,
    input: Option<Input>,
    output: Memory,
    config: sys::jpeg_encode_cfg_t,
    packing: pixels::Packing,
    dma_quiescent: bool,
}

impl Encoder {
    fn new(config: Config) -> camera::Result<Self> {
        let output = Memory::encoder(OUTPUT_CAPACITY, true)?;
        let engine_config = sys::jpeg_encode_engine_cfg_t {
            intr_priority: 0,
            timeout_ms: ENCODE_TIMEOUT_MS,
            ..Default::default()
        };
        let mut handle = ptr::null_mut();
        check("create hardware JPEG encoder", unsafe {
            sys::jpeg_new_encoder_engine(&engine_config, &mut handle)
        })?;
        Ok(Self {
            handle,
            input: None,
            output,
            config: sys::jpeg_encode_cfg_t {
                width: WIDTH as u32,
                height: HEIGHT as u32,
                // The enum selects the hardware YUV422 mode, not a proven byte
                // order. calibrate() checks packing and pixel_reverse on silicon.
                src_type: sys::jpeg_enc_input_format_t_JPEG_ENCODE_IN_FORMAT_YUV422,
                // FHD is aligned to 16x8 MCUs, so no padded rows are necessary.
                sub_sample: sys::jpeg_down_sampling_type_t_JPEG_DOWN_SAMPLING_YUV422,
                image_quality: config.quality,
                pixel_reverse: false,
            },
            packing: pixels::Packing::Yuyv,
            dma_quiescent: true,
        })
    }

    fn calibrate(&mut self, mut snapshot: Memory) -> camera::Result<Memory> {
        let mut best = None;
        for packing in pixels::PROBE_ORDER {
            for reverse in [false, true] {
                let config = sys::jpeg_encode_cfg_t {
                    width: probe::WIDTH as u32,
                    height: probe::HEIGHT as u32,
                    image_quality: 100,
                    pixel_reverse: reverse,
                    ..self.config
                };
                for (y, row) in snapshot.bytes_mut()[..probe::WIDTH * probe::HEIGHT * 2]
                    .chunks_exact_mut(probe::WIDTH * 2)
                    .enumerate()
                {
                    for (pair_index, bytes) in row.chunks_exact_mut(4).enumerate() {
                        bytes.copy_from_slice(&packing.pack(probe::pair(pair_index * 2, y)));
                    }
                }
                let (returned, size) =
                    self.encode_with_config(Input::Snapshot(snapshot), config)?;
                snapshot = returned.release().expect("calibration snapshot");
                let score = probe::score(&self.output.bytes()[..size]).map_err(|reason| {
                    log::error!("JPEG packing probe parse failed: {reason}");
                    error("parse JPEG packing probe", sys::ESP_ERR_INVALID_RESPONSE)
                })?;

                if best.is_none_or(|(previous, _, _)| score < previous) {
                    best = Some((score, packing, reverse));
                }
                unsafe { sys::vTaskDelay(1) };
            }
        }
        let Some((score, packing, reverse)) = best else {
            return Err(error(
                "no JPEG packing probe result",
                sys::ESP_ERR_INVALID_RESPONSE,
            ));
        };
        if score > 8 {
            log::error!("JPEG packing probe rejected all candidates; best score={score}");
            return Err(error(
                "verify hardware JPEG YUV422 packing",
                sys::ESP_ERR_INVALID_RESPONSE,
            ));
        }
        self.packing = packing;
        self.config.pixel_reverse = reverse;

        Ok(snapshot)
    }

    fn encode(&mut self, input: Input) -> camera::Result<(Input, usize)> {
        self.encode_with_config(input, self.config)
    }

    fn encode_with_config(
        &mut self,
        input: Input,
        config: sys::jpeg_encode_cfg_t,
    ) -> camera::Result<(Input, usize)> {
        // Keep the DMA-visible allocation/lease on every error/unwind path.
        self.input = Some(input);
        let input = self.input.as_ref().unwrap();
        let input_bytes = input.bytes();
        let input_pointer = input_bytes.as_ptr();
        let input_size = config
            .width
            .checked_mul(config.height)
            .and_then(|pixels| pixels.checked_mul(2))
            .filter(|bytes| *bytes as usize <= input_bytes.len())
            .ok_or_else(|| error("invalid JPEG input dimensions", sys::ESP_ERR_INVALID_SIZE))?;
        let mut size = 0;
        self.dma_quiescent = false;
        check("encode JPEG image", unsafe {
            sys::jpeg_encoder_process(
                self.handle,
                &config,
                input_pointer,
                input_size,
                self.output.pointer,
                self.output.size as u32,
                &mut size,
            )
        })?;
        // IDF success follows RX EOF/channel teardown and cache invalidation.
        self.dma_quiescent = true;
        let size = size as usize;
        if size < 4 || size > self.output.size {
            return Err(error(
                "invalid encoded JPEG size",
                sys::ESP_ERR_INVALID_SIZE,
            ));
        }
        let bytes = &self.output.bytes()[..size];
        if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
            return Err(error(
                "invalid encoded JPEG markers",
                sys::ESP_ERR_INVALID_RESPONSE,
            ));
        }
        Ok((self.input.take().unwrap(), size))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.dma_quiescent {
            // IDF 6.1 error paths request abort, but do not provide a verified
            // callback/idle fence. Never free memory potentially visible to DMA.
            log::error!("JPEG failed; retaining encoder and DMA buffers until reboot");
            if let Some(input) = self.input.take() {
                // A leaked camera lease keeps both allocation and HELD state alive,
                // even if Camera later shuts down. Never return it to CSI DMA.
                std::mem::forget(input);
            }
            self.output.pointer = ptr::null_mut();
            return;
        }
        if let Err(error) = check("delete hardware JPEG encoder", unsafe {
            sys::jpeg_del_encoder_engine(self.handle)
        }) {
            log::error!("{error}");
        }
    }
}

fn error(operation: &'static str, code: sys::esp_err_t) -> camera::Error {
    camera::Error {
        operation,
        source: sys::EspError::from(code).expect("nonzero error code"),
    }
}

fn check(operation: &'static str, code: sys::esp_err_t) -> camera::Result<()> {
    if code == sys::ESP_OK {
        Ok(())
    } else {
        Err(error(operation, code))
    }
}
