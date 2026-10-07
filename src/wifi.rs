//! ESP-Hosted Wi-Fi STA and reboot-resilient browser delivery of the latest JPEG.
//!
//! Wi-Fi events only wake a reconnect worker. HTTP readers hold immutable PSRAM
//! snapshots; JPEG publication never waits for a browser or holds a network lock.

use crate::{jpeg::EncodedImage, pthread_config::PthreadConfigGuard};
use esp_idf_sys as sys;
use std::{
    ffi::{c_void, CStr},
    io, ptr, slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "wifi_store.rs"]
mod store;
use store::{Budget, Latest, Reservation};

const HOSTNAME: &CStr = c"esp4-fridge";
const PAGE: &[u8] = include_bytes!("wifi_page.html");
// Aggregate published JPEG storage, including snapshots still being sent.
const IMAGE_BUDGET: usize = 8 * 1024 * 1024;
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
static SERVICE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct Config<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
}

impl Config<'static> {
    /// Read values embedded by build.rs from the ignored local wifi.conf.
    pub fn from_build() -> io::Result<Self> {
        let config = Self {
            ssid: option_env!("FRIDGE_WIFI_SSID").unwrap_or(""),
            password: option_env!("FRIDGE_WIFI_PASSWORD").unwrap_or(""),
        };
        config.validate()?;
        Ok(config)
    }
}

impl Config<'_> {
    fn validate(&self) -> io::Result<()> {
        if self.ssid.is_empty() || self.ssid.len() > 32 || self.ssid.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Set a 1..=32-byte SSID in wifi.conf (see wifi.conf.example)",
            ));
        }
        let password = self.password.as_bytes();
        let valid = password.is_empty()
            || (8..=63).contains(&password.len())
            || (password.len() == 64 && password.iter().all(u8::is_ascii_hexdigit));
        if !valid || self.password.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid Wi-Fi password length/format",
            ));
        }
        Ok(())
    }
}

struct Shared {
    latest: Mutex<Latest<PsramImage>>,
    budget: Budget,
}

/// Can be moved into the JPEG worker. Holds no reference to encoder DMA output.
#[derive(Clone)]
pub struct JpegPublisher {
    shared: Arc<Shared>,
}

impl JpegPublisher {
    /// Record detection independently of JPEG submission/publication, including
    /// busy/skipped triggers. No image allocation or network send occurs here.
    pub fn notify_trigger(&self, frame_sequence: u32) {
        self.shared
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_trigger(frame_sequence);
    }

    /// Copy a completed JPEG into owned PSRAM and publish an immutable version.
    /// On allocation/budget failure the previous image remains available.
    pub fn publish(&self, image: &EncodedImage<'_>) -> io::Result<u32> {
        if !image.data.starts_with(&[0xff, 0xd8]) || !image.data.ends_with(&[0xff, 0xd9]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid JPEG markers",
            ));
        }
        let data = PsramImage::copy(image.data, &self.shared.budget)?;
        let mut latest = self
            .shared
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(latest.publish(image.trigger.frame_sequence, data))
    }
}

struct PsramImage {
    pointer: *mut u8,
    length: usize,
    _reservation: Reservation,
}

// Initialized before publication and never mutated afterwards. Arc readers keep
// the allocation alive, and its reservation bounds memory even for slow clients.
unsafe impl Send for PsramImage {}
unsafe impl Sync for PsramImage {}

impl PsramImage {
    fn copy(bytes: &[u8], budget: &Budget) -> io::Result<Self> {
        let reservation = budget.reserve(bytes.len()).ok_or_else(|| {
            io::Error::other("JPEG publication budget exhausted; previous image retained")
        })?;
        let pointer = unsafe {
            sys::heap_caps_malloc(bytes.len(), sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT)
        }
        .cast::<u8>();
        if pointer.is_null() {
            return Err(io::Error::other("Cannot allocate published JPEG in PSRAM"));
        }
        // Initialize raw storage before constructing any Rust byte slice.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
        Ok(Self {
            pointer,
            length: bytes.len(),
            _reservation: reservation,
        })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.pointer, self.length) }
    }
}

impl Drop for PsramImage {
    fn drop(&mut self) {
        unsafe { sys::heap_caps_free(self.pointer.cast()) };
    }
}

struct Station {
    started: AtomicBool,
    connected: AtomicBool,
    stopping: AtomicBool,

    wake: SyncSender<()>,
}

