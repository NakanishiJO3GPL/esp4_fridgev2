#include <stddef.h>
#include "esp_idf_version.h"
#include "esp_cam_ctlr.h"
#include "esp_cam_ctlr_interface.h"
#include "esp_cam_ctlr_csi_internal.h"
#include "esp_private/dw_gdma.h"

#if ESP_IDF_VERSION_MAJOR != 6 || ESP_IDF_VERSION_MINOR != 1
#error "Review the CSI stop helper against this ESP-IDF version before using it"
#endif

// IDF 6.1's public stop does not mask DMA callbacks. Its completion callback
// rearms the DMA channel before publishing the completed frame. On the original
// pinned IRQ core, mask callbacks and stop atomically so pending IRQs cannot
// rearm DMA during teardown. All callbacks were installed at controller creation;
// unregistering them here does not allocate or block.
esp_err_t fridge_camera_stop_csi(esp_cam_ctlr_handle_t handle)
{
    if (!handle) {
        return ESP_ERR_INVALID_ARG;
    }
    csi_controller_t *controller = (csi_controller_t *)((char *)handle - offsetof(csi_controller_t, base));
    dw_gdma_event_callbacks_t callbacks = {0};
    portMUX_TYPE lock = portMUX_INITIALIZER_UNLOCKED;
    portENTER_CRITICAL(&lock);
    esp_err_t result = dw_gdma_channel_register_event_callbacks(controller->dma_chan, &callbacks, NULL);
    if (result == ESP_OK) {
        result = esp_cam_ctlr_stop(handle);
    }
    portEXIT_CRITICAL(&lock);
    return result;
}
