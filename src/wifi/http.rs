//! Synchronous ESP-IDF HTTP configuration and bounded socket sends.
//!
//! Each HTTP task serves one response at a time. Thread-local state prevents
//! different server tasks sharing a deadline; asynchronous handlers are not used.

const RESPONSE_TIMEOUT_US: i64 = 10_000_000;
const SEND_CHUNK_BYTES: usize = 4096;

#[derive(Clone, Copy)]
struct SendBudget {
    deadline_us: i64,
    queued: usize,
}

impl SendBudget {
    fn new(now_us: i64) -> Self {
        Self {
            deadline_us: now_us.saturating_add(RESPONSE_TIMEOUT_US),
            queued: 0,
        }
    }

    fn send_length(&self, now_us: i64, requested: usize) -> Option<usize> {
        (now_us < self.deadline_us).then_some(requested.min(SEND_CHUNK_BYTES))
    }

    fn record(&mut self, sent: usize) {
        self.queued = self.queued.saturating_add(sent);
    }
}

#[cfg(target_os = "espidf")]
mod idf {
    use super::super::defaults;
    use super::SendBudget;
    use esp_idf_sys as sys;
    use std::{cell::Cell, ffi::c_char};

    thread_local! {
        static SEND_BUDGET: Cell<Option<SendBudget>> = const { Cell::new(None) };
    }

    pub(crate) unsafe fn start(server: *mut sys::httpd_handle_t) -> sys::esp_err_t {
        let mut config = defaults::http_config();
        config.core_id = 1;
        config.task_priority = 4;
        config.stack_size = 8192;
        config.max_open_sockets = 3;
        config.max_uri_handlers = 4;
        config.lru_purge_enable = true;
        config.recv_wait_timeout = 3;
        config.send_wait_timeout = 3;
        sys::httpd_start(server, &config)
    }

    pub(crate) unsafe fn prepare_response(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
        let socket = sys::httpd_req_to_sockfd(request);
        let enabled: i32 = 1;
        if sys::lwip_setsockopt(
            socket,
            sys::IPPROTO_TCP as i32,
            sys::TCP_NODELAY as i32,
            (&enabled as *const i32).cast(),
            std::mem::size_of_val(&enabled) as sys::socklen_t,
        ) != 0
        {
            log::warn!(
                "Cannot enable TCP_NODELAY: {}",
                std::io::Error::last_os_error()
            );
            return sys::ESP_FAIL;
        }
        SEND_BUDGET.with(|state| state.set(Some(SendBudget::new(sys::esp_timer_get_time()))));
        sys::httpd_sess_set_send_override((*request).handle, socket, Some(deadline_send))
    }

    unsafe extern "C" fn deadline_send(
        _: sys::httpd_handle_t,
        socket: i32,
        data: *const c_char,
        length: usize,
        flags: i32,
    ) -> i32 {
        let Some(mut budget) = SEND_BUDGET.with(Cell::get) else {
            return sys::HTTPD_SOCK_ERR_FAIL;
        };
        let Some(length) = budget.send_length(sys::esp_timer_get_time(), length) else {
            log::warn!("HTTP send deadline: {} bytes queued", budget.queued);
            return sys::HTTPD_SOCK_ERR_TIMEOUT;
        };
        let result = sys::lwip_send(socket, data.cast(), length, flags);
        // Capture errno before logging or other runtime calls can change it.
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if result > 0 {
            budget.record(result as usize);
            SEND_BUDGET.with(|state| state.set(Some(budget)));
            return result as i32;
        }
        log::warn!(
            "HTTP send failed: {} bytes queued, result={}, errno={}",
            budget.queued,
            result,
            errno,
        );
        if result < 0 && (errno == sys::EAGAIN as i32 || errno == sys::EWOULDBLOCK as i32) {
            sys::HTTPD_SOCK_ERR_TIMEOUT
        } else {
            sys::HTTPD_SOCK_ERR_FAIL
        }
    }

    pub(crate) unsafe fn finish_response(request: *mut sys::httpd_req_t) -> sys::esp_err_t {
        // Connection: close must also close the actual HTTP session.
        sys::httpd_sess_trigger_close((*request).handle, sys::httpd_req_to_sockfd(request))
    }
}

#[cfg(target_os = "espidf")]
pub(super) use idf::{finish_response, prepare_response, start};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_has_ten_second_absolute_deadline() {
        let budget = SendBudget::new(100);
        assert_eq!(budget.send_length(100, 1), Some(1));
        assert_eq!(budget.send_length(10_000_099, 1), Some(1));
        assert_eq!(budget.send_length(10_000_100, 1), None);
        assert_eq!(budget.send_length(10_000_101, 1), None);
    }

    #[test]
    fn sends_are_capped_without_growing_small_writes() {
        let budget = SendBudget::new(0);
        for length in [0, 1, 100, 4095, 4096] {
            assert_eq!(budget.send_length(0, length), Some(length));
        }
        assert_eq!(budget.send_length(0, 4097), Some(4096));
        assert_eq!(budget.send_length(0, usize::MAX), Some(4096));
    }

    #[test]
    fn partial_sends_accumulate_without_extending_deadline() {
        let mut budget = SendBudget::new(100);
        budget.record(1000);
        budget.record(17);
        assert_eq!(budget.queued, 1017);
        assert_eq!(budget.send_length(10_000_100, 4096), None);
    }

    #[test]
    fn each_response_resets_budget_and_deadline() {
        let mut budget = SendBudget::new(0);
        budget.record(4096);
        budget = SendBudget::new(10_000_000);
        assert_eq!(budget.queued, 0);
        assert_eq!(budget.send_length(10_000_000, 4096), Some(4096));
    }
}