/// Keep alive for the duration of capture. Drop stops HTTP/Wi-Fi and the worker.
/// Startup does not wait for an AP: camera operation can proceed while STA retries.
pub struct WifiService {
    shared: Arc<Shared>,
    station: Arc<Station>,
    worker: Option<JoinHandle<()>>,
    netif: *mut sys::esp_netif_t,
    server: sys::httpd_handle_t,
    wifi_handler: sys::esp_event_handler_instance_t,
    ip_handler: sys::esp_event_handler_instance_t,
    wifi_initialized: bool,
    initialization_uncertain: bool,
    wifi_started: bool,
    mdns_initialized: bool,
}

impl WifiService {
    pub fn publisher(&self) -> JpegPublisher {
        JpegPublisher {
            shared: self.shared.clone(),
        }
    }
}

pub fn start(config: Config<'_>) -> io::Result<WifiService> {
    config.validate()?;
    if SERVICE_ACTIVE.swap(true, Ordering::AcqRel) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Wi-Fi service already active",
        ));
    }
    let (wake, events) = sync_channel(1);

    let mut service = WifiService {
        shared: Arc::new(Shared {
            latest: Mutex::new(Latest::new(0)),
            budget: Budget::new(IMAGE_BUDGET),
        }),
        station: Arc::new(Station {
            started: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            stopping: AtomicBool::new(false),

            wake,
        }),
        worker: None,
        netif: ptr::null_mut(),
        server: ptr::null_mut(),
        wifi_handler: ptr::null_mut(),
        ip_handler: ptr::null_mut(),
        wifi_initialized: false,
        initialization_uncertain: false,
        wifi_started: false,
        mdns_initialized: false,
    };
    unsafe {
        // Never erase NVS automatically: it can contain unrelated application data.
        check(
            "initialize NVS (repair incompatible NVS manually)",
            sys::nvs_flash_init(),
        )?;
        let mut boot_id = 0;
        check(
            "commit Wi-Fi boot identity",
            sys::fridge_wifi_next_boot_id(&mut boot_id),
        )?;
        service
            .shared
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .boot_id = boot_id;
        let code = sys::esp_netif_init();
        if code != sys::ESP_ERR_INVALID_STATE {
            check("initialize netif", code)?;
        }
        let code = sys::esp_event_loop_create_default();
        if code != sys::ESP_ERR_INVALID_STATE {
            check("create default event loop", code)?;
        }
        service.netif = sys::esp_netif_create_default_wifi_sta();
        if service.netif.is_null() {
            return Err(io::Error::other("Cannot create Wi-Fi STA netif"));
        }
        check(
            "set STA hostname",
            sys::esp_netif_set_hostname(service.netif, HOSTNAME.as_ptr()),
        )?;
        // Hosted allocates host channels before its initialization RPC. If that
        // RPC fails, keep the netif and prohibit a second start until reboot.
        service.initialization_uncertain = true;
        check("initialize Hosted Wi-Fi", sys::fridge_wifi_init())?;
        service.initialization_uncertain = false;
        service.wifi_initialized = true;
        check(
            "use volatile Wi-Fi credentials",
            sys::fridge_wifi_set_storage_ram(),
        )?;
        check(
            "set Wi-Fi STA mode",
            sys::esp_wifi_set_mode(sys::wifi_mode_t_WIFI_MODE_STA),
        )?;
        let mut wifi_config = sys::wifi_config_t::default();
        let sta = &mut wifi_config.sta;
        sta.ssid[..config.ssid.len()].copy_from_slice(config.ssid.as_bytes());
        sta.password[..config.password.len()].copy_from_slice(config.password.as_bytes());
        sta.threshold.authmode = if config.password.is_empty() {
            sys::wifi_auth_mode_t_WIFI_AUTH_OPEN
        } else {
            sys::wifi_auth_mode_t_WIFI_AUTH_WPA2_PSK
        };
        sta.sae_pwe_h2e = sys::wifi_sae_pwe_method_t_WPA3_SAE_PWE_BOTH;
        check(
            "set Wi-Fi credentials",
            sys::esp_wifi_set_config(sys::wifi_interface_t_WIFI_IF_STA, &mut wifi_config),
        )?;
        let context = Arc::as_ptr(&service.station).cast_mut().cast::<c_void>();
        check(
            "register Wi-Fi events",
            sys::esp_event_handler_instance_register(
                sys::WIFI_EVENT,
                -1,
                Some(station_event),
                context,
                &mut service.wifi_handler,
            ),
        )?;
        check(
            "register IP events",
            sys::esp_event_handler_instance_register(
                sys::IP_EVENT,
                sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32,
                Some(station_event),
                context,
                &mut service.ip_handler,
            ),
        )?;
        check("initialize mDNS", sys::mdns_init())?;
        service.mdns_initialized = true;
        check(
            "set mDNS hostname",
            sys::mdns_hostname_set(HOSTNAME.as_ptr()),
        )?;
        check(
            "set mDNS instance",
            sys::mdns_instance_name_set(c"ESP32-P4 camera".as_ptr()),
        )?;
        check(
            "advertise HTTP",
            sys::mdns_service_add(
                ptr::null(),
                c"_http".as_ptr(),
                c"_tcp".as_ptr(),
                80,
                ptr::null_mut(),
                0,
            ),
        )?;
        check(
            "start HTTP server",
            sys::fridge_httpd_start(&mut service.server),
        )?;
        for (uri, handler) in [
            (
                c"/",
                page_handler as unsafe extern "C" fn(*mut sys::httpd_req_t) -> sys::esp_err_t,
            ),
            (c"/status", status_handler),
            (c"/image.jpg", image_handler),
            (c"/latest.jpg", latest_handler),
        ] {
            let definition = sys::httpd_uri_t {
                uri: uri.as_ptr(),
                method: sys::http_method_HTTP_GET,
                handler: Some(handler),
                user_ctx: Arc::as_ptr(&service.shared).cast_mut().cast(),
            };
            check(
                "register HTTP endpoint",
                sys::httpd_register_uri_handler(service.server, &definition),
            )?;
        }
    }
    let station = service.station.clone();
    let _thread_config = PthreadConfigGuard::configure(1, 3, 8192, c"wifi_reconnect")?;
    service.worker = Some(
        thread::Builder::new()
            .name("wifi_reconnect".into())
            .stack_size(8192)
            .spawn(move || {
                let mut last_attempt: Option<Instant> = None;
                loop {
                    let _ = events.recv_timeout(Duration::from_secs(1));
                    if station.stopping.load(Ordering::Acquire) {
                        break;
                    }

                    if !station.started.load(Ordering::Acquire)
                        || station.connected.load(Ordering::Acquire)
                    {
                        continue;
                    }
                    if last_attempt.is_some_and(|time| time.elapsed() < RETRY_INTERVAL) {
                        continue;
                    }
                    last_attempt = Some(Instant::now());

                    // Hosted RPC may block; do it here, never on the event-loop/timer task.
                    let code = unsafe { sys::esp_wifi_connect() };
                    if let Err(error) = check("connect Wi-Fi STA (will retry)", code) {
                        log::warn!("{error}");
                    }
                }
            })?,
    );
    check("start Wi-Fi STA", unsafe { sys::esp_wifi_start() })?;
    service.wifi_started = true;
    // Keep the C6 awake while serving large JPEGs; DTIM sleep adds latency to
    // TCP ACK reception with the P4's small send window.
    check("disable Wi-Fi modem sleep", unsafe {
        sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE)
    })?;

    Ok(service)
}

