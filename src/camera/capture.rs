//! OV5647: two-lane MIPI RAW10 -> ESP32-P4 ISP -> packed UYVY (YUV422).
//!
//! The module owns its I2C bus, CSI controller, ISP and two PSRAM frame buffers.
//! Supply the module's power rails and 24 MHz XCLK before detection; an optional
//! clock-router output can generate XCLK. GPIO assignments are board-specific.
//! `Camera::new` starts streaming. `next_frame` borrows a completed buffer until
//! the returned `Frame` and its owned leases are dropped. If the consumer stalls,
//! capture continues on the other buffer and drops frames rather than overwriting
//! the borrowed buffer.
//! `Frame::lease` retains that same buffer read-only, including after shutdown.

use core::sync::atomic::{AtomicU32, Ordering};
use core::{ffi::c_void, marker::PhantomData, ptr, slice};
use esp_idf_sys::{self as sys, EspError};
use std::{
    ffi::CStr,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use super::buffers::{self, Buffers, FrameLease, LeasePool};
pub use super::buffers::{LuminanceError, Region};
use super::stop;

pub const WIDTH: usize = 1920;
pub const HEIGHT: usize = 1080;
pub const STRIDE: usize = WIDTH * 2;
pub const FRAME_BYTES: usize = STRIDE * HEIGHT;
pub const FRAME_BUFFER_COUNT: usize = 2;
const FORMAT_NAME: &[u8] = b"MIPI_2lane_24Minput_RAW10_1920x1080_30fps";
const XCLK_HZ: u32 = 24_000_000;
// Reserved exclusively for camera frame wakeups on the owning task.
const FRAME_NOTIFICATION_INDEX: sys::UBaseType_t = 0;

/// Confirm channel/voltage against the board schematic. `None` in `Config`
/// means the application already owns and supplies the MIPI PHY rail.
#[derive(Clone, Copy, Debug)]
pub struct MipiPhySupply {
    pub channel: i32,
    pub voltage_mv: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub sda_gpio: i32,
    pub scl_gpio: i32,
    /// -1 lets ESP-IDF allocate an unused I2C controller.
    pub i2c_port: i32,
    pub reset_gpio: Option<i32>,
    pub pwdn_gpio: Option<i32>,
    /// None requires an external 24 MHz oscillator. Some GPIOs do not support
    /// clock-router output; ESP-IDF validates the selected GPIO.
    pub xclk_gpio: Option<i32>,
    pub internal_i2c_pullups: bool,
    pub mipi_phy_supply: Option<MipiPhySupply>,
}

impl Config {
    /// No SDA/SCL pin numbers are assumed. The PHY defaults match IDF's P4
    /// example, but must be checked against the actual board.
    pub fn new(sda_gpio: i32, scl_gpio: i32) -> Self {
        Self {
            sda_gpio,
            scl_gpio,
            i2c_port: -1,
            reset_gpio: None,
            pwdn_gpio: None,
            xclk_gpio: None,
            internal_i2c_pullups: false,
            mipi_phy_supply: Some(MipiPhySupply {
                channel: 3,
                voltage_mv: 2500,
            }),
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub operation: &'static str,
    pub source: EspError,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.operation, self.source)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = core::result::Result<T, Error>;

fn check(operation: &'static str, code: sys::esp_err_t) -> Result<()> {
    match EspError::from(code) {
        Some(source) => Err(Error { operation, source }),
        None => Ok(()),
    }
}
fn failure(operation: &'static str, code: sys::esp_err_t) -> Error {
    Error {
        operation,
        source: EspError::from(code).unwrap(),
    }
}

/// All storage dereferenced by CSI callbacks is allocated in internal RAM.
struct PoolStorage {
    pointers: [*mut u8; FRAME_BUFFER_COUNT],
    buffers: Buffers,
}

/// Private allocation owner; never exposes mutable frame data or its Arc.
struct FramePool {
    storage: *mut PoolStorage,
}

// SAFETY: storage and frame addresses are stable and initialized before capture
// or leases are published. Only Buffers' atomics change on the CPU thereafter.
// DMA writes only DMA buffers; read-only leases hold HELD until their final drop.
// CaptureState retains a pool reference until CSI callbacks/DMA are quiesced,
// deliberately leaking it if teardown fails. Final destruction may run on any task.
unsafe impl Send for FramePool {}
unsafe impl Sync for FramePool {}

impl LeasePool for FramePool {
    fn buffers(&self) -> &Buffers {
        unsafe { &(*self.storage).buffers }
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        unsafe {
            for pointer in (*self.storage).pointers {
                sys::heap_caps_free(pointer.cast());
            }
            ptr::drop_in_place(self.storage);
            sys::heap_caps_free(self.storage.cast());
        }
    }
}

struct CaptureState {
    notify_task: sys::TaskHandle_t,
    pointers: [*mut u8; FRAME_BUFFER_COUNT],
    // ISR uses this internal-RAM pointer, never the task-side Arc metadata.
    buffers: *const Buffers,
    pool: Arc<FramePool>,
    errors: AtomicU32,
}

impl CaptureState {
    fn buffers(&self) -> &Buffers {
        // The pool reference above keeps this internal allocation alive.
        unsafe { &*self.buffers }
    }
}

/// Not Send/Sync: create, consume and destroy on the same task/core. All ISR
/// sharing is limited to internal CaptureState/PoolStorage and atomic ownership flags.
pub struct Camera {
    owner_task: sys::TaskHandle_t,
    owner_core: sys::BaseType_t,
    i2c: sys::i2c_master_bus_handle_t,
    sccb: sys::esp_sccb_io_handle_t,
    sensor: *mut sys::esp_cam_sensor_device_t,
    xclk: sys::esp_cam_sensor_xclk_handle_t,
    xclk_started: bool,
    ldo: sys::esp_ldo_channel_handle_t,
    csi: sys::esp_cam_ctlr_handle_t,
    csi_enabled: bool,
    csi_started: bool,
    isp: sys::isp_proc_handle_t,
    isp_enabled: bool,
    demosaic_enabled: bool,
    sensor_started: bool,
    state: *mut CaptureState,
}

#[derive(Clone, Copy, Debug)]
pub struct Statistics {
    /// Counts and frame sequence numbers wrap at u32::MAX.
    pub completed: u32,
    pub dropped: u32,
    pub csi_error_flags: u32,
}

/// Read-only UYVY frame: each pair of pixels is [U, Y0, V, Y1].
/// DMA cannot reuse the buffer until this frame and all its owned leases drop.
/// Remains tied to the borrowing Camera and cannot cross tasks or be shared.
pub struct Frame<'a> {
    lease: Arc<FrameLease<FramePool>>,
    _camera: PhantomData<&'a Camera>,
    _not_send_sync: PhantomData<Rc<()>>,
}

/// Zero-copy, read-only UYVY lease, transferable to another task (`Send`).
/// Retains the buffer allocation after Camera shutdown. Leaking a lease retains
/// the pool allocations until reboot; while capturing, its buffer stays HELD.
/// Additional calls to `Frame::lease` share the same final-release guard.
pub struct OwnedFrame {
    lease: Arc<FrameLease<FramePool>>,
}

impl OwnedFrame {
    pub fn data(&self) -> &[u8] {
        lease_data(&self.lease)
    }

    pub fn sequence(&self) -> u32 {
        self.lease.sequence()
    }
}

fn lease_data(lease: &FrameLease<FramePool>) -> &[u8] {
    // SAFETY: READY -> HELD preceded guard construction and IDF invalidated
    // the completed cache. The guard retains the pool and prevents DMA reuse
    // for the entire returned borrow. No mutable CPU access is exposed.
    unsafe { slice::from_raw_parts((*lease.pool().storage).pointers[lease.index()], FRAME_BYTES) }
}
impl Frame<'_> {
    pub fn data(&self) -> &[u8] {
        lease_data(&self.lease)
    }

    /// Share this HELD buffer without copying or consuming the borrowed frame.
    /// Each lease retains the pool independently of the Camera's lifetime.
    pub fn lease(&self) -> OwnedFrame {
        OwnedFrame {
            lease: Arc::clone(&self.lease),
        }
    }

    /// Mean raw Y (0..=255) for three pixel rectangles, in input order.
    /// Calculation runs on this held frame; DMA cannot overwrite it meanwhile.
    pub fn average_luminance(
        &self,
        regions: &[Region; 3],
    ) -> core::result::Result<[f32; 3], LuminanceError> {
        buffers::average_luminance(self.data(), WIDTH, HEIGHT, STRIDE, regions)
    }

    pub fn buffer_index(&self) -> usize {
        self.lease.index()
    }
    pub fn sequence(&self) -> u32 {
        self.lease.sequence()
    }
    pub fn width(&self) -> usize {
        WIDTH
    }
    pub fn height(&self) -> usize {
        HEIGHT
    }
    pub fn stride(&self) -> usize {
        STRIDE
    }
}

impl Camera {
    /// Initializes the official OV5647 FHD register table and starts capture.
    /// Needs at least 8,294,400 bytes of free PSRAM plus allocator overhead.
    /// Call from a core-pinned task (the default ESP-IDF main task is pinned).
    /// Do not change that task's affinity while the camera is alive.
    /// Its task-notification slot 0 is reserved for frame wakeups until close.
    pub fn new(config: Config) -> Result<Self> {
        let owner_task = unsafe { sys::xTaskGetCurrentTaskHandle() };
        let owner_core = unsafe { sys::xTaskGetCoreID(owner_task) };
        if owner_core < 0 || owner_core >= sys::CONFIG_FREERTOS_NUMBER_OF_CORES as i32 {
            return Err(failure(
                "camera requires a core-pinned task",
                sys::ESP_ERR_INVALID_STATE,
            ));
        }
        let mut camera = Self {
            owner_task,
            owner_core,
            i2c: ptr::null_mut(),
            sccb: ptr::null_mut(),
            sensor: ptr::null_mut(),
            xclk: ptr::null_mut(),
            xclk_started: false,
            ldo: ptr::null_mut(),
            csi: ptr::null_mut(),
            csi_enabled: false,
            csi_started: false,
            isp: ptr::null_mut(),
            isp_enabled: false,
            demosaic_enabled: false,
            sensor_started: false,
            state: ptr::null_mut(),
        };
        // Drop unwinds successfully created resources on initialization failure.
        unsafe {
            camera.initialize(config)?;
        }
        Ok(camera)
    }

    unsafe fn initialize(&mut self, config: Config) -> Result<()> {
        if config.sda_gpio < 0
            || config.scl_gpio < 0
            || config.sda_gpio == config.scl_gpio
            || [config.reset_gpio, config.pwdn_gpio, config.xclk_gpio]
                .into_iter()
                .flatten()
                .any(|pin| pin < 0)
        {
            return Err(failure(
                "invalid camera GPIO configuration",
                sys::ESP_ERR_INVALID_ARG,
            ));
        }
        if let Some(supply) = config.mipi_phy_supply {
            let cfg = sys::esp_ldo_channel_config_t {
                chan_id: supply.channel,
                voltage_mv: supply.voltage_mv,
                ..Default::default()
            };
            check(
                "acquire MIPI PHY LDO",
                sys::esp_ldo_acquire_channel(&cfg, &mut self.ldo),
            )?;
        }
        if let Some(pin) = config.xclk_gpio {
            check(
                "allocate XCLK",
                sys::esp_cam_sensor_xclk_allocate(
                    sys::esp_cam_sensor_xclk_source_t_ESP_CAM_SENSOR_XCLK_ESP_CLOCK_ROUTER,
                    &mut self.xclk,
                ),
            )?;
            let mut cfg = sys::esp_cam_sensor_xclk_config_t::default();
            cfg.__bindgen_anon_1.esp_clock_router_cfg.xclk_pin = pin as sys::gpio_num_t;
            cfg.__bindgen_anon_1.esp_clock_router_cfg.xclk_freq_hz = XCLK_HZ;
            check(
                "start 24 MHz XCLK",
                sys::esp_cam_sensor_xclk_start(self.xclk, &cfg),
            )?;
            self.xclk_started = true;
        }
        let mut bus_cfg = sys::i2c_master_bus_config_t {
            i2c_port: config.i2c_port,
            sda_io_num: config.sda_gpio as sys::gpio_num_t,
            scl_io_num: config.scl_gpio as sys::gpio_num_t,
            glitch_ignore_cnt: 7,
            ..Default::default()
        };
        // Bindgen's Default is zeroed; IDF 6.1 requires an explicit clock source.
        bus_cfg.__bindgen_anon_1.clk_source = sys::soc_periph_i2c_clk_src_t_I2C_CLK_SRC_DEFAULT;
        bus_cfg
            .flags
            .set_enable_internal_pullup(config.internal_i2c_pullups as u32);
        check(
            "create camera I2C bus",
            sys::i2c_new_master_bus(&bus_cfg, &mut self.i2c),
        )?;
        let sccb_cfg = sys::sccb_i2c_config_t {
            dev_addr_length: sys::i2c_addr_bit_len_t_I2C_ADDR_BIT_LEN_7,
            device_address: sys::OV5647_SCCB_ADDR as u16,
            scl_speed_hz: 100_000,
            addr_bits_width: 16,
            val_bits_width: 8,
        };
        check(
            "create OV5647 SCCB",
            sys::sccb_new_i2c_io(self.i2c, &sccb_cfg, &mut self.sccb),
        )?;
        let mut sensor_cfg = sys::esp_cam_sensor_config_t {
            sccb_handle: self.sccb,
            reset_pin: config.reset_gpio.unwrap_or(-1) as sys::gpio_num_t,
            pwdn_pin: config.pwdn_gpio.unwrap_or(-1) as sys::gpio_num_t,
            // The driver has no XCLK implementation; we own the clock above.
            xclk_pin: -1_i32 as sys::gpio_num_t,
            xclk_freq_hz: XCLK_HZ as i32,
            sensor_port: sys::esp_cam_sensor_port_t_ESP_CAM_SENSOR_MIPI_CSI,
        };
        self.sensor = sys::ov5647_detect(&mut sensor_cfg);
        if self.sensor.is_null() {
            log::error!(
                "OV5647 ID read failed: SDA=GPIO{}, SCL=GPIO{}, SCCB=0x{:02X}, internal pull-ups={}",
                config.sda_gpio,
                config.scl_gpio,
                sys::OV5647_SCCB_ADDR,
                config.internal_i2c_pullups,
            );
            // Detection powers the sensor off on failure. Only probe when no
            // driver-controlled reset/power-down pins can have disabled it.
            if config.reset_gpio.is_none() && config.pwdn_gpio.is_none() {
                match sys::i2c_master_probe(self.i2c, sys::OV5647_SCCB_ADDR as u16, 100) {
                    sys::ESP_OK => log::error!(
                        "SCCB 0x36 responds with ACK, but sensor ID read failed; check XCLK and sensor/module type"
                    ),
                    sys::ESP_ERR_NOT_FOUND => log::error!(
                        "SCCB 0x36 returned NACK; check module power, SDA/SCL wiring, reset/power-down levels and XCLK"
                    ),
                    sys::ESP_ERR_TIMEOUT => log::error!(
                        "SCCB probe timed out; check external pull-ups, bus voltage and SDA/SCL stuck low"
                    ),
                    code => log::error!("SCCB probe failed: {:?}", EspError::from(code)),
                }
            }
            if config.xclk_gpio.is_none() {
                log::error!("No ESP32-P4 XCLK output configured; confirm the module supplies its own 24 MHz clock");
            }
            return Err(failure(
                "detect OV5647 (check power, XCLK and SCCB)",
                sys::ESP_ERR_NOT_FOUND,
            ));
        }
        self.set_sensor_stream(false)?;
        let mut formats = sys::esp_cam_sensor_format_array_t::default();
        check(
            "query OV5647 formats",
            sys::esp_cam_sensor_query_format(self.sensor, &mut formats),
        )?;
        if formats.format_array.is_null() || formats.count == 0 {
            return Err(failure(
                "OV5647 format list is empty",
                sys::ESP_ERR_NOT_FOUND,
            ));
        }
        let format = slice::from_raw_parts(formats.format_array, formats.count as usize)
            .iter()
            .find(|f| !f.name.is_null() && CStr::from_ptr(f.name).to_bytes() == FORMAT_NAME)
            .ok_or_else(|| {
                failure(
                    "OV5647 FHD RAW10 mode not enabled in sdkconfig",
                    sys::ESP_ERR_NOT_SUPPORTED,
                )
            })?;
        let mipi = format.__bindgen_anon_1.mipi_info;
        if format.width as usize != WIDTH
            || format.height as usize != HEIGHT
            || format.format != sys::esp_cam_sensor_output_format_t_ESP_CAM_SENSOR_PIXFORMAT_RAW10
            || format.port != sys::esp_cam_sensor_port_t_ESP_CAM_SENSOR_MIPI_CSI
            || format.xclk != XCLK_HZ as i32
            || mipi.lane_num != 2
            || format.isp_info.is_null()
        {
            return Err(failure(
                "unexpected OV5647 format metadata",
                sys::ESP_ERR_NOT_SUPPORTED,
            ));
        }
        let bayer = match (*format.isp_info).isp_v1_info.bayer_type {
            sys::esp_cam_sensor_bayer_pattern_t_ESP_CAM_SENSOR_BAYER_GBRG => {
                sys::color_raw_element_order_t_COLOR_RAW_ELEMENT_ORDER_GBRG
            }
            sys::esp_cam_sensor_bayer_pattern_t_ESP_CAM_SENSOR_BAYER_BGGR => {
                sys::color_raw_element_order_t_COLOR_RAW_ELEMENT_ORDER_BGGR
            }
            sys::esp_cam_sensor_bayer_pattern_t_ESP_CAM_SENSOR_BAYER_GRBG => {
                sys::color_raw_element_order_t_COLOR_RAW_ELEMENT_ORDER_GRBG
            }
            sys::esp_cam_sensor_bayer_pattern_t_ESP_CAM_SENSOR_BAYER_RGGB => {
                sys::color_raw_element_order_t_COLOR_RAW_ELEMENT_ORDER_RGGB
            }
            _ => {
                return Err(failure(
                    "unsupported Bayer order",
                    sys::ESP_ERR_NOT_SUPPORTED,
                ))
            }
        };
        check(
            "select OV5647 1920x1080 RAW10",
            sys::esp_cam_sensor_set_format(self.sensor, format),
        )?;
        // set_format may change stream state in sensor drivers: enforce standby.
        self.set_sensor_stream(false)?;
        self.allocate_buffers()?;

        // In IDF 6.1, CSI's two color fields describe the DMA stream AFTER ISP.
        // RAW10 -> UYVY here is rejected and would miscalculate DMA item count.
        let mut csi_cfg = sys::esp_cam_ctlr_csi_config_t {
            ctlr_id: 0,
            h_res: WIDTH as u32,
            v_res: HEIGHT as u32,
            data_lane_num: 2,
            lane_bit_rate_mbps: (mipi.mipi_clk / 1_000_000) as i32,
            input_data_color_type: sys::cam_ctlr_color_t_CAM_CTLR_COLOR_YUV422_UYVY,
            output_data_color_type: sys::cam_ctlr_color_t_CAM_CTLR_COLOR_YUV422_UYVY,
            queue_items: 2,
            ..Default::default()
        };
        // Exactly two buffers: no hidden third full-frame fallback allocation.
        csi_cfg.__bindgen_anon_1.set_bk_buffer_dis(1);
        check(
            "create MIPI CSI controller",
            sys::esp_cam_new_csi_ctlr(&csi_cfg, &mut self.csi),
        )?;
        let callbacks = sys::esp_cam_ctlr_evt_cbs_t {
            on_get_new_trans: Some(on_get_new_trans),
            on_trans_finished: Some(on_trans_finished),
            on_error: Some(on_error),
        };
        check(
            "register CSI callbacks",
            sys::esp_cam_ctlr_register_event_callbacks(self.csi, &callbacks, self.state.cast()),
        )?;
        check("enable CSI", sys::esp_cam_ctlr_enable(self.csi))?;
        self.csi_enabled = true;
        let isp_cfg = sys::esp_isp_processor_cfg_t {
            clk_hz: 240_000_000,
            input_data_source: sys::isp_input_data_source_t_ISP_INPUT_DATA_SOURCE_CSI,
            input_data_color_type: sys::isp_color_t_ISP_COLOR_RAW10,
            output_data_color_type: sys::isp_color_t_ISP_COLOR_YUV422,
            yuv_range: sys::color_range_t_COLOR_RANGE_FULL,
            yuv_std: sys::isp_yuv_conv_std_t_ISP_YUV_CONV_STD_BT709,
            has_line_start_packet: mipi.line_sync_en,
            has_line_end_packet: mipi.line_sync_en,
            h_res: WIDTH as u32,
            v_res: HEIGHT as u32,
            bayer_order: bayer,
            ..Default::default()
        };
        check(
            "create RAW10 to YUV422 ISP",
            sys::esp_isp_new_processor(&isp_cfg, &mut self.isp),
        )?;
        let mut demosaic_cfg = sys::esp_isp_demosaic_config_t {
            padding_mode:
                sys::isp_demosaic_edge_padding_mode_t_ISP_DEMOSAIC_EDGE_PADDING_MODE_SRND_DATA,
            ..Default::default()
        };
        demosaic_cfg.grad_ratio.__bindgen_anon_1.set_integer(1);
        check(
            "configure ISP demosaic",
            sys::esp_isp_demosaic_configure(self.isp, &demosaic_cfg),
        )?;
        check(
            "enable ISP demosaic",
            sys::esp_isp_demosaic_enable(self.isp),
        )?;
        self.demosaic_enabled = true;
        check("enable ISP", sys::esp_isp_enable(self.isp))?;
        self.isp_enabled = true;
        check("start CSI capture", sys::esp_cam_ctlr_start(self.csi))?;
        self.csi_started = true;
        // On stream-on failure, still attempt stream-off during cleanup.
        self.sensor_started = true;
        self.set_sensor_stream(true)?;
        log::info!(
            "OV5647: {}x{} RAW10 -> ISP -> UYVY, two PSRAM buffers ({} bytes each)",
            WIDTH,
            HEIGHT,
            FRAME_BYTES
        );
        Ok(())
    }

    unsafe fn allocate_buffers(&mut self) -> Result<()> {
        let caps = sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT | sys::MALLOC_CAP_DMA;
        let mut alignment = 0;
        check(
            "query camera cache alignment",
            sys::esp_cache_get_alignment(caps, &mut alignment),
        )?;
        alignment = alignment.max(64);
        if !alignment.is_power_of_two() || FRAME_BYTES % alignment != 0 {
            return Err(failure(
                "unsupported frame cache alignment",
                sys::ESP_ERR_NOT_SUPPORTED,
            ));
        }
        self.state = sys::heap_caps_malloc(
            core::mem::size_of::<CaptureState>(),
            sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
        )
        .cast();
        if self.state.is_null() {
            return Err(failure(
                "allocate internal camera state",
                sys::ESP_ERR_NO_MEM,
            ));
        }
        let storage: *mut PoolStorage = sys::heap_caps_malloc(
            core::mem::size_of::<PoolStorage>(),
            sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
        )
        .cast();
        if storage.is_null() {
            // CaptureState has not been initialized, so cleanup must not drop it.
            sys::heap_caps_free(self.state.cast());
            self.state = ptr::null_mut();
            return Err(failure(
                "allocate internal camera pool",
                sys::ESP_ERR_NO_MEM,
            ));
        }
        storage.write(PoolStorage {
            pointers: [ptr::null_mut(); FRAME_BUFFER_COUNT],
            buffers: Buffers::new(),
        });
        self.state.write(CaptureState {
            notify_task: self.owner_task,
            pointers: [ptr::null_mut(); FRAME_BUFFER_COUNT],
            buffers: ptr::addr_of!((*storage).buffers),
            pool: Arc::new(FramePool { storage }),
            errors: AtomicU32::new(0),
        });
        // No callbacks are registered yet. Discard stale wakeups before capture.
        sys::ulTaskGenericNotifyTake(FRAME_NOTIFICATION_INDEX, 1, 0);
        // Initialize before registering callbacks or exposing any pool leases.
        // The pool owns partial allocations as well, for initialization failures.
        for index in 0..FRAME_BUFFER_COUNT {
            let pointer = sys::heap_caps_aligned_alloc(alignment, FRAME_BYTES, caps).cast();
            (*storage).pointers[index] = pointer;
            (*self.state).pointers[index] = pointer;
            if pointer.is_null() {
                return Err(failure(
                    "allocate YUV422 frame buffer in PSRAM",
                    sys::ESP_ERR_NO_MEM,
                ));
            }
        }
        Ok(())
    }

    unsafe fn set_sensor_stream(&self, enabled: bool) -> Result<()> {
        let mut value: core::ffi::c_int = enabled as i32;
        check(
            "set OV5647 streaming",
            sys::esp_cam_sensor_ioctl(
                self.sensor,
                sys::FRIDGE_CAMERA_SENSOR_IOC_S_STREAM as u32,
                (&mut value as *mut core::ffi::c_int).cast(),
            ),
        )
    }

    pub fn try_frame(&mut self) -> Option<Frame<'_>> {
        let state = unsafe { &*self.state };
        FrameLease::acquire(&state.pool).map(|lease| Frame {
            lease,
            _camera: PhantomData,
            _not_send_sync: PhantomData,
        })
    }

    /// Wait for a completed frame using the owning task's notification slot 0.
    /// Notifications are wakeup hints; atomic buffer ownership remains authoritative.
    /// For continuous capture, process/drop the guard before requesting another.
    pub fn next_frame(&mut self, timeout: Duration) -> Result<Frame<'_>> {
        if unsafe { sys::xTaskGetCurrentTaskHandle() } != self.owner_task {
            return Err(failure(
                "frame wait requires the camera's owning task",
                sys::ESP_ERR_INVALID_STATE,
            ));
        }
        let state = unsafe { &*self.state };
        let start = Instant::now();
        loop {
            if let Some(lease) = FrameLease::acquire(&state.pool) {
                return Ok(Frame {
                    lease,
                    _camera: PhantomData,
                    _not_send_sync: PhantomData,
                });
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(failure("wait for camera frame", sys::ESP_ERR_TIMEOUT));
            }
            // Round finite waits up to ticks; avoid both zero (busy-wait) and
            // portMAX_DELAY (infinite). Recompute from the original deadline.
            let ticks = (remaining.as_nanos() * sys::CONFIG_FREERTOS_HZ as u128)
                .div_ceil(1_000_000_000)
                .clamp(1, sys::TickType_t::MAX as u128 - 1)
                as sys::TickType_t;
            // An ISR between acquire() and this call leaves a pending count,
            // so waking cannot be lost. Coalesce stale notifications and always
            // recheck buffer ownership; a notified frame may have been replaced.
            unsafe { sys::ulTaskGenericNotifyTake(FRAME_NOTIFICATION_INDEX, 1, ticks) };
        }
    }

    pub fn statistics(&self) -> Statistics {
        let state = unsafe { &*self.state };
        let (completed, dropped) = state.buffers().counts();
        Statistics {
            completed,
            dropped,
            csi_error_flags: state.errors.load(Ordering::Relaxed),
        }
    }

    /// Stop capture and release resources; reports teardown errors.
    /// Outstanding owned leases retain the frame pool until their final drop.
    pub fn close(mut self) -> Result<()> {
        unsafe { self.cleanup() }
    }

    unsafe fn cleanup(&mut self) -> Result<()> {
        if sys::xTaskGetCurrentTaskHandle() != self.owner_task
            || sys::xTaskGetCoreID(self.owner_task) != self.owner_core
        {
            return Err(failure(
                "camera teardown requires its original pinned task/core",
                sys::ESP_ERR_INVALID_STATE,
            ));
        }
        if self.sensor_started {
            self.set_sensor_stream(false)?;
            self.sensor_started = false;
        }
        if self.csi_started {
            check("quiesce DMA callbacks and stop CSI", stop::stop(self.csi))?;
            self.csi_started = false;
        }
        if self.csi_enabled {
            check("disable CSI", sys::esp_cam_ctlr_disable(self.csi))?;
            self.csi_enabled = false;
        }
        if !self.csi.is_null() {
            // Deletes DMA/error interrupts before freeing callback state/buffers.
            check("delete CSI", sys::esp_cam_ctlr_del(self.csi))?;
            self.csi = ptr::null_mut();
        }
        if self.isp_enabled {
            check("disable ISP", sys::esp_isp_disable(self.isp))?;
            self.isp_enabled = false;
        }
        if self.demosaic_enabled {
            check("disable demosaic", sys::esp_isp_demosaic_disable(self.isp))?;
            self.demosaic_enabled = false;
        }
        if !self.isp.is_null() {
            check("delete ISP", sys::esp_isp_del_processor(self.isp))?;
            self.isp = ptr::null_mut();
        }
        if !self.state.is_null() {
            // CSI deletion above has quiesced all notification producers.
            sys::ulTaskGenericNotifyTake(FRAME_NOTIFICATION_INDEX, 1, 0);
            // Drop the capture owner's Arc only after CSI deletion. Owned
            // leases retain the internal pool and PSRAM allocations independently.
            ptr::drop_in_place(self.state);
            sys::heap_caps_free(self.state.cast());
            self.state = ptr::null_mut();
        }
        if !self.sensor.is_null() {
            check("delete OV5647", sys::esp_cam_sensor_del_dev(self.sensor))?;
            self.sensor = ptr::null_mut();
        }
        if !self.sccb.is_null() {
            check("delete SCCB", sys::esp_sccb_del_i2c_io(self.sccb))?;
            self.sccb = ptr::null_mut();
        }
        if !self.i2c.is_null() {
            check("delete I2C bus", sys::i2c_del_master_bus(self.i2c))?;
            self.i2c = ptr::null_mut();
        }
        if self.xclk_started {
            check("stop XCLK", sys::esp_cam_sensor_xclk_stop(self.xclk))?;
            self.xclk_started = false;
        }
        if !self.xclk.is_null() {
            check("free XCLK", sys::esp_cam_sensor_xclk_free(self.xclk))?;
            self.xclk = ptr::null_mut();
        }
        if !self.ldo.is_null() {
            check("release MIPI LDO", sys::esp_ldo_release_channel(self.ldo))?;
            self.ldo = ptr::null_mut();
        }
        Ok(())
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.cleanup() } {
            // Raw allocations deliberately remain alive if hardware teardown
            // fails: freeing memory still reachable by DMA/ISR is unsafe.
            log::error!("Camera cleanup failed; retaining remaining resources: {error}");
        }
    }
}

