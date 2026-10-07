idf_component_get_property(camera_driver_dir esp_driver_cam COMPONENT_DIR)

# Bindgen uses the pinned official IDF 6.1 private CSI layout, never a copied header.
# This is a header-only component; application runtime code is entirely Rust.
idf_component_register(
    INCLUDE_DIRS "." "${camera_driver_dir}/csi/src"
    REQUIRES esp_cam_sensor esp_sccb_intf esp_driver_i2c esp_driver_cam
             esp_driver_isp esp_driver_jpeg esp_hw_support esp_mm heap
             esp_wifi esp_hosted esp_netif esp_event nvs_flash esp_http_server mdns
             esp_driver_dma
)

if(NOT CMAKE_BUILD_EARLY_EXPANSION)
    idf_build_get_property(sdk_dir IDF_PATH)
    include("${CMAKE_CURRENT_LIST_DIR}/official_sdk.cmake")
    fridge_check_official_sdk("${sdk_dir}" "${camera_driver_dir}")

endif()
