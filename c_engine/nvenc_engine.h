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

/* GPU-source entry. Serialized calls only. copy runs with the encoder context
 * current, supplies BGRA in dst at pitch, and must not reenter engine APIs.
 * After a successful call, a no-op copy may reuse that encoder-owned surface
 * if no intervening upload/registration changed it. Never use this before its
 * first successful initialization or after any error/quarantine.
 * Caller validates device compatibility and waits for producer acquire BEFORE
 * calling. copy must submit all source access on this context and retain imports
 * until source_complete is observed. No completion is inferred from submission.
 * On ANY error this engine is quarantined: destroy retains all resources, and
 * no further encode/register operation is allowed. Retire its owning worker
 * thread: failure can leave the context pushed and resources mapped/locked.
 * source_complete only proves
 * this context's copy completed, not that all encoder resources are releasable.
 * No Portal/device admission or automatic fallback is provided by this API. */
typedef int (*NvencGpuCopy)(void *opaque, uint64_t dst, uint32_t pitch);
int nvenc_encode_gpu(NvencEngine *engine, NvencGpuCopy copy, void *opaque,
    int force_idr, const uint8_t **out_data, uint32_t *out_len, int *source_complete);

/* Borrowed EGLImageKHR created from a separately validated DMA-BUF. The caller
 * retains image/display/lease through success, or indefinitely on error. expected_pci
 * is the canonical DRM PCI identity (dddd:bb:ss.f), independently established by
 * the caller. Acquire must already be complete. Same serialization/quarantine
 * contract as nvenc_encode_gpu; no implicit compositor synchronization. */
/* Idle, owning-thread preflight only. Returns 0 for the same PCI device,
 * -EXDEV for mismatch, or another negative errno. Does not import images,
 * submit GPU work, or quarantine the engine on failure. This is not producer
 * identity/lifetime admission; encode still rechecks the image device. */
int nvenc_check_device(NvencEngine *engine, const char *expected_pci);
/* Diagnostic only: register/query/unregister an EGLImage in the encoder context,
 * without copying pixels or encoding. Caller holds image/display/pixel fd/lease.
 * views_complete==1 permits image disposal; 0 requires retaining all owners.
 * This does not authorize pixel access or establish producer synchronization. */
int nvenc_inspect_egl(NvencEngine *engine, void *image, const char *expected_pci,
    int *views_complete);
int nvenc_encode_egl(NvencEngine *engine, void *image, const char *expected_pci,
    int force_idr, const uint8_t **out_data, uint32_t *out_len, int *source_complete);

// Destroy the NVENC engine and free all CUDA/NVENC resources.
void nvenc_destroy(NvencEngine* engine);

#ifdef __cplusplus
}
#endif

#endif // UU_NVENC_ENGINE_H
