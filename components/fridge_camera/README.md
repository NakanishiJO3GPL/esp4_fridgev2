# OV5647 sensor integration (ESP32-P4 / ESP-IDF 6.1)

## Hosted SDIO backpressure overlay

`hosted_backpressure.cmake` substitutes a generated copy of the adjacent Hosted
SDIO source in its existing transport target. The only behavioral change is to
retain an unsent SW_AGGR DMA buffer across credit-wait timeouts, releasing the
bus lock and blocking between retries so RX and TCP ACKs can continue. Shutdown
still exits the wait; the upstream checkout is not modified. An unexpected or
ambiguous upstream code pattern fails configuration instead of applying an
unchecked patch. See `WIFI.md` for runtime counters and limitations.

Run `python3 tests/hosted_backpressure.py` to check the transformation and its
fail-closed guards. This is not a hardware throughput test.

## Rust API

`src/camera.rs` implements `Camera::new(Config)`, `try_frame()`,
`next_frame(Duration)`, `statistics()` and `close()`. Initialization starts the
sensor and capture. Each `Frame` guards one read-only PSRAM buffer. `Frame::lease()`
returns a read-only `OwnedFrame` that can move to another task without copying.
The buffer stays HELD until both the borrowed frame and all owned leases drop;
leases keep the allocations alive even after camera shutdown. Camera and borrowed
Frame remain non-Send/non-Sync.
The output is **UYVY, BT.709 full range**, stride 3840 bytes, 4,147,200 bytes
per frame. There are exactly two full-frame buffers, no hidden backup buffer.

```rust
use crate::camera::{Camera, Config, Result};
use std::time::Duration;

fn capture_one(sda_gpio: i32, scl_gpio: i32, xclk_gpio: Option<i32>) -> Result<()> {
    let mut config = Config::new(sda_gpio, scl_gpio);
    config.xclk_gpio = xclk_gpio;
    let mut camera = Camera::new(config)?;
    {
        let frame = camera.next_frame(Duration::from_secs(1))?;
        log::info!("Frame {}: buffer {}, {} bytes",
            frame.sequence(), frame.buffer_index(), frame.data().len());
        // Read/process frame.data() here. Dropping frame releases this buffer.
    }
    camera.close()
}
```

Use real board GPIOs, not the IDF test-board pin numbers. External I2C pull-ups
are the default expectation. `xclk_gpio = None` requires a module-provided
**24 MHz** clock; supplying a GPIO enables the P4 clock-router output. Supply the
module's own power rails externally. Check PHY LDO channel 3 / 2500 mV against
board wiring, or set `mipi_phy_supply = None` when already supplied/owned by the
application. `main.rs` starts a dedicated `camera_capture` thread using
`src/camera_thread.rs`, with **SDA GPIO7 / SCL GPIO8**; XCLK, reset and power-down
GPIOs are unused. The thread releases each frame after its processing callback
returns and reports frame metadata once per second. The camera module must
supply its own 24 MHz XCLK.

## Three-region mean luminance

`src/camera_buffers.rs` provides allocation-free UYVY luminance calculation;
`Frame::average_luminance(&[Region; 3])` calls it while holding the completed
buffer against DMA reuse. Specify rectangles in pixels from the top-left corner.
For example, inside the capture thread's `on_frame` callback:

```rust
use crate::camera::Region;

let regions = [
    Region { x: 0, y: 0, width: 640, height: 1080 },
    Region { x: 640, y: 0, width: 640, height: 1080 },
    Region { x: 1280, y: 0, width: 640, height: 1080 },
];
match frame.average_luminance(&regions) {
    Ok(means) => log::info!("Mean Y: {:.2}, {:.2}, {:.2}", means[0], means[1], means[2]),
    Err(error) => log::error!("Luminance calculation failed: {error}"),
}
```

`main.rs` now uses these left/center/right full-height thirds and calls
`average_luminance` for every acquired frame. Only metadata and mean-Y logging
are limited to once per second (plus the first frame). Sensor capture remains
configured at 1920x1080 RAW10, 30 FPS; no additional frame pacing is introduced.
Processing every sensor frame still requires calculation plus the worker's
existing one-tick idle delay to fit within the frame period. The two-buffer
policy drops frames rather than reducing sensor FPS when processing falls behind.
Results are `[f32; 3]` in input order, averaging only Y bytes, not U/V. They
retain the raw 0..=255 scale; nominal full-range black/white are 0/255,
without normalization or clipping. Switching the ISP from limited to full range
changes measured APL values and can affect the fixed trigger thresholds. Odd x coordinates and overlapping regions
are supported. Empty/out-of-bounds rectangles return
`LuminanceError::InvalidRegion(index)` with a zero-based index. Invalid frame
layouts return `InvalidFrame`. Calculation runs in task context, never the ISR;
large regions add processing time and can increase dropped frames. The existing
progress-based WDT feeding and idle-task delay remain unchanged.

## Center-region JPEG trigger notifications

`src/trigger.rs` implements the reference APL difference detector. On each
acquired frame, `main.rs` passes the three means to `AplTrigger::update_center`,
which examines only `means[1]`. Initial tuning follows the reference example:
`settle_velocity_abs_max = 0.05`, `settle_frames_required = 2`, and
`wait_to_detect_velocity_abs_min = 0.2`, in raw Y units per acquired-frame
interval. These are starting values and should be tuned on hardware.

