#pragma once

#include "driver/i2c_master.h"
#include "esp_sccb_intf.h"
#include "esp_sccb_i2c.h"
#include "esp_cam_sensor.h"
#include "esp_cam_sensor_xclk.h"
#include "ov5647.h"

#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_csi.h"
#include "driver/isp.h"
#include "driver/jpeg_encode.h"
#include "hal/color_types.h"
#include "esp_ldo_regulator.h"
#include "esp_heap_caps.h"
#include "esp_cache.h"
#include "esp_private/esp_cache_private.h"

#include "esp_wifi.h"
#include "esp_netif.h"
#include "esp_event.h"
#include "nvs_flash.h"
#include "esp_http_server.h"
#include "esp_random.h"
#include "mdns.h"

// Defaults contain macros/function pointers that must be initialized by IDF.
esp_err_t fridge_wifi_init(void);
esp_err_t fridge_wifi_set_storage_ram(void);
esp_err_t fridge_wifi_next_boot_id(uint64_t *boot_id);
esp_err_t fridge_httpd_start(httpd_handle_t *server);
esp_err_t fridge_httpd_prepare_response(httpd_req_t *request);
esp_err_t fridge_httpd_finish_response(httpd_req_t *request);

// Requires the caller to run on the pinned task/core that created CSI.
esp_err_t fridge_camera_stop_csi(esp_cam_ctlr_handle_t handle);

// Bindgen cannot reliably evaluate an ioctl macro containing sizeof(int).
// Let Clang evaluate the official command instead of duplicating its encoding.
enum {
    FRIDGE_CAMERA_SENSOR_IOC_S_STREAM = ESP_CAM_SENSOR_IOC_S_STREAM,
};
