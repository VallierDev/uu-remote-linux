#ifndef UU_NVENC_ENGINE_H
#define UU_NVENC_ENGINE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct NvencEngine NvencEngine;

typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    uint32_t cq;
    int64_t target_bps;
    int64_t max_bps;
    int64_t buffer_bits;
    int is_hevc;    // 1 = H.265, 0 = H.264
    int chroma_444; // 1 = 4:4:4 (H.264 High 4:4:4 / HEVC FRExt), 0 = 4:2:0
} NvencConfig;

// Initialize and open an NVENC encoding session. Returns NULL on error.
NvencEngine* nvenc_create(const NvencConfig* config);

// Register host shared memory (XShm buffer) as CUDA pinned memory that NVENC reads by DMA.
// Returns 0 on success, negative value on error.
int nvenc_register_host_buffer(NvencEngine* engine, void* host_ptr, size_t size);

// Unregister a previously registered host buffer (the CUDA context must be current).
void nvenc_unregister_host_buffer(NvencEngine* engine);

// Encode one frame.
// If host buffer was registered via nvenc_register_host_buffer, input_argb can be NULL
// and the encoder reads the registered host memory directly (no host-to-device copy).
// force_idr: 1 to force keyframe/IDR, 0 otherwise
// out_data: pointer to receive the bitstream buffer (valid until next encode call or destroy)
// out_len: pointer to receive the bitstream size in bytes
// Returns 0 on success, negative value on error.
int nvenc_encode_frame(
    NvencEngine* engine,
    const uint8_t* input_argb,
    uint32_t pitch,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
);

// Destroy the NVENC engine and free all CUDA/NVENC resources.
void nvenc_destroy(NvencEngine* engine);

#ifdef __cplusplus
}
#endif

#endif // UU_NVENC_ENGINE_H