The first input establishes history. Two consecutive stable differences produce
one `JpegTriggerNotification` containing the frame sequence, center APL and
signed difference. The next frame never fires; subsequent frames remain off
until the absolute difference reaches the rearm threshold. The rearm frame
itself also stays off. Failed luminance calculations and non-finite inputs reset
history. Monitoring/reset APIs match the reference detector. Dropped camera
frames mean differences are between acquired frames, not necessarily adjacent
sensor frames.

On a trigger, the capture callback calls `jpeg::CaptureSender::try_submit` while
holding that exact `Frame`. With verified UYVY packing, it transfers a read-only
owned lease through a bounded channel: no FHD snapshot copy or pixel processing.
A single availability token bounds queued/in-flight work; busy triggers are skipped
without waiting. The sensor's 30 FPS configuration and two DMA buffers are unchanged.
While JPEG holds one buffer, CSI reuses the other and may drop acquisition frames;
never overwrite the leased buffer. JPEG releases its lease immediately after successful
encoding/validation, before publication. A non-native verified layout falls back to
one preallocated snapshot and in-place repacking, without mutating camera buffers.

## Hardware JPEG worker

`src/jpeg.rs` implements `jpeg::spawn(Config, on_jpeg)` and returns a submission
handle plus `JoinHandle<camera::Result<()>>`. Initialization allocates all buffers
before returning; PSRAM/engine initialization errors are reported immediately.
The worker is **CPU1-pinned**, priority 4, with an **8192-byte internal stack**.
Caller pthread settings are restored, using the same RAII helpers as capture.

Verified native UYVY bypasses the entire JPEG preprocessing row loop, including
its per-32-row delays. For a non-native fallback, `src/jpeg_pixels.rs` repacks the
snapshot **in place**, from ISP BT.709 full-range UYVY into the calibrated encoder byte layout. Y/Cb/Cr sample values are unchanged:
there is no range expansion or BT.709-to-BT.601 matrix conversion. No RGB pixels
or separate raw-input buffer are allocated. UYVY packing needs no byte changes.
The JPEG retains BT.709 samples without signaling a BT.709 decoding matrix;
conventional browser JPEG decoders generally interpret YCbCr as BT.601, so
chromatic colors may differ from the original. Full range alone does not resolve
that matrix mismatch. The aligned camera buffer (or fallback snapshot) is passed to the P4 hardware
JPEG engine as **YUV422 input**, encoding **1920x1080**, quality **80** by default,
with **YUV422 subsampling**. FHD is aligned to the YUV422 16x8 MCU size.

The IDF YUV422 enum declaration and the inspected pre-release TRM disagree on
byte packing. Rather than assuming YVYU, the worker now **calibrates on the actual
encoder before accepting camera jobs**. It encodes a 32x16 known pattern with all
four packings (UYVY/YUYV/YVYU/VYUY) and both `pixel_reverse` values, at quality 100.
`src/jpeg_probe.rs` parses that baseline JPEG's Huffman-coded DCT coefficients:
DC block means check independent Y/Cb/Cr values across four 16x8 tiles, and the
sign of luma's first horizontal AC coefficient checks unequal adjacent-pixel
order. No hardware decoder RGB/range behavior is used as the reference.

The lowest score at or below 8 is selected; equal-score combinations prefer native
UYVY, then deterministic candidate order (`pixel_reverse=false` first). Probe parse/hardware errors or failure of all
candidates abort JPEG initialization instead of silently publishing invalid
colors. Probe operations use a temporary 32x16 input and the existing output
allocation. The temporary input is freed after calibration, with no FHD input
allocation on the native UYVY path. Production resolution/quality remain
unchanged. Calibration remains enabled, but routine probe/configuration logs
are removed. Synthetic host fixtures test the parser; actual input selection
is validated by the probe on the board at startup.

Passing this check verifies encoder packing, **not the camera/ISP data**. If camera
images remain incorrect afterwards, inspect the sensor Bayer configuration and
raw ISP YUV422 data rather than changing packing blindly.

Additional persistent PSRAM allocations (not extra camera DMA buffers):

| Purpose | Bytes |
|---|---:|
| JPEG output capacity | 8,388,608 |
| Native UYVY persistent total, excluding overhead | 8,388,608 (8 MiB) |
| Optional non-native fallback snapshot | 4,147,200 |
| Fallback persistent total, excluding overhead | 12,535,808 (about 12 MiB) |

Removing the BGR888 buffer saves **6,220,800 bytes (about 5.93 MiB)**.
Native UYVY additionally removes the **4,147,200-byte** snapshot. Including the
two capture buffers requires about **16 MiB** (fallback: about **20 MiB**) before
driver, allocator and application overhead. Camera buffers are cache/DMA aligned
PSRAM; the JPEG output and fallback input use the JPEG allocator. Output capacity
is a policy limit, not a guaranteed maximum; exceeding it is an encoding error.
Successful output sizes and JPEG SOI/EOI markers are checked before delivery.

