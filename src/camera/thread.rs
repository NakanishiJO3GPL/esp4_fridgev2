//! Core-pinned camera acquisition thread with progress-based task watchdog feeding.

use crate::{
    camera::capture::{self as camera, Camera, Config, Frame},
    pthread_config::PthreadConfigGuard,
};
use esp_idf_sys as sys;
use std::{
    io, ptr,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const THREAD_NAME: &str = "camera_capture";
const STACK_BYTES: usize = 8192;
const CORE: i32 = 0;
const PRIORITY: usize = 5;
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Creates and owns Camera entirely inside a CPU0-pinned thread. The callback
/// runs on that thread and may read the frame only for the duration of the call.
/// It must return promptly: the watchdog is fed AFTER frame processing, not by
/// an independent timer. Do not pass Camera/Frame pointers to another thread;
/// use `Frame::lease()` for an owned read-only cross-task lease instead.
/// Task-notification slot 0 is reserved for camera wakeups; the callback must
/// not use that slot for unrelated notifications or waits.
///
/// The returned handle propagates camera/WDT errors; join() blocks the caller
/// without busy-waiting. Task WDT must already be initialized by ESP-IDF.
pub fn spawn<F>(config: Config, mut on_frame: F) -> io::Result<JoinHandle<camera::Result<()>>>
where
    F: FnMut(&Frame<'_>) + Send + 'static,
{
    // ESP-IDF pthread configuration belongs to this calling task. Restore it
    // after pthread_create snapshots the camera-specific core/stack settings.
    let _thread_config =
        PthreadConfigGuard::configure(CORE, PRIORITY, STACK_BYTES, c"camera_capture")?;
    thread::Builder::new()
        .name(THREAD_NAME.into())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let watchdog = TaskWatchdog::subscribe()?;
            // Construction order makes Camera drop BEFORE the watchdog guard
            // on errors/unwind, so teardown is also covered by the watchdog.
            let mut camera = Camera::new(config)?;
            log::info!("Camera acquisition thread started on CPU{CORE}");
            let mut last_fps_log = Instant::now();
            let mut previous_statistics = camera.statistics();
            let mut acquired_frames = 0u32;

            loop {
                {
                    let frame = camera.next_frame(FRAME_TIMEOUT)?;
                    on_frame(&frame);
                } // Release the frame before sleeping or awaiting another one.
                watchdog.reset()?;
                acquired_frames += 1;
                let now = Instant::now();
                let elapsed = now.duration_since(last_fps_log);
                if elapsed >= Duration::from_secs(1) {
                    let statistics = camera.statistics();
                    let completed = statistics
                        .completed
                        .wrapping_sub(previous_statistics.completed);
                    let dropped = statistics.dropped.wrapping_sub(previous_statistics.dropped);
                    let seconds = elapsed.as_secs_f64();
                    log::info!(
                        "Camera FPS: capture={:.2}, acquired={:.2}, dropped={} in {:.2}s",
                        f64::from(completed) / seconds,
                        f64::from(acquired_frames) / seconds,
                        dropped,
                        seconds,
                    );
                    last_fps_log = now;
                    previous_statistics = statistics;
                    acquired_frames = 0;
                }
                // Always block, even if next_frame immediately found READY.
                // Feeding this task's WDT does not feed IDLE0's subscription.
                unsafe { sys::vTaskDelay(1) };
            }
        })
}

pub(crate) struct TaskWatchdog;

impl TaskWatchdog {
    pub(crate) fn subscribe() -> camera::Result<Self> {
        check("subscribe worker thread to task WDT", unsafe {
            sys::esp_task_wdt_add(ptr::null_mut())
        })?;
        Ok(Self)
    }

    pub(crate) fn reset(&self) -> camera::Result<()> {
        check("reset worker thread task WDT", unsafe {
            sys::esp_task_wdt_reset()
        })
    }
}

impl Drop for TaskWatchdog {
    fn drop(&mut self) {
        if let Err(error) = check("unsubscribe worker thread from task WDT", unsafe {
            sys::esp_task_wdt_delete(ptr::null_mut())
        }) {
            log::error!("{error}");
        }
    }
}

fn idf_error(operation: &'static str, code: sys::esp_err_t) -> camera::Error {
    camera::Error {
        operation,
        source: sys::EspError::from(code).expect("error status must not be ESP_OK"),
    }
}

fn check(operation: &'static str, code: sys::esp_err_t) -> camera::Result<()> {
    if code == sys::ESP_OK {
        Ok(())
    } else {
        Err(idf_error(operation, code))
    }
}
