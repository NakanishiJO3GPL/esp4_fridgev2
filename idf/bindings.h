#pragma once

#include "driver/i2c_master.h"
#include "esp_sccb_intf.h"
#include "esp_sccb_i2c.h"
#include "esp_cam_sensor.h"
#include "esp_cam_sensor_xclk.h"
#include "ov5647.h"

#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_csi.h"
#include "esp_cam_ctlr_interface.h"
#include "esp_cam_ctlr_csi_internal.h"
#include "esp_private/dw_gdma.h"
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
#include "nvs.h"
#include "eh_host_wifi.h"
#include "esp_http_server.h"
#include "esp_random.h"
#include "mdns.h"


// Bindgen cannot evaluate the cast in tskNO_AFFINITY; Clang supplies its value.
enum {
    FRIDGE_TSK_NO_AFFINITY = tskNO_AFFINITY,
};


// Bindgen cannot reliably evaluate an ioctl macro containing sizeof(int).
// Let Clang evaluate the official command instead of duplicating its encoding.
enum {
    FRIDGE_CAMERA_SENSOR_IOC_S_STREAM = ESP_CAM_SENSOR_IOC_S_STREAM,
};