`on_jpeg` executes on the JPEG worker with an `EncodedImage` containing trigger
metadata, dimensions and `data: &[u8]`. Bytes are valid only during the callback;
write/transmit/copy them there if persistence is needed. `main.rs` copies the
JPEG into the Wi-Fi HTTP publisher without normal completion/publication logs. See [Wi-Fi setup and reboot
recovery](../../WIFI.md) for credentials, C6 SDIO wiring, browser display and
additional bounded PSRAM usage. Filesystem persistence is not implemented.

JPEG timing measurements and routine JPEG/HTTP logs are removed. Normal app
output is limited to `Capture trigger: frame N, center APL=…, delta=…`.
Warnings/errors remain available for faults; browser image delivery and trigger
notifications are unchanged. Native UYVY still skips all JPEG input preprocessing;
fallback repacking retains its watchdog/idle yields.

The idle JPEG worker blocks receiving requests without a WDT subscription. It
subscribes for each actual job, feeds after conversion/encoding/callback progress,
and unsubscribes afterwards. Only fallback repacking blocks for one RTOS tick
every 32 rows. Native UYVY skips that loop; the worker still blocks after each
job and the hardware call has a **1000 ms** timeout. Callbacks must return
promptly. No independent timer or ISR feeds the worker's WDT.

Worker errors are logged immediately and returned by `join()`. A disconnected
worker disables further submissions in main's capture callback. Encoder retains
the camera lease or fallback snapshot during
the hardware call and releases it only after success. Hardware process errors terminate
JPEG processing and retain the engine, input and output until reboot. A retained
camera lease stays HELD, so capture cannot acquire further frames with only two
buffers (its existing wait eventually times out); it is never recycled into CSI DMA.
IDF 6.1 error-return paths do not
provide a verified hardware/callback quiescence fence for safely freeing them.
Do not automatically restart the encoder after such an error. Normal shutdown
finishes any submitted job, deletes the encoder and releases its allocations.

## Capture thread and watchdog

`camera_thread::spawn(config, on_frame)` returns a
`JoinHandle<camera::Result<()>>`. The worker is pinned to **CPU0**, priority 5,
with an **8192-byte internal-RAM stack**. It creates, reads and destroys `Camera`
on that same task/core; neither `Camera` nor its frame guard is moved across
threads. `on_frame` runs on the worker and can read `frame.data()` during the
call. It must return promptly and must not retain raw pointers to the frame.
The caller's effective pthread creation settings are restored after spawning;
core/stack settings are not inherited by unrelated threads.

The worker subscribes itself to the existing ESP-IDF Task WDT before camera
initialization. It feeds its subscription **only after acquiring and processing
a frame**, then blocks with `vTaskDelay(1)`. This explicit block also occurs when
a frame is immediately ready, so the idle task cannot be starved by a fast loop.
Resetting the camera subscription does not reset the idle tasks' subscriptions.
Frame acquisition retains a 2-second timeout; missing frames produce an error
rather than feeding a stalled acquisition forever. With the current 5-second
TWDT timeout, a callback or initialization that hangs is still detected.

Camera resources are cleaned up on the worker before its WDT subscription is
removed on exit. The worker does not disable/reconfigure the WDT or remove idle
task monitoring. ESP-IDF must initialize Task WDT at boot (the current
configuration does). `main` blocks on `join()` without polling and reports
worker errors/panics; other application work can be started before the join.

Run on a **core-pinned task**, and do not change its affinity while the camera
is alive. `new` rejects unpinned tasks; teardown checks the original task/core.
ESP-IDF's default main task is normally pinned. An existing DW-GDMA interrupt
group must be on this same core or CSI creation fails.

During normal operation the DMA alternates the two buffers. If a frame is still
borrowed when the next frame finishes, DMA reuses only the other buffer and drops
that completion. Unread old frames can also be replaced. This policy allows a
slow consumer without corrupting the borrowed image and without a third buffer.
There are no allocations, blocking waits, SCCB accesses or normal logs in the
callbacks. After publishing a valid READY buffer, the frame-completion ISR calls
`vTaskGenericNotifyGiveFromISR` and returns its wakeup flag to the IDF driver,
which handles the context switch after the ISR finishes. Dropped/invalid frames
are not notified. `next_frame` blocks in `ulTaskGenericNotifyTake` instead of
polling. The owning task's **notification slot 0 is exclusively reserved for the
camera** until it closes; `on_frame` must not use that slot for unrelated waits.
Pending notifications prevent a lost wakeup between checking READY and blocking.
Notifications can coalesce or become stale when a frame is replaced, so atomic
buffer ownership remains authoritative and every wake rechecks it. Finite waits
round up to RTOS ticks and retain the original timeout deadline.

The worker's post-processing `vTaskDelay(1)` remains necessary to allow idle work
even when a frame is immediately available (one tick, 10 ms at 100 Hz). Sub-tick
POSIX sleeps on ESP-IDF can busy-wait and must not replace that block. CSI error
flags are latched for inspection; reported transfer sizes are not a substitute
for checking actual sensor timing/frame integrity on hardware. AWB/AE/CCM tuning
is not part of this initial demosaic implementation.

