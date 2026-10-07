# Local overlay for the adjacent Hosted 3.0.9-compatible SDIO implementation.
# Keep the upstream checkout untouched and fail closed if its credit loop changes.
function(fridge_patch_hosted_sdio source destination)
    file(READ "${source}" original)
    set(old [=[
			if (has_ctrl || sdio_aggr_credit_timeouts >= SDIO_AGGR_NO_CREDIT_WEDGE)
				ESP_LOGE(TAG, "SDIO aggr: no slave credits (%lu consecutive)%s — "
				         "slave stalled?", (unsigned long)sdio_aggr_credit_timeouts,
				         has_ctrl ? " [ctrl dropped]" : "");
			break;                                /* drop this aggregate */
]=])
    string(FIND "${original}" "${old}" first)
    if(first EQUAL -1)
        message(FATAL_ERROR "Hosted SDIO credit loop changed; review the fridge backpressure overlay")
    endif()
    string(REPLACE "${old}" "" without_match "${original}")

    # Removing all matches would hide duplicates; compare their combined size.
    string(LENGTH "${original}" original_size)
    string(LENGTH "${without_match}" stripped_size)
    string(LENGTH "${old}" match_size)
    math(EXPR removed_size "${original_size} - ${stripped_size}")
    if(NOT removed_size EQUAL match_size)
        message(FATAL_ERROR "Hosted SDIO credit-loop match is ambiguous")
    endif()
    set(new [=[
            // Retain this DMA aggregate until credits arrive. Dropping it here
            // loses TCP segments and forces retransmission. The probe released
            // the bus lock, so RX/ACK processing can continue while TX blocks.
            if (!s_running)
                return;
            if (sdio_aggr_credit_timeouts == SDIO_AGGR_NO_CREDIT_WEDGE ||
                sdio_aggr_credit_timeouts % 100u == 0)
                ESP_LOGW(TAG, "SDIO aggr: waiting for slave credits (%lu rounds); aggregate retained",
                         (unsigned long)sdio_aggr_credit_timeouts);
            eh_host_port_task_delay_ms(1);
            continue;
]=])
    string(REPLACE "${old}" "${new}" patched "${original}")
    file(WRITE "${destination}.tmp" "${patched}")
    configure_file("${destination}.tmp" "${destination}" COPYONLY)
    file(REMOVE "${destination}.tmp")
endfunction()

function(fridge_install_hosted_backpressure)
    if(NOT TARGET eh_host_mcu_transport)
        message(FATAL_ERROR "Hosted MCU transport target is required for the backpressure overlay")
    endif()
    get_target_property(transport_dir eh_host_mcu_transport SOURCE_DIR)
    get_target_property(sources eh_host_mcu_transport SOURCES)
    set(original "${transport_dir}/src/eh_host_bus_sdio.c")
    set(patched "${CMAKE_CURRENT_BINARY_DIR}/fridge_hosted_sdio.c")
    set(replacements 0)
    set(updated_sources "")
    foreach(source IN LISTS sources)
        if(source STREQUAL "src/eh_host_bus_sdio.c" OR source STREQUAL "${original}")
            math(EXPR replacements "${replacements} + 1")
            list(APPEND updated_sources "${patched}")
        else()
            list(APPEND updated_sources "${source}")
        endif()
    endforeach()
    if(NOT replacements EQUAL 1)
        message(FATAL_ERROR "Cannot uniquely replace the Hosted SDIO source")
    endif()
    fridge_patch_hosted_sdio("${original}" "${patched}")
    set_property(TARGET eh_host_mcu_transport PROPERTY SOURCES "${updated_sources}")
    set_property(DIRECTORY APPEND PROPERTY CMAKE_CONFIGURE_DEPENDS "${original}")
    message(STATUS "Fridge: Hosted SDIO aggregate-retention overlay enabled")
endfunction()
