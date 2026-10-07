//! Rust equivalents of ESP-IDF 6.1's Wi-Fi and HTTP default initializers.
//!
//! Use bindgen's evaluated header constants instead of duplicating Kconfig logic.
//! Exhaustive struct initializers reject newly added fields at compile time.

use esp_idf_sys as sys;
use std::ptr;

const _: () = assert!(
    sys::ESP_IDF_VERSION_MAJOR == 6 && sys::ESP_IDF_VERSION_MINOR == 1,
    "Review network default initializers against the new ESP-IDF headers"
);

/// WIFI_INIT_CONFIG_DEFAULT(), including the driver's OS/crypto callbacks.
pub(super) fn wifi_config() -> sys::wifi_init_config_t {
    sys::wifi_init_config_t {
        osi_funcs: ptr::addr_of_mut!(sys::g_wifi_osi_funcs),
        // This immutable table is exported and initialized by the IDF driver.
        wpa_crypto_funcs: unsafe { sys::g_wifi_default_wpa_crypto_funcs },
        static_rx_buf_num: sys::CONFIG_ESP_WIFI_STATIC_RX_BUFFER_NUM as i32,
        dynamic_rx_buf_num: sys::CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM as i32,
        tx_buf_type: sys::CONFIG_ESP_WIFI_TX_BUFFER_TYPE as i32,
        static_tx_buf_num: sys::WIFI_STATIC_TX_BUFFER_NUM as i32,
        dynamic_tx_buf_num: sys::WIFI_DYNAMIC_TX_BUFFER_NUM as i32,
        rx_mgmt_buf_type: sys::CONFIG_ESP_WIFI_DYNAMIC_RX_MGMT_BUF as i32,
        rx_mgmt_buf_num: sys::WIFI_RX_MGMT_BUF_NUM_DEF as i32,
        cache_tx_buf_num: sys::WIFI_CACHE_TX_BUFFER_NUM as i32,
        csi_enable: sys::WIFI_CSI_ENABLED as i32,
        ampdu_rx_enable: sys::WIFI_AMPDU_RX_ENABLED as i32,
        ampdu_tx_enable: sys::WIFI_AMPDU_TX_ENABLED as i32,
        amsdu_tx_enable: sys::WIFI_AMSDU_TX_ENABLED as i32,
        nvs_enable: sys::WIFI_NVS_ENABLED as i32,
        nano_enable: sys::WIFI_NANO_FORMAT_ENABLED as i32,
        rx_ba_win: sys::WIFI_DEFAULT_RX_BA_WIN as i32,
        wifi_task_core_id: sys::WIFI_TASK_CORE_ID as i32,
        beacon_max_len: sys::WIFI_SOFTAP_BEACON_MAX_LEN as i32,
        mgmt_sbuf_num: sys::WIFI_MGMT_SBUF_NUM as i32,
        feature_caps: sys::WIFI_FEATURE_CAPS as u64,
        sta_disconnected_pm: sys::WIFI_STA_DISCONNECTED_PM_ENABLED != 0,
        espnow_max_encrypt_num: sys::CONFIG_ESP_WIFI_ESPNOW_MAX_ENCRYPT_NUM as i32,
        tx_hetb_queue_num: sys::WIFI_TX_HETB_QUEUE_NUM as i32,
        dump_hesigb_enable: sys::WIFI_DUMP_HESIGB_ENABLED != 0,
        privacy_enhancements: sys::WIFI_PRIVACY_ENHANCEMENTS_ENABLED != 0,
        rmac_auto_reset_int: sys::WIFI_RMAC_AUTO_RESET_INTERVAL as u8,
        magic: sys::WIFI_INIT_CONFIG_MAGIC as i32,
    }
}

/// HTTPD_DEFAULT_CONFIG(); application overrides are applied in wifi_http.rs.
pub(super) fn http_config() -> sys::httpd_config_t {
    sys::httpd_config_t {
        task_priority: 5,
        stack_size: 4096,
        core_id: sys::FRIDGE_TSK_NO_AFFINITY as sys::BaseType_t,
        task_caps: sys::MALLOC_CAP_INTERNAL | sys::MALLOC_CAP_8BIT,
        max_req_hdr_len: sys::CONFIG_HTTPD_MAX_REQ_HDR_LEN as usize,
        max_uri_len: sys::CONFIG_HTTPD_MAX_URI_LEN as usize,
        server_port: 80,
        ctrl_port: sys::ESP_HTTPD_DEF_CTRL_PORT as u16,
        max_open_sockets: 7,
        max_uri_handlers: 8,
        max_resp_headers: 8,
        backlog_conn: 5,
        lru_purge_enable: false,
        recv_wait_timeout: 5,
        send_wait_timeout: 5,
        global_user_ctx: ptr::null_mut(),
        global_user_ctx_free_fn: None,
        global_transport_ctx: ptr::null_mut(),
        global_transport_ctx_free_fn: None,
        enable_so_linger: false,
        linger_timeout: 0,
        keep_alive_enable: false,
        keep_alive_idle: 0,
        keep_alive_interval: 0,
        keep_alive_count: 0,
        if_name: ptr::null_mut(),
        open_fn: None,
        close_fn: None,
        uri_match_fn: None,
    }
}