`camera_stop.c` is a small **ESP-IDF 6.1-specific** teardown adapter, not an
alternative sensor driver. IDF's CSI completion ISR rearms DMA before publishing
completion. The helper masks DMA callbacks and stops CSI in one critical section
on the IRQ core before the controller, state or PSRAM buffers are freed. It uses
IDF's actual private CSI header, never a copied struct layout, and deliberately
fails compilation on other IDF minor versions until reviewed. If teardown fails,
remaining hardware-reachable allocations are retained rather than freed unsafely.

The Rust callback code expects the default non-cache-safe ISR configuration; do
not enable CSI/ISP cache-safe ISR options without moving every callback/callee to
IRAM and validating their internal-memory requirements.

## What is wired up

`Cargo.toml` uses the documented `esp-idf-sys.extra_components` metadata:

- Official `espressif/esp_cam_sensor` **2.1.0** and `esp_sccb_intf` **0.0.8**, both from
  `https://github.com/espressif/esp-video-components.git` at commit
  **`f977b225c3ce456ac5cb9413091509e260ca635f`**.
- The sensor's otherwise floating `cmake_utilities: 0.*` dependency is constrained
  to **`=0.5.3`**.
- `components/fridge_camera` declares driver dependencies and builds the small
  teardown adapter; `bindings.h` extends the **existing root bindings**. Deliberately
  no `bindings_module`: use `esp_idf_sys` and its existing I2C/CSI/ISP types,
  not incompatible copies in a separate module.
- Rust depends directly on `esp-idf-sys`, not `esp-idf-svc`/`esp-idf-hal`. HAL
  0.47's always-compiled legacy I2C code could otherwise pull the old driver's
  startup constructor into the ELF even when its runtime APIs are unused.
  `src/logger.rs` preserves `log` output through ESP-IDF under the `fridge` tag.
  No LTO or `CONFIG_I2C_SKIP_LEGACY_CONFLICT_CHECK` workaround is needed.

`esp-idf-sys` 0.38.1 includes CSI, ISP and LDO for IDF 6. The extra header
explicitly includes them and the cache-alignment helper too; what was missing
was sensor/SCCB bindings. Sensor detection is explicit
`ov5647_detect`, not a linker-section auto-detection loop. Upstream register
sequences are compiled unchanged; no local register tables or sensor wrapper.

On the next build, IDF Component Manager downloads the pinned components. Git
sources need GitHub access; registry dependencies need the Espressif component
registry/storage hosts. Preserve the generated `components_esp32p4.lock` when
produced by esp-idf-sys, including component hashes. This integration does not
require changes to the Rust dependency versions or the IDF selection in `.cargo/config.toml`.
If `ESP_IDF_SYS_EXTRA_COMPONENTS_FILE` is set, it overrides root Cargo metadata;
its entries must include this integration. Component Manager must be enabled.
The installed IDF Component Manager converts the sensor's sibling
`override_path: ../esp_sccb_intf` into a Git dependency at the same revision; SCCB
is also pinned explicitly here. Its dependency name intentionally remains
unnamespaced (`esp_sccb_intf`) to match the sensor manifest and avoid building a
second copy under `espressif/esp_sccb_intf`.

`sdkconfig.defaults` enables only the FHD format and makes it the default. Existing
persisted sdkconfig values take precedence over defaults: verify the generated
`sdkconfig.h` has `CONFIG_CAMERA_OV5647`,
`CONFIG_CAMERA_OV5647_MIPI_RAW10_1920X1080_30FPS`,
`CONFIG_CAMERA_OV5647_MIPI_DEFAULT_FMT_RAW10_1920X1080_30FPS`, and
`CONFIG_CAMERA_OV5647_CSI_LINESYNC_ENABLE` set to 1. Reset/reconfigure stale
sdkconfig through the normal project build workflow if needed. Do not define
`CONFIG_CAMERA_OV5647_MIPI_IF_FORMAT_INDEX_DEFAULT` manually: Kconfig derives it.

## Verified sensor mode

Select the entry returned by `esp_cam_sensor_query_format` with the exact name:

```text
MIPI_2lane_24Minput_RAW10_1920x1080_30fps
```

| Property | Official 2.1.0 driver value |
| --- | --- |
| Active size | 1920 x 1080 |
| Pixel format | `ESP_CAM_SENSOR_PIXFORMAT_RAW10` |
| Port | `ESP_CAM_SENSOR_MIPI_CSI` |
| Advertised frame rate | 30 fps |
| XCLK | 24,000,000 Hz |
| CSI lanes | 2 |
| Lane bit rate (`mipi_info.mipi_clk`) | **408,333,500 bit/s per lane** |
| Sensor timing PCLK | 81,666,700 Hz |
| HTS / VTS | 2416 / 1104 |
| Bayer | `ESP_CAM_SENSOR_BAYER_GBRG` -> ISP `COLOR_RAW_ELEMENT_ORDER_GBRG` |
| Line start/end short packets | Enabled by our defaults |
| SCCB address / sensor PID | 7-bit `0x36` / `0x5647` |