unsafe extern "C" fn station_event(
    arg: *mut c_void,
    base: sys::esp_event_base_t,
    id: i32,
    data: *mut c_void,
) {
    let station = &*arg.cast::<Station>();
    if station.stopping.load(Ordering::Acquire) {
        return;
    }
    if base == sys::WIFI_EVENT {
        match id as u32 {
            sys::wifi_event_t_WIFI_EVENT_STA_START => {
                station.started.store(true, Ordering::Release);
                station.connected.store(false, Ordering::Release);
            }
            sys::wifi_event_t_WIFI_EVENT_STA_CONNECTED => {
                station.connected.store(true, Ordering::Release);
            }
            sys::wifi_event_t_WIFI_EVENT_STA_DISCONNECTED => {
                station.connected.store(false, Ordering::Release);
            }
            sys::wifi_event_t_WIFI_EVENT_STA_STOP => {
                station.started.store(false, Ordering::Release);
            }
            _ => {}
        }
        let _ = station.wake.try_send(());
    } else if base == sys::IP_EVENT
        && id == sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32
        && !data.is_null()
    {
        let _ = station.wake.try_send(());
    }
}

unsafe extern "C" fn page_handler(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
    respond(request, c"200 OK", c"text/html; charset=utf-8", PAGE)
}

