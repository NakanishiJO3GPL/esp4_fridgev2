#include "esp_wifi.h"
#include "esp_http_server.h"
#include "esp_timer.h"
#include "esp_log.h"
#include "eh_host_wifi.h"
#include "nvs.h"
#include <sys/socket.h>
#include <netinet/tcp.h>
#include <errno.h>

// Use IDF defaults, including Wi-Fi OS function pointers and configuration magic.
esp_err_t fridge_wifi_init(void)
{
    wifi_init_config_t config = WIFI_INIT_CONFIG_DEFAULT();
    return esp_wifi_init(&config);
}

// Hosted 3.0.9's generic storage hook ignores its argument; use its real RPC.
esp_err_t fridge_wifi_set_storage_ram(void)
{
    return eh_host_wifi_set_storage(WIFI_STORAGE_RAM);
}

// A committed boot counter guarantees a new image identity after normal reboot,
// without assuming that the C6 radio provides entropy to the P4 RNG.
esp_err_t fridge_wifi_next_boot_id(uint64_t *boot_id)
{
    nvs_handle_t handle;
    esp_err_t result = nvs_open("fridge_wifi", NVS_READWRITE, &handle);
    if (result != ESP_OK) {
        return result;
    }
    uint64_t value = 0;
    result = nvs_get_u64(handle, "boot", &value);
    if (result == ESP_ERR_NVS_NOT_FOUND) {
        result = ESP_OK;
    }
    if (result == ESP_OK && value == UINT64_MAX) {
        result = ESP_ERR_INVALID_STATE;
    }
    if (result == ESP_OK) {
        result = nvs_set_u64(handle, "boot", ++value);
    }
    if (result == ESP_OK) {
        result = nvs_commit(handle);
    }
    nvs_close(handle);
    if (result == ESP_OK) {
        *boot_id = value;
    }
    return result;
}

// All responses run on this service's single HTTP task. A custom send callback
// bounds the total response time, not just each individual socket operation.
static int64_t send_deadline_us;
static size_t sent_bytes;

static int deadline_send(httpd_handle_t handle, int socket, const char *data, size_t length, int flags)
{
    (void)handle;
    if (esp_timer_get_time() >= send_deadline_us) {
        ESP_LOGW("fridge_http", "HTTP send deadline: %u bytes queued", (unsigned)sent_bytes);
        return HTTPD_SOCK_ERR_TIMEOUT;
    }
    if (length > 4096) {
        length = 4096;
    }

    int result = send(socket, data, length, flags);
    int saved_errno = errno;

    if (result > 0) {
        sent_bytes += result;

        return result;
    }
    ESP_LOGW("fridge_http", "HTTP send failed: %u bytes queued, result=%d, errno=%d",
             (unsigned)sent_bytes, result, saved_errno);
    return result < 0 && (saved_errno == EAGAIN || saved_errno == EWOULDBLOCK)
        ? HTTPD_SOCK_ERR_TIMEOUT : HTTPD_SOCK_ERR_FAIL;
}

esp_err_t fridge_httpd_prepare_response(httpd_req_t *request)
{
    int socket = httpd_req_to_sockfd(request);
    // The HTTP server sends headers in small writes and the JPEG in bounded
    // writes. Avoid Nagle/delayed-ACK stalls on these non-MSS-aligned writes.
    int enabled = 1;
    if (setsockopt(socket, IPPROTO_TCP, TCP_NODELAY, &enabled, sizeof(enabled)) != 0) {
        ESP_LOGW("fridge_http", "Cannot enable TCP_NODELAY: errno=%d", errno);
        return ESP_FAIL;
    }
    sent_bytes = 0;
    send_deadline_us = esp_timer_get_time() + 10000000;
    return httpd_sess_set_send_override(request->handle, socket, deadline_send);
}

esp_err_t fridge_httpd_finish_response(httpd_req_t *request)
{
    // Connection: close must be reflected in actual session lifecycle too.
    return httpd_sess_trigger_close(request->handle, httpd_req_to_sockfd(request));
}

esp_err_t fridge_httpd_start(httpd_handle_t *server)
{
    httpd_config_t config = HTTPD_DEFAULT_CONFIG();
    config.core_id = 1;
    config.task_priority = 4;
    config.stack_size = 8192;
    config.max_open_sockets = 3;
    config.max_uri_handlers = 4;
    config.lru_purge_enable = true;
    config.recv_wait_timeout = 3;
    config.send_wait_timeout = 3;
    return httpd_start(server, &config);
}
