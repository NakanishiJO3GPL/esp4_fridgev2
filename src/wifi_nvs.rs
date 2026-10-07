//! Committed boot identities. The caller must initialize NVS first.

use std::io;

// Keep the transaction and handle lifetime shared by ESP-IDF and host tests.
pub(super) trait Driver {
    type Handle: Copy;

    fn open(&mut self) -> io::Result<Self::Handle>;
    fn get_boot(&mut self, handle: Self::Handle) -> io::Result<Option<u64>>;
    fn set_boot(&mut self, handle: Self::Handle, value: u64) -> io::Result<()>;
    fn commit(&mut self, handle: Self::Handle) -> io::Result<()>;
    fn close(&mut self, handle: Self::Handle);
}

struct Handle<'a, D: Driver> {
    driver: &'a mut D,
    raw: D::Handle,
}

impl<D: Driver> Drop for Handle<'_, D> {
    fn drop(&mut self) {
        self.driver.close(self.raw);
    }
}

pub(super) fn next_boot_id_with<D: Driver>(driver: &mut D) -> io::Result<u64> {
    let raw = driver.open()?;
    let handle = Handle { driver, raw };
    let current = handle.driver.get_boot(handle.raw)?.unwrap_or(0);
    let next = current
        .checked_add(1)
        .ok_or_else(|| io::Error::other("Wi-Fi boot counter exhausted (ESP_ERR_INVALID_STATE)"))?;
    handle.driver.set_boot(handle.raw, next)?;
    handle.driver.commit(handle.raw)?;
    Ok(next)
}

/// Return a new boot identity only after its NVS commit succeeds.
#[cfg(target_os = "espidf")]
pub(super) fn next_boot_id() -> io::Result<u64> {
    next_boot_id_with(&mut esp::Nvs)
}

#[cfg(target_os = "espidf")]
mod esp {
    use super::Driver;
    use esp_idf_sys as sys;
    use std::io;

    pub(super) struct Nvs;

    fn check(operation: &str, code: sys::esp_err_t) -> io::Result<()> {
        match sys::EspError::from(code) {
            Some(error) => Err(io::Error::other(format!("{operation}: {error}"))),
            None => Ok(()),
        }
    }

    impl Driver for Nvs {
        type Handle = sys::nvs_handle_t;

        fn open(&mut self) -> io::Result<Self::Handle> {
            let mut handle = 0;
            check("open Wi-Fi boot identity", unsafe {
                sys::nvs_open(
                    b"fridge_wifi\0".as_ptr().cast(),
                    sys::nvs_open_mode_t_NVS_READWRITE,
                    &mut handle,
                )
            })?;
            Ok(handle)
        }

        fn get_boot(&mut self, handle: Self::Handle) -> io::Result<Option<u64>> {
            let mut value = 0;
            let code = unsafe { sys::nvs_get_u64(handle, b"boot\0".as_ptr().cast(), &mut value) };
            if code == sys::ESP_ERR_NVS_NOT_FOUND as sys::esp_err_t {
                return Ok(None);
            }
            check("read Wi-Fi boot identity", code)?;
            Ok(Some(value))
        }

        fn set_boot(&mut self, handle: Self::Handle, value: u64) -> io::Result<()> {
            check("set Wi-Fi boot identity", unsafe {
                sys::nvs_set_u64(handle, b"boot\0".as_ptr().cast(), value)
            })
        }

        fn commit(&mut self, handle: Self::Handle) -> io::Result<()> {
            check("commit Wi-Fi boot identity", unsafe {
                sys::nvs_commit(handle)
            })
        }

        fn close(&mut self, handle: Self::Handle) {
            unsafe { sys::nvs_close(handle) };
        }
    }
}
