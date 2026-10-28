#ifndef UU_VAAPI_ENGINE_H
#define UU_VAAPI_ENGINE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct VaapiEngine VaapiEngine;

typedef struct {
    uint32_t width;      // encode (output) size
    uint32_t height;
    uint32_t cap_width;  // capture (input) size; VPP scales cap -> width
    uint32_t cap_height;
    uint32_t fps;
    uint32_t gop;
    int64_t target_bps;
    int64_t max_bps;     // VBR ceiling (viewer preset maxrate)
    int64_t buffer_bits; // HRD buffer size
    int is_hevc;
    const char *device;  // DRM render node (UU_VAAPI_DEVICE); NULL = /dev/dri/renderD128
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