unsafe extern "C" fn status_handler(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
    let shared = &*(*request).user_ctx.cast::<Shared>();
    let (boot_id, snapshot) = {
        let latest = shared
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (latest.boot_id, latest.snapshot())
    };
    let Some(snapshot) = snapshot else {
        return respond(
            request,
            c"503 Service Unavailable",
            c"text/plain",
            b"Waiting for first triggered JPEG",
        );
    };
    let body = format!(
        "{{\"boot_id\":\"{boot_id:016x}\",\"version\":{},\"frame_sequence\":{},\"bytes\":{}}}",
        snapshot.version, snapshot.frame_sequence, snapshot.data.length,
    );
    respond(request, c"200 OK", c"application/json", body.as_bytes())
}

unsafe extern "C" fn image_handler(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
    serve_image(request, false)
}

unsafe extern "C" fn latest_handler(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
    serve_image(request, true)
}

unsafe fn serve_image(request: *mut sys::httpd_req_t, latest_mode: bool) -> sys::esp_err_t {
    let length = sys::httpd_req_get_url_query_len(request);
    let mut query = [0 as std::ffi::c_char; 128];
    if (length == 0 && !latest_mode)
        || length >= query.len()
        || (length > 0
            && sys::httpd_req_get_url_query_str(request, query.as_mut_ptr(), query.len())
                != sys::ESP_OK)
    {
        return respond(
            request,
            c"400 Bad Request",
            c"text/plain",
            b"Expected boot and version query parameters",
        );
    }
    let Ok(query) = CStr::from_ptr(query.as_ptr()).to_str() else {
        return respond(request, c"400 Bad Request", c"text/plain", b"Invalid query");
    };
    let mut boot = None;
    let mut version = None;
    for pair in query.split('&').filter(|_| length > 0) {
        match pair.split_once('=') {
            Some(("boot", value)) if boot.is_none() => {
                boot = u64::from_str_radix(value, 16).ok();
            }
            Some(("version", value)) if version.is_none() => {
                version = value.parse::<u32>().ok();
            }
            _ => {
                return respond(
                    request,
                    c"400 Bad Request",
                    c"text/plain",
                    b"Invalid query parameters",
                )
            }
        }
    }
    if (length > 0 || !latest_mode) && (boot.is_none() || version.is_none()) {
        return respond(
            request,
            c"400 Bad Request",
            c"text/plain",
            b"Invalid image identity",
        );
    }
    let shared = &*(*request).user_ctx.cast::<Shared>();
    let (boot_id, snapshot, trigger_header) = {
        let latest = shared
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (latest.boot_id, latest.snapshot(), latest.trigger_header())
    };
    // IDF retains header-value pointers until the synchronous send finishes.
    // This adds one header: JPEG responses use eight, within IDF's default limit.
    let trigger_header = std::ffi::CString::new(trigger_header).expect("numeric trigger header");
    if latest_mode {
        let code = sys::httpd_resp_set_hdr(
            request,
            c"X-Fridge-Trigger".as_ptr(),
            trigger_header.as_ptr(),
        );
        if code != sys::ESP_OK {
            log::warn!("HTTP trigger header failed: error=0x{code:x}");
            return code;
        }
    }
    let Some(snapshot) = snapshot else {
        return respond(
            request,
            c"503 Service Unavailable",
            c"text/plain",
            b"Waiting for JPEG",
        );
    };
    if latest_mode {
        if boot == Some(boot_id) && version == Some(snapshot.version) {
            return respond(request, c"204 No Content", c"text/plain", b"");
        }
        // Identity and JPEG belong to the same Arc, even if publication races
        // with the download. Header values must stay alive through respond().
        let headers = [
            (c"X-Fridge-Boot", format!("{boot_id:016x}")),
            (c"X-Fridge-Version", snapshot.version.to_string()),
            (c"X-Fridge-Frame", snapshot.frame_sequence.to_string()),
            (c"X-Fridge-Bytes", snapshot.data.length.to_string()),
        ];
        let values = headers
            .each_ref()
            .map(|(_, value)| std::ffi::CString::new(value.as_str()).expect("numeric HTTP header"));
        for ((name, _), value) in headers.iter().zip(&values) {
            let code = sys::httpd_resp_set_hdr(request, name.as_ptr(), value.as_ptr());
            if code != sys::ESP_OK {
                log::warn!("HTTP image identity header failed: error=0x{code:x}");
                return code;
            }
        }
        return respond(request, c"200 OK", c"image/jpeg", snapshot.data.bytes());
    }
    if boot != Some(boot_id) || version != Some(snapshot.version) {
        return respond(
            request,
            c"409 Conflict",
            c"text/plain",
            b"Image changed; refresh status",
        );
    }
    // No store lock while sending. The Arc preserves these exact bytes across updates.
    respond(request, c"200 OK", c"image/jpeg", snapshot.data.bytes())
}

