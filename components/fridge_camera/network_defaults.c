#include "esp_wifi.h"
#include "esp_http_server.h"

// Keep IDF's version/config-specific defaults, including Wi-Fi function pointers.
esp_err_t fridge_wifi_init(void)
{
    wifi_init_config_t config = WIFI_INIT_CONFIG_DEFAULT();
    return esp_wifi_init(&config);
}

// Bindgen's zeroed Default is not HTTPD_DEFAULT_CONFIG(). Rust applies the
// application's overrides and starts the server after this initialization.
void fridge_httpd_default_config(httpd_config_t *config)
{
    *config = (httpd_config_t)HTTPD_DEFAULT_CONFIG();
}
