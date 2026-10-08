//! Triggered full-frame JPEG encoding on CPU1 using read-only CSI buffer leases.
//!
//! Fixed UYVY input goes directly to JPEG DMA without copying or pixel processing.
//! Hardware pixel_reverse is disabled; there is no startup calibration.

use crate::{
    camera::{
        capture::{self as camera, Frame, OwnedFrame, HEIGHT, WIDTH},
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

const STACK_BYTES: usize = 8192;

const ENCODE_TIMEOUT_MS: i32 = 1000;
const OUTPUT_CAPACITY: usize = 2 * 1024 * 1024;

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
    available: Receiver<()>,
    requests: SyncSender<Request>,
}

impl CaptureSender {
    /// Lease the UYVY triggering frame without copying or preprocessing.
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
        match self.available.try_recv() {
            Ok(()) => (),
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => {
                return Err(error("JPEG worker stopped", sys::ESP_ERR_INVALID_STATE));
            }
        };
        self.requests
            .try_send(Request {
                frame: frame.lease(),
                trigger,
            })
            .map_err(|_| error("submit JPEG frame", sys::ESP_ERR_INVALID_STATE))?;
        Ok(true)
    }
}

struct Request {
    frame: OwnedFrame,
    trigger: JpegTriggerNotification,
}

/// Start the CPU1-pinned, priority-4 worker and wait for resource initialization.
/// One job token bounds memory/backlog; new triggers while busy are skipped.
/// The callback runs on CPU1 and must return promptly. Worker errors are returned
/// by join(); dropping CaptureSender shuts down the worker after its current job.
/// Requires 2 MiB output PSRAM; input uses a read-only camera buffer lease.
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
            let mut encoder = match Encoder::new(config) {
                Ok(encoder) => encoder,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return Err(error);
                }
            };
            if free_tx.send(()).is_err() || ready_tx.send(Ok(())).is_err() {
                return Ok(());
            }

            let result = (|| -> camera::Result<()> {
                while let Ok(Request { frame, trigger }) = requests_rx.recv() {
                    // An idle worker waits indefinitely without a WDT subscription.
                    // Subscribe only for real work; feed after encoding,
                    // never from a timer that could hide a stalled encoder.
                    let watchdog = TaskWatchdog::subscribe()?;
                    watchdog.reset()?;
                    let (input, size) = encoder.encode(frame)?;
                    // Success guarantees JPEG DMA is done. Release the camera lease
                    // before publication/network copying to resume capture promptly.
                    drop(input);
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
                    if free_tx.send(()).is_err() {
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
                available: free_rx,
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

// Encoder owns the allocation throughout DMA use; no aliases cross tasks.
unsafe impl Send for Memory {}

impl Memory {
    fn output(size: usize) -> camera::Result<Self> {
        let config = sys::jpeg_encode_memory_alloc_cfg_t {
            buffer_direction: sys::jpeg_enc_buffer_alloc_direction_t_JPEG_ENC_ALLOC_OUTPUT_BUFFER,
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
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { sys::heap_caps_free(self.pointer.cast()) };
    }
}

struct Encoder {
    handle: sys::jpeg_encoder_handle_t,
    input: Option<OwnedFrame>,
    output: Memory,
    config: sys::jpeg_encode_cfg_t,
    dma_quiescent: bool,
}

impl Encoder {
    fn new(config: Config) -> camera::Result<Self> {
        let output = Memory::output(OUTPUT_CAPACITY)?;
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
                // Fixed UYVY input; hardware byte-order reversal is disabled.
                src_type: sys::jpeg_enc_input_format_t_JPEG_ENCODE_IN_FORMAT_YUV422,
                // FHD is aligned to 16x8 MCUs, so no padded rows are necessary.
                sub_sample: sys::jpeg_down_sampling_type_t_JPEG_DOWN_SAMPLING_YUV422,
                image_quality: config.quality,
                pixel_reverse: false,
            },
            dma_quiescent: true,
        })
    }

    fn encode(&mut self, input: OwnedFrame) -> camera::Result<(OwnedFrame, usize)> {
        // Keep the DMA-visible allocation/lease on every error/unwind path.
        self.input = Some(input);
        let input = self.input.as_ref().unwrap();
        let input_bytes = input.data();
        let input_pointer = input_bytes.as_ptr();
        let input_size = self
            .config
            .width
            .checked_mul(self.config.height)
            .and_then(|pixels| pixels.checked_mul(2))
            .filter(|bytes| *bytes as usize <= input_bytes.len())
            .ok_or_else(|| error("invalid JPEG input dimensions", sys::ESP_ERR_INVALID_SIZE))?;
        let mut size = 0;
        self.dma_quiescent = false;
        check("encode JPEG image", unsafe {
            sys::jpeg_encoder_process(
                self.handle,
                &self.config,
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
