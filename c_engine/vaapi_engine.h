#ifndef UU_VAAPI_ENGINE_H
#define UU_VAAPI_ENGINE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct VaapiEngine VaapiEngine;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    int64_t target_bps;
    int is_hevc;
} VaapiConfig;

VaapiEngine* vaapi_create(const VaapiConfig* config);

// Import a DRI3 / GBM DMA-BUF fd as the zero-copy input surface
int vaapi_import_dma_buffer(VaapiEngine* engine, int dma_fd, uint32_t stride);

// Host fallback: upload ARGB host memory if DMA-BUF is unavailable
int vaapi_upload_host_buffer(VaapiEngine* engine, const uint8_t* host_ptr, uint32_t pitch);

// Encode one frame
int vaapi_encode_frame(
    VaapiEngine* engine,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
);

void vaapi_destroy(VaapiEngine* engine);

#ifdef __cplusplus
}
#endif

#endif // UU_VAAPI_ENGINE_H
