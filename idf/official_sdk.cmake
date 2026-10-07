# Private CSI bindings must describe the same, unmodified official SDK we link.
function(fridge_check_official_sdk sdk_dir camera_driver_dir)
    set(expected_commit "fff9895c82d744c7237be8847347bdd1b07c6643")
    file(REAL_PATH "${sdk_dir}/components/esp_driver_cam" expected_camera_dir)
    file(REAL_PATH "${camera_driver_dir}" actual_camera_dir)
    if(NOT actual_camera_dir STREQUAL expected_camera_dir)
        message(FATAL_ERROR "CSI driver override rejected: use the driver from the pinned official ESP-IDF SDK")
    endif()

    find_package(Git REQUIRED)
    execute_process(COMMAND "${GIT_EXECUTABLE}" --no-pager -C "${sdk_dir}" remote get-url origin
        RESULT_VARIABLE result OUTPUT_VARIABLE origin OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET TIMEOUT 10)
    if(NOT result STREQUAL "0" OR NOT origin MATCHES "^https://github\\.com/espressif/esp-idf(\\.git)?$")
        message(FATAL_ERROR "ESP-IDF must be a Git checkout from https://github.com/espressif/esp-idf.git")
    endif()
    execute_process(COMMAND "${GIT_EXECUTABLE}" --no-pager -C "${sdk_dir}" rev-parse HEAD
        RESULT_VARIABLE result OUTPUT_VARIABLE commit OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET TIMEOUT 10)
    if(NOT result STREQUAL "0" OR NOT commit STREQUAL expected_commit)
        message(FATAL_ERROR "ESP-IDF must use official 6.1 commit ${expected_commit}; check IDF_PATH and ESP_IDF_VERSION")
    endif()
    execute_process(COMMAND "${GIT_EXECUTABLE}" --no-pager -C "${sdk_dir}"
        diff --quiet HEAD --ignore-submodules=none --
        RESULT_VARIABLE result ERROR_QUIET TIMEOUT 30)
    if(NOT result STREQUAL "0")
        message(FATAL_ERROR "Modified ESP-IDF SDK rejected: use a clean official checkout")
    endif()
endfunction()