unsafe fn respond(
    request: *mut sys::httpd_req_t,
    status: &CStr,
    content_type: &CStr,
    bytes: &[u8],
) -> sys::esp_err_t {
    let socket = sys::httpd_req_to_sockfd(request);
    let started = Instant::now();
    let is_jpeg = content_type == c"image/jpeg";

    let code = sys::fridge_httpd_prepare_response(request);
    if code != sys::ESP_OK {
        log::warn!("HTTP response prepare failed: socket={socket}, error=0x{code:x}");
        return code;
    }
    for code in [
        sys::httpd_resp_set_status(request, status.as_ptr()),
        sys::httpd_resp_set_type(request, content_type.as_ptr()),
        sys::httpd_resp_set_hdr(
            request,
            c"Cache-Control".as_ptr(),
            c"no-store, max-age=0".as_ptr(),
        ),
        sys::httpd_resp_set_hdr(
            request,
            c"X-Content-Type-Options".as_ptr(),
            c"nosniff".as_ptr(),
        ),
        sys::httpd_resp_set_hdr(request, c"Connection".as_ptr(), c"close".as_ptr()),
    ] {
        if code != sys::ESP_OK {
            log::warn!("HTTP response header failed: socket={socket}, error=0x{code:x}");
            return code;
        }
    }

    let code = sys::httpd_resp_send(request, bytes.as_ptr().cast(), bytes.len() as isize);
    if code != sys::ESP_OK {
        log::warn!(
            "HTTP {} send failed: {} bytes, {:.2}s, error=0x{:x}",
            if is_jpeg { "JPEG" } else { "response" },
            bytes.len(),
            started.elapsed().as_secs_f64(),
            code,
        );
        return code;
    }

    let code = sys::fridge_httpd_finish_response(request);
    if code != sys::ESP_OK {
        log::warn!("HTTP response close failed: socket={socket}, error=0x{code:x}");
    }
    code
}

impl Drop for WifiService {
    fn drop(&mut self) {
        self.station.stopping.store(true, Ordering::Release);
        let _ = self.station.wake.try_send(());
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                log::error!("Wi-Fi reconnect worker panicked");
            }
        }
        let mut retained = self.initialization_uncertain;
        if retained {
            log::error!("Hosted initialization state uncertain; restart requires reboot");
        }
        unsafe {
            if !self.server.is_null() {
                if let Err(error) = check("stop HTTP", sys::httpd_stop(self.server)) {
                    log::error!("{error}; retaining HTTP context");
                    std::mem::forget(self.shared.clone());
                    retained = true;
                }
            }
            if self.mdns_initialized {
                sys::mdns_free();
            }
            for (base, id, handler) in [
                (sys::WIFI_EVENT, -1, self.wifi_handler),
                (
                    sys::IP_EVENT,
                    sys::ip_event_t_IP_EVENT_STA_GOT_IP as i32,
                    self.ip_handler,
                ),
            ] {
                if !handler.is_null() {
                    if let Err(error) = check(
                        "unregister Wi-Fi event",
                        sys::esp_event_handler_instance_unregister(base, id, handler),
                    ) {
                        log::error!("{error}; retaining event context");
                        std::mem::forget(self.station.clone());
                        retained = true;
                    }
                }
            }
            let mut wifi_quiescent = !self.initialization_uncertain;
            if self.wifi_started {
                if let Err(error) = check("stop Wi-Fi", sys::esp_wifi_stop()) {
                    log::error!("{error}");
                    wifi_quiescent = false;
                }
            }
            if self.wifi_initialized && wifi_quiescent {
                if let Err(error) = check("deinitialize Wi-Fi", sys::esp_wifi_deinit()) {
                    log::error!("{error}");
                    wifi_quiescent = false;
                }
            }
            if wifi_quiescent && !self.netif.is_null() {
                sys::esp_netif_destroy_default_wifi(self.netif.cast());
            }
            retained |= !wifi_quiescent;
        }
        // Hosted owns the global event loop/netif subsystem; do not delete them.
        if !retained {
            SERVICE_ACTIVE.store(false, Ordering::Release);
        }
    }
}

fn check(operation: &'static str, code: sys::esp_err_t) -> io::Result<()> {
    match sys::EspError::from(code) {
        Some(error) => Err(io::Error::other(format!("{operation}: {error}"))),
        None => Ok(()),
    }
}