The lane rate comes from `OV5647_IDI_CLOCK_RATE_1920x1080_30FPS * 5`, not a generic
OV5647 450/500 Mbps assumption. Although the field's comment calls it a clock-lane
frequency, this driver stores the lane **bit rate**. Follow the official example
conversion `format.mipi_info.mipi_clk / 1000000` -> CSI `lane_bit_rate_mbps = 408`;
this API accepts only integer Mbps. Read it from the selected format rather than
hard-coding a different mode's value. The timing metadata gives approximately
29.58 us/line and 32.66 ms/frame (30.62 fps), so the advertised 30 fps should not be
treated as an exact wall-clock guarantee.

`isp_info` points to a union: inspect `isp_v1_info.bayer_type`, `pclk`, `hts`, `vts`.
`mipi_info` is an anonymous union member; bindgen typically exposes it through
`__bindgen_anon_1`. Use the generated layout rather than inventing a Rust struct.
Translate Bayer by **named variants**; sensor and IDF Bayer enum numbers need not
match. Mirror/flip changes can alter the effective Bayer order.

## Sensor API contract for camera.rs

All functions below are available at `sys::` after bindings regeneration. C
signatures map directly to bindgen pointer types (all `esp_err_t` returns must be
checked):

```c
esp_err_t i2c_new_master_bus(const i2c_master_bus_config_t *,
                             i2c_master_bus_handle_t *);
esp_err_t sccb_new_i2c_io(i2c_master_bus_handle_t,
                         const sccb_i2c_config_t *, esp_sccb_io_handle_t *);
esp_cam_sensor_device_t *ov5647_detect(esp_cam_sensor_config_t *);
esp_err_t esp_cam_sensor_query_format(esp_cam_sensor_device_t *,
                                     esp_cam_sensor_format_array_t *);
esp_err_t esp_cam_sensor_set_format(esp_cam_sensor_device_t *,
                                   const esp_cam_sensor_format_t *);
esp_err_t esp_cam_sensor_get_format(esp_cam_sensor_device_t *,
                                   esp_cam_sensor_format_t *);
esp_err_t esp_cam_sensor_ioctl(esp_cam_sensor_device_t *, uint32_t, void *);
esp_err_t esp_cam_sensor_del_dev(esp_cam_sensor_device_t *);
esp_err_t esp_sccb_del_i2c_io(esp_sccb_io_handle_t);
esp_err_t i2c_del_master_bus(i2c_master_bus_handle_t);
```

1. Supply module power and **24 MHz XCLK before detection**. Choose SDA/SCL,
   reset/power-down pins from the actual board schematic; the IDF test's GPIO
   7/8 are only test-board wiring, not a universal pin assignment. SCCB should
   normally use 100 kHz initially, `I2C_ADDR_BIT_LEN_7`, address `OV5647_SCCB_ADDR`.
   OV5647 uses 16-bit register addresses / 8-bit values; its driver calls the
   explicit `a16v8` SCCB functions. Use the new I2C-master driver, not legacy I2C
   simultaneously on that controller. External I2C pull-ups are recommended.
2. Fill `esp_cam_sensor_config_t`: `sccb_handle`, `sensor_port =
   ESP_CAM_SENSOR_MIPI_CSI`; `reset_pin`, `pwdn_pin`, `xclk_pin` are -1 if unused.
   The struct also has `xclk_freq_hz` (24,000,000 when applicable).
   **The pinned OV5647 driver has empty XCLK enable/disable macros and does not
   copy/use the config's XCLK frequency. Setting xclk_pin does not generate a
   clock.** For an external oscillator, leave `xclk_pin = -1`.
3. Call `ov5647_detect(&mut config)`; NULL means failure. Query formats and find
   the exact name, retaining the driver-owned pointer; call `set_format` while
   streaming is off. Do not copy/invent the official register list.
4. Configure buffers, CSI, ISP and demosaic; start capture before requesting
   sensor streaming to avoid losing initial frames.
5. Stream control takes a pointer to a **C int**, value 1/0, not a Rust bool:
   `esp_cam_sensor_ioctl(dev, sys::FRIDGE_CAMERA_SENSOR_IOC_S_STREAM as u32,
   (&mut enabled as *mut core::ffi::c_int).cast())`.
   The header's enum alias lets Clang evaluate the official `sizeof(int)` ioctl
   macro; it does not duplicate the command encoding.
6. Stop sensor output before tearing down capture, then delete the sensor,
   SCCB IO and I2C bus in that order. The pinned sensor delete operation frees
   its device allocation only: application-owned clocks/power/reset must be
   stopped or released separately.

Optional application-owned P4 clock-router API (also included in bindings):

```c
esp_err_t esp_cam_sensor_xclk_allocate(esp_cam_sensor_xclk_source_t,
                                      esp_cam_sensor_xclk_handle_t *);
esp_err_t esp_cam_sensor_xclk_start(esp_cam_sensor_xclk_handle_t,
                                   const esp_cam_sensor_xclk_config_t *);
esp_err_t esp_cam_sensor_xclk_stop(esp_cam_sensor_xclk_handle_t);
esp_err_t esp_cam_sensor_xclk_free(esp_cam_sensor_xclk_handle_t);
```

Use `ESP_CAM_SENSOR_XCLK_ESP_CLOCK_ROUTER`, config union `esp_clock_router_cfg`
(`xclk_pin`, `xclk_freq_hz = 24000000`); check the chosen GPIO is supported by the
clock-output driver. Allocate/start before sensor detection; stop/free last.
No XCLK GPIO is chosen by this integration.

