//! ESP-IDF 6.1 CSI teardown using generated private-driver bindings.

#[cfg(test)]
use crate::fake_idf as sys;
#[cfg(not(test))]
use esp_idf_sys as sys;
use std::{mem::offset_of, ptr};

const _: () = assert!(
    sys::ESP_IDF_VERSION_MAJOR == 6
        && sys::ESP_IDF_VERSION_MINOR == 1
        && sys::CONFIG_IDF_TARGET_ESP32P4 == 1
        && sys::CONFIG_FREERTOS_NUMBER_OF_CORES == 2,
    "Review CSI teardown against this ESP-IDF version and FreeRTOS port"
);

struct CriticalSection<'a> {
    lock: &'a mut sys::portMUX_TYPE,
}

impl<'a> CriticalSection<'a> {
    unsafe fn enter(lock: &'a mut sys::portMUX_TYPE) -> Option<Self> {
        // The P4 RISC-V port's vPortEnterCriticalMultiCore inline calls this
        // function with portMUX_NO_TIMEOUT. Do not use the no-spinlock variant.
        // pdTRUE is ((BaseType_t) 1) in this port; bindgen omits that cast macro.
        if sys::xPortEnterCriticalTimeout(lock, sys::portMUX_NO_TIMEOUT) == 1 {
            Some(Self { lock })
        } else {
            None
        }
    }
}

impl Drop for CriticalSection<'_> {
    fn drop(&mut self) {
        unsafe { sys::vPortExitCriticalMultiCore(self.lock) };
    }
}

/// Mask DMA callbacks and stop CSI without allowing a pending IRQ to rearm DMA.
///
/// # Safety
/// `handle` must be a live CSI controller created on this same pinned task/core.
/// Call only from task context after CSI start, before disable/delete. Callback
/// registration must already have installed the DMA interrupt, so clearing the
/// callbacks inside the critical section cannot allocate or block. The caller
/// must retain all DMA/ISR-visible memory if teardown reports an error.
pub(super) unsafe fn stop(handle: sys::esp_cam_ctlr_handle_t) -> sys::esp_err_t {
    if handle.is_null() {
        return sys::ESP_ERR_INVALID_ARG as sys::esp_err_t;
    }
    // container_of(handle, csi_controller_t, base), using the actual SDK layout.
    // Never create &mut to the controller: its other fields are shared with ISR.
    let controller = handle
        .cast::<u8>()
        .sub(offset_of!(sys::csi_controller_t, base))
        .cast::<sys::csi_controller_t>();
    let mut callbacks = sys::dw_gdma_event_callbacks_t {
        on_block_trans_done: None,
        on_full_trans_done: None,
        on_invalid_block: None,
    };
    let mut lock = sys::portMUX_TYPE {
        owner: sys::portMUX_FREE_VAL,
        count: 0,
    };
    let Some(_critical) = CriticalSection::enter(&mut lock) else {
        return sys::ESP_FAIL;
    };
    let channel = ptr::addr_of!((*controller).dma_chan).read();
    let result =
        sys::dw_gdma_channel_register_event_callbacks(channel, &mut callbacks, ptr::null_mut());
    if result == sys::ESP_OK {
        sys::esp_cam_ctlr_stop(handle)
    } else {
        result
    }
}
