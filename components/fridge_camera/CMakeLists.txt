idf_component_get_property(camera_driver_dir esp_driver_cam COMPONENT_DIR)

# The IDF 6.1 stop helper uses the driver's own private layout, never a copy.
idf_component_register(
    SRCS "camera_stop.c" "network_defaults.c"
    INCLUDE_DIRS "."
    PRIV_INCLUDE_DIRS "${camera_driver_dir}/csi/src"
    REQUIRES esp_cam_sensor esp_sccb_intf esp_driver_i2c esp_driver_cam
             esp_driver_isp esp_driver_jpeg esp_hw_support esp_mm heap
             esp_wifi esp_hosted esp_netif esp_event nvs_flash esp_http_server mdns
    PRIV_REQUIRES esp_driver_dma esp_driver_cam
)

if(NOT CMAKE_BUILD_EARLY_EXPANSION AND CONFIG_ESP_HOSTED_HOST_TRANSPORT_BUS_SDIO_READY)
    include("${CMAKE_CURRENT_LIST_DIR}/hosted_backpressure.cmake")
    fridge_install_hosted_backpressure()
endif()