## CSI / ISP contract and buffer constraints

```c
esp_err_t esp_cam_new_csi_ctlr(const esp_cam_ctlr_csi_config_t *,
                              esp_cam_ctlr_handle_t *);
esp_err_t esp_cam_ctlr_register_event_callbacks(esp_cam_ctlr_handle_t,
                                               const esp_cam_ctlr_evt_cbs_t *,
                                               void *);
esp_err_t esp_cam_ctlr_enable(esp_cam_ctlr_handle_t);
esp_err_t esp_cam_ctlr_start(esp_cam_ctlr_handle_t);
esp_err_t esp_cam_ctlr_receive(esp_cam_ctlr_handle_t, esp_cam_ctlr_trans_t *,
                              uint32_t timeout_ms);
esp_err_t esp_cam_ctlr_stop(esp_cam_ctlr_handle_t);
esp_err_t esp_cam_ctlr_disable(esp_cam_ctlr_handle_t);
esp_err_t esp_cam_ctlr_del(esp_cam_ctlr_handle_t);
esp_err_t esp_isp_new_processor(const esp_isp_processor_cfg_t *,
                               isp_proc_handle_t *);
esp_err_t esp_isp_demosaic_configure(isp_proc_handle_t,
                                    const esp_isp_demosaic_config_t *);
esp_err_t esp_isp_demosaic_enable(isp_proc_handle_t);
esp_err_t esp_isp_enable(isp_proc_handle_t);
esp_err_t esp_isp_demosaic_disable(isp_proc_handle_t);
esp_err_t esp_isp_disable(isp_proc_handle_t);
esp_err_t esp_isp_del_processor(isp_proc_handle_t);
esp_err_t esp_cache_get_alignment(uint32_t heap_caps, size_t *out_alignment);
```

- **IDF 6.1 CSI: input and output both `CAM_CTLR_COLOR_YUV422_UYVY`**, controller
  0, width/height 1920/1080, two lanes, integer lane rate 408, `queue_items = 2`,
  all input/output swap flags false. Keep custom `data_type` zero. The sensor
  still sends RAW10; these CSI color fields describe the ISP-to-DMA stream for
  this path, not the sensor wire format. RAW10 -> UYVY in the CSI config is
  rejected by the CSI driver's format converter with `ESP_ERR_NOT_SUPPORTED`.
  The detailed source trace and concrete configuration are below.
- ISP: source `ISP_INPUT_DATA_SOURCE_CSI`, input `ISP_COLOR_RAW10`, output
  `ISP_COLOR_YUV422`, size 1920/1080, GBRG, `has_line_start_packet` and
  `has_line_end_packet` true (match sensor `line_sync_en`), bypass false.
  Explicitly choose the YUV range and conversion standard. IDF 6.1 defines
  `ISP_COLOR_YUV422` as packed **UYVY**; the matching CSI enum is
  `CAM_CTLR_COLOR_YUV422_UYVY` (there is no plain `CAM_CTLR_COLOR_YUV422`).
  RAW -> YUV conversion needs the ISP:
  selecting a CSI output type alone does not demosaic.
- Use `ISP_CLK_SRC_DEFAULT`/PLL240 and `clk_hz = 240000000` as an initial
  configuration. IDF recommends approximately twice the sensor pixel rate
  (~163.3 MHz here); its integer clock divider cannot produce that value exactly
  from 240 MHz. 80 MHz from the low-resolution IDF test is not a safe FHD
  assumption; measure frame integrity/throughput on hardware.
- Local IDF 6.1 `isp_ll_set_output_data_color_format(ISP_COLOR_YUV422)` already
  sets the hardware demosaic and RGB-to-YUV enable bits. Explicit demosaic
  configure/enable calls can still be used to select interpolation/padding and
  establish the demosaic API's software state; do not assume creation leaves
  the hardware demosaic disabled. `esp_isp_demosaic_config_t` contains
  `grad_ratio`, `padding_mode`, `padding_data`, and optional line-tail padding
  start/end bytes. Choose valid IDF enum values.
- Acquire the MIPI PHY supply before CSI setup. The local test uses
  `esp_ldo_acquire_channel` with channel 3 / 2500 mV; confirm those settings
  against the board schematic and coordinate ownership with any DSI user.
- Each packed YUV422 buffer is **4,147,200 bytes**; two require **8,294,400 bytes**
  before other allocations. Allocate aligned PSRAM buffers (e.g.
  `heap_caps_aligned_alloc` with `MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT`), alignment
  at least 4 and the cache alignment returned by `esp_cache_get_alignment`.
  Buffers and lengths must satisfy `esp_cache_msync` alignment requirements.
  IDF's CSI driver performs M2C cache invalidation before the completion
  callback, **but requires the supplied transaction's `received_size` to be
  initialized to the full frame length in this local version** (see below).
  Do not let CPU readers race subsequent DMA writes.
- CSI allocates a third full-size backup buffer by default. Setting the CSI
  bitfield `bk_buffer_dis = 1` avoids that allocation, **but the driver's ISR
  asserts if a valid replacement buffer is unavailable**, and start fails if
  the initial `on_get_new_trans` returns none. Exactly two buffers require a
  deliberate ownership/drop policy; never DMA into a frame still borrowed by
  the consumer. A backup buffer is safer for a consumer that can stall.
