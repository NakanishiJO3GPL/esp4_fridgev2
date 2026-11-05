//! Task-context logging via ESP-IDF without the HAL/service dependencies.

use esp_idf_sys as sys;
use log::{Level, LevelFilter, Log, Metadata, Record, SetLoggerError};
use std::ffi::CString;

struct EspLogger;
static LOGGER: EspLogger = EspLogger;

pub const CAPTURE_TRIGGER_TARGET: &str = "capture_trigger";
pub const WIFI_IP_TARGET: &str = "wifi_ip";

pub fn initialize() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER)?;
    // Suppress routine IDF/driver logs while retaining fault diagnostics.
    unsafe {
        sys::esp_log_level_set(c"*".as_ptr(), sys::esp_log_level_t_ESP_LOG_WARN);
        sys::esp_log_level_set(c"fridge".as_ptr(), sys::esp_log_level_t_ESP_LOG_INFO);
    }
    log::set_max_level(LevelFilter::Info);
    Ok(())
}

fn idf_level(level: Level) -> (sys::esp_log_level_t, char) {
    match level {
        Level::Error => (sys::esp_log_level_t_ESP_LOG_ERROR, 'E'),
        Level::Warn => (sys::esp_log_level_t_ESP_LOG_WARN, 'W'),
        Level::Info => (sys::esp_log_level_t_ESP_LOG_INFO, 'I'),
        Level::Debug => (sys::esp_log_level_t_ESP_LOG_DEBUG, 'D'),
        Level::Trace => (sys::esp_log_level_t_ESP_LOG_VERBOSE, 'V'),
    }
}

impl Log for EspLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        if metadata.level() > Level::Warn
            && (metadata.level() != Level::Info
                || !matches!(metadata.target(), CAPTURE_TRIGGER_TARGET | WIFI_IP_TARGET))
        {
            return false;
        }
        let (level, _) = idf_level(metadata.level());
        level <= unsafe { sys::esp_log_level_get(c"fridge".as_ptr()) }
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let (level, prefix) = idf_level(record.level());
        let timestamp = unsafe { sys::esp_log_timestamp() };
        let text = format!("{prefix} ({timestamp}) fridge: {}\n", record.args());
        // Keep both the IDF tag and printf format static. Treat the rendered
        // message as data, escaping embedded NULs rather than truncating it.
        let message = CString::new(text.replace('\0', "\\0")).unwrap();
        unsafe {
            sys::esp_log_write(level, c"fridge".as_ptr(), c"%s".as_ptr(), message.as_ptr());
        }
    }

    fn flush(&self) {}
}
