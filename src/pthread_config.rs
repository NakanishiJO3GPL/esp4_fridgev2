//! Temporarily configure ESP-IDF pthread creation on the calling task.

use esp_idf_sys as sys;
use std::{ffi::CStr, io};

pub(crate) struct PthreadConfigGuard {
    previous: sys::esp_pthread_cfg_t,
}

impl PthreadConfigGuard {
    pub(crate) fn configure(
        core: i32,
        priority: usize,
        stack_bytes: usize,
        name: &'static CStr,
    ) -> io::Result<Self> {
        let mut previous = unsafe { sys::esp_pthread_get_default_config() };
        let status = unsafe { sys::esp_pthread_get_cfg(&mut previous) };
        match status {
            sys::ESP_OK => {}
            sys::ESP_ERR_NOT_FOUND => {
                // get_cfg zeroes its output on NOT_FOUND. The public API cannot
                // clear an unset configuration; restore its effective defaults.
                previous = unsafe { sys::esp_pthread_get_default_config() };
            }
            code => check("get caller pthread configuration", code)?,
        }
        let mut config = unsafe { sys::esp_pthread_get_default_config() };
        config.stack_size = stack_bytes;
        config.stack_alloc_caps = sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT;
        config.prio = priority;
        config.pin_to_core = core;
        config.thread_name = name.as_ptr();
        config.inherit_cfg = false;
        check("configure worker pthread", unsafe {
            sys::esp_pthread_set_cfg(&config)
        })?;
        Ok(Self { previous })
    }
}

impl Drop for PthreadConfigGuard {
    fn drop(&mut self) {
        if let Err(error) = check("restore caller pthread configuration", unsafe {
            sys::esp_pthread_set_cfg(&self.previous)
        }) {
            log::error!("{error}");
        }
    }
}

fn check(operation: &'static str, code: sys::esp_err_t) -> io::Result<()> {
    match sys::EspError::from(code) {
        Some(source) => Err(io::Error::other(format!("{operation}: {source}"))),
        None => Ok(()),
    }
}