- Callbacks are `bool (*)(esp_cam_ctlr_handle_t, esp_cam_ctlr_trans_t *, void *)`,
  running in ISR context. Return whether a higher-priority task was woken;
  `trans.buffer` and `trans.buflen` supply a buffer, `received_size` reports
  completion. Do not block, allocate, log normally, or perform SCCB transactions
  in callbacks. Our defaults do not enable cache-safe ISR modes, which would
  additionally require IRAM callbacks and internal-RAM callback state.

## IDF 6.1 CSI/ISP format configuration: source trace

The local CSI constructor first computes `in_bpp` and `out_bpp` from its color
fields, then unconditionally calls `s_csi_ctlr_format_conversion`. That function
accepts equal source/destination formats by setting the CSI bridge's **color
converter** bypass bit. RAW types are not in its supported conversion list, so
RAW10 -> UYVY fails. This converter bypass is distinct from **ISP pipeline**
bypass; leave `esp_isp_processor_cfg_t.flags.bypass_isp = 0`.

Sizing also requires equality here: the constructor uses
`h_res * v_res * in_bpp / 64` as the DW-GDMA transfer count in **64-bit items**,
and `h_res * v_res * out_bpp / 8` as the frame-buffer byte count. UYVY/UYVY gives
16 bpp for both: **518,400 DMA items / 4,147,200 bytes**. RAW10/RAW10 would get
past the converter but incorrectly size this ISP-output capture at 2,592,000
bytes, regardless of how large an application buffer you provide.

The unchanged official pinned video implementation explicitly uses
`input_data_color_type = state.out_color` and
`output_data_color_type = state.out_color` for the non-converting ISP path and
IDF >= 6.0. Its separate ISP config derives input color from the sensor state.
Local ISP core sets RAW10 decoding (`isp_data_type = 1`), CSI input routing
(`isp_in_src = 0`, `mipi_data_en = 1`), YUV422 output (`isp_out_type = 1`),
demosaic and RGB-to-YUV bits. `esp_isp_enable` sets `isp_en = 1`; it does **not**
rewrite the CSI driver's software bpp fields or converter configuration. No
private bridge-register override is required for this path.

Concrete C configuration (use generated bindgen fields/setters in Rust):

```c
esp_cam_ctlr_csi_config_t csi = {
    .ctlr_id = 0,
    .h_res = 1920,
    .v_res = 1080,
    .data_lane_num = 2,
    .lane_bit_rate_mbps = 408,
    .input_data_color_type = CAM_CTLR_COLOR_YUV422_UYVY,
    .output_data_color_type = CAM_CTLR_COLOR_YUV422_UYVY,
    .queue_items = 2,
    .bk_buffer_dis = true, // only with the replacement protocol below
};
esp_isp_processor_cfg_t isp = {
    .clk_src = ISP_CLK_SRC_DEFAULT,
    .clk_hz = 240000000,
    .input_data_source = ISP_INPUT_DATA_SOURCE_CSI,
    .input_data_color_type = ISP_COLOR_RAW10,
    .output_data_color_type = ISP_COLOR_YUV422,
    .yuv_range = ISP_COLOR_RANGE_FULL,
    .yuv_std = ISP_YUV_CONV_STD_BT709,
    .has_line_start_packet = true,
    .has_line_end_packet = true,
    .h_res = 1920,
    .v_res = 1080,
    .bayer_order = COLOR_RAW_ELEMENT_ORDER_GBRG,
};
```

Unspecified fields/flags above are zero. The YUV range/standard are explicit
choices, not mandatory sensor settings. Match the consumer's conversion.
Create CSI before ISP; keep the sensor stream off until both are configured and
capture is armed. The official video driver creates/enables/starts CSI, then
creates/enables ISP, then requests sensor streaming. Creating/enabling ISP before
starting CSI is also consistent with the local test; ensure neither constructor
is run while the sensor is actively sending frames.

## Exactly two buffers: callback ordering and cache-length workaround

Local `csi_dma_trans_done_callback` executes these steps:

1. Zero-initialize `new_trans`, call `on_get_new_trans` (or dequeue if that
   callback is absent).
2. Invalidate `new_trans.buffer` for `new_trans.buflen` bytes.
3. Configure **and enable the next DMA transfer**.
4. Invalidate the completed `ctlr->trans.buffer` for its **existing**
   `ctlr->trans.received_size` bytes.
5. Assign the old transaction's `received_size = fb_size_in_bytes`, then call
   `on_trans_finished`.
6. Replace `ctlr->trans` with `new_trans`.

At initial start, `on_get_new_trans` is also called and its transaction is saved
without assigning `received_size`. Thus, in **every** `on_get_new_trans` call,
including first start and same-buffer reuse, supply all three fields:

```c
trans->buffer = selected_buffer;
trans->buflen = 4147200;
trans->received_size = 4147200;
```

Although documented as driver output, `received_size` is read before it is
written in this implementation. Setting only the completed callback's value is
too late and does not initialize the next transaction. A zero length reaches
`esp_cache_msync`/the HAL; do not rely on it to invalidate frame data or succeed.
The official video's get-new callback also omits this field in the pinned
source, so it is a reference for formats, not a fix for this local cache issue.