unsafe extern "C" fn on_get_new_trans(
    _: sys::esp_cam_ctlr_handle_t,
    trans: *mut sys::esp_cam_ctlr_trans_t,
    user: *mut c_void,
) -> bool {
    let state = &*user.cast::<CaptureState>();
    let index = state.buffers().next_dma();
    (*trans).buffer = state.pointers[index].cast();
    (*trans).buflen = FRAME_BYTES;
    // IDF 6.1 invalidates the completed transaction using received_size BEFORE
    // overwriting it at completion. Initialize it for every new transaction.
    (*trans).received_size = FRAME_BYTES;
    false
}

unsafe extern "C" fn on_trans_finished(
    _: sys::esp_cam_ctlr_handle_t,
    trans: *mut sys::esp_cam_ctlr_trans_t,
    user: *mut c_void,
) -> bool {
    let state = &*user.cast::<CaptureState>();
    if let Some(index) = state
        .pointers
        .iter()
        .position(|p| p.cast::<c_void>() == (*trans).buffer)
    {
        if state
            .buffers()
            .finish(index, (*trans).received_size == FRAME_BYTES)
        {
            let mut higher_priority_task_woken: sys::BaseType_t = 0;
            sys::vTaskGenericNotifyGiveFromISR(
                state.notify_task,
                FRAME_NOTIFICATION_INDEX,
                &mut higher_priority_task_woken,
            );
            // Let the IDF DMA driver request a context switch after its ISR
            // finishes; never yield directly from inside this callback.
            return higher_priority_task_woken != 0;
        }
    }
    false
}

unsafe extern "C" fn on_error(
    _: sys::esp_cam_ctlr_handle_t,
    event: *const sys::esp_cam_ctlr_error_event_data_t,
    user: *mut c_void,
) -> bool {
    let state = &*user.cast::<CaptureState>();
    state
        .errors
        .fetch_or((*event).csi_host_err_evts, Ordering::Relaxed);
    false
}
