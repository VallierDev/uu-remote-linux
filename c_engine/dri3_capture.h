#ifndef UU_DRI3_CAPTURE_H
#define UU_DRI3_CAPTURE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Dri3Capture Dri3Capture;

Dri3Capture* dri3_capture_create(const char* display_name, uint32_t width, uint32_t height);

// Trigger GPU blit from Root Window to the DRI3 DMA-BUF Pixmap.
// Returns 0 on success, negative on error.
int dri3_capture_blit(Dri3Capture* cap);

// Wait for next XDamage event or timeout (ms).
// Returns 1 if damaged, 0 if timeout, negative on error.
int dri3_capture_wait_damage(Dri3Capture* cap, int timeout_ms);

// Get the exported DMA-BUF file descriptor and stride
int dri3_capture_get_dma_fd(Dri3Capture* cap);
uint32_t dri3_capture_get_stride(Dri3Capture* cap);

void dri3_capture_destroy(Dri3Capture* cap);

#ifdef __cplusplus
}
#endif

#endif // UU_DRI3_CAPTURE_H