The proposed CAS/drop protocol is consistent with this ordering, provided:

- Initially reserve one `FREE -> DMA` buffer; never publish the initial
  transaction as though it had completed.
- At completion selection, atomically reserve the **other** buffer via
  `FREE -> DMA` or `READY -> DMA`. The latter deliberately drops an unborrowed
  ready frame. The consumer must acquire through a competing atomic
  `READY -> HELD` CAS **before reading any payload or constructing a slice**.
  A plain state load followed by returning a borrowed frame is unsafe.
- If reservation loses to the consumer or the other buffer is already `HELD`,
  return the **current DMA buffer** again and retain its `DMA` state. Record the
  selected next pointer/index before returning from get-new; no consumer can
  acquire this current buffer.
- In `on_trans_finished`, compare the completed buffer against that selected
  next buffer. If equal, **drop this completion and do not publish READY**:
  DMA has already been rearmed on it. If different, publish only the old buffer
  `DMA -> READY` with Release ordering after driver cache invalidation.
- Consumer acquisition needs Acquire semantics; consumer release/drop must
  finish all reads before a Release transition `HELD -> FREE`. Producer claims
  need Acquire semantics (typically AcqRel CAS), to pair with both completion
  publication and consumer release. Failed CAS must never grant ownership.
  Store per-frame metadata before publishing READY and read it only after
  successful acquisition; the consumer must also stop using all slices on
  release. Keep allocation/metadata stable and valid until capture is stopped
  and outstanding borrows are released.
- During get-new and completion, two buffer states may temporarily be `DMA`
  (old completed + next reserved). Keep the old/current and selected-next
  identities distinct; do not clear or republish the wrong buffer. When reusing
  the same buffer the driver's old-frame invalidation occurs after rearm; it
  must not trigger any CPU read/publication of that discarded frame.
- `queue_items` does not provide another safety mechanism when get-new is
  installed: that callback takes precedence over the receive queue. Start also
  calls get-new, so do not combine this protocol with queued ownership unless
  the two paths are explicitly coordinated.

This protocol can preserve a consumer-held frame indefinitely by dropping new
frames into the other buffer. It is safe only if all borrowing/reclamation
paths obey the state machine; hardware timing and cache behavior still need
on-device validation. No consumer writes to DMA payload are assumed.

## Research references

- Local IDF: `components/esp_driver_cam/test_apps/csi/main/test_csi_ov5647.c`.
  This is an **800x640 RAW8** test, not an FHD timing recipe.
- Its `idf_component.yml` depends on local example `sensor_init`; that component's
  manifest specifies `espressif/esp_cam_sensor: ^2.1.0` (we pin the 2.1.0 commit).
- [Official pinned CSI/video configuration and callbacks](https://github.com/espressif/esp-video-components/blob/f977b225c3ce456ac5cb9413091509e260ca635f/esp_video/src/device/esp_video_csi_device.c), especially `csi_video_start` (lines 456-538).
- Local IDF source trace: `esp_driver_cam/csi/src/esp_cam_ctlr_csi.c`, constructor
  lines 156-227/258 and callback/start lines 398-445/573-596; ISP setup in
  `esp_driver_isp/src/isp_core.c` lines 138-179/249-258; register behavior in
  `esp_hal_cam/esp32p4/include/hal/isp_ll.h` lines 387-425/467-501.
- [Official pinned OV5647 driver](https://github.com/espressif/esp-video-components/blob/f977b225c3ce456ac5cb9413091509e260ca635f/esp_cam_sensor/sensors/ov5647/ov5647.c)
- [Official pinned timing/register tables](https://github.com/espressif/esp-video-components/blob/f977b225c3ce456ac5cb9413091509e260ca635f/esp_cam_sensor/sensors/ov5647/private_include/ov5647_settings.h)
- [esp-idf-sys metadata documentation](https://github.com/esp-rs/esp-idf/blob/621f955647d2ab5a30a788f6283e2e6a73d46962/esp-idf-sys/BUILD-OPTIONS.md#extra-esp-idf-components)

Validation performed for this integration: Cargo TOML parsing, a standalone
header/API/stream-ioctl syntax probe with the installed ESP32-P4 GCC and local
IDF 6.1 headers, the unchanged official OV5647 C driver with only FHD RAW10
enabled, and Kconfig selection/default-index checks. These passed. The probe
uses the existing IDF picolibc toolchain flags, not a host C compiler.

Follow-up validation: the corrected concrete CSI/ISP configuration above compiles
against local IDF 6.1, with static assertions confirming the matching UYVY FOURCC
and 518,400-item DMA count. A finite atomic two-buffer ownership model explored
18 reachable states / 26 transitions (including consumer acquisition/release
between callback phases) without DMA ever owning a HELD buffer. This checks the
abstract protocol, not the parent's Rust implementation or hardware races.

The integration-only checks above were followed by a complete ESP32-P4 debug
build with regenerated bindings and the Rust capture module. The two-buffer
protocol has independent host tests in `tests/camera_buffers.rs`. Hardware
capture, actual frame rate, PHY signal integrity and image quality still require
an OV5647 connected to the target board.
