#ifndef PORTAL_NVENC_EXPERIMENTAL_H
#define PORTAL_NVENC_EXPERIMENTAL_H
/* Synthetic experiment only; never use real frames. Caller must supply a current
 * context and correctly initialized, mutually compatible objects and function table.
 * This cannot verify initialization, device isolation, callback-submitted work,
 * or fully synchronize work on other contexts/streams/devices. On any error the
 * caller must quarantine ALL related GPU objects until process exit; no cleanup
 * or recovery is attempted. Different-thread concurrent access, including
 * quarantine, is unsupported and requires external serialization. */
#include <stdint.h>
#include <stddef.h>
#include <ffnvcodec/dynlink_cuda.h>
#include <ffnvcodec/nvEncodeAPI.h>
typedef int (*portal_nvenc_copy)(void *opaque, CUdeviceptr dst, uint32_t pitch);
typedef struct {
    NV_ENCODE_API_FUNCTION_LIST *fn;
    void *encoder;
    NV_ENC_REGISTERED_PTR surface;
    NV_ENC_OUTPUT_PTR bitstream;
    CUdeviceptr dst;
    uint32_t width, height;
    CUresult (CUDAAPI *sync)(void);
    uint8_t *output;
    size_t capacity;
    int quarantined, source_complete, busy; /* zero-initialize before first use */
} PortalNvencExperimental;
/* Each call clears non-NULL data/len outputs and, if p is non-NULL,
 * source_complete. Invalid arguments quarantine p. source_complete becomes true
 * only after successful copy and sync; later failures can leave it true, but it
 * does not prove completion of work outside sync's scope. False is not proof
 * that no work was submitted or that resources can be released. On success data/len
 * describe output, valid until the next call. Same-thread recursive encode is
 * rejected and poisons the outer call; it must not free or mutate p in callbacks. */
int portal_nvenc_experimental_encode(PortalNvencExperimental *p, portal_nvenc_copy copy,
                                     void *opaque, const uint8_t **data, uint32_t *len);
void portal_nvenc_experimental_quarantine(PortalNvencExperimental *p);
#endif
