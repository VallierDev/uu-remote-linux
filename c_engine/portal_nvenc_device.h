#ifndef UU_PORTAL_NVENC_DEVICE_H
#define UU_PORTAL_NVENC_DEVICE_H
#include <EGL/egl.h>
#include <EGL/eglext.h>
struct gbm_device;
/* Unique owner; do not copy. Initialized display, no context or frame access.
 * Device selection does not prove the compositor's DMA-BUF provenance. */
struct pnegl_device {
    int fd, initialized, close_attempted;
    void *library;
    struct gbm_device *gbm;
    EGLDisplay display;
    EGLConfig config;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNEGLTERMINATEPROC terminate;
    char pci[32];
};
/* Borrow an already opened NVIDIA PCI render node; duplicate it CLOEXEC.
 * out must be NULL. Even on failure, non-NULL out owns partial initialization. */
int pnegl_device_open(struct pnegl_device **out, int render_fd);
/* Caller proves all workers, images and contexts ended before idle==1.
 * No retry after terminate failure; retain the object and dependent resources. */
int pnegl_device_close_idle(struct pnegl_device **owner, int idle);
/* Opaque image bridge for Rust. The fd is borrowed and must remain alive.
 * A non-NULL result owns even failed creation state; retain on error. */
#include "dmabuf_frame.h"
struct pnegl_image;
const char *pnegl_device_pci(const struct pnegl_device *device);
int pnegl_owned_image_create(struct pnegl_device *device, const UuDmaFrame *frame,
    struct pnegl_image **out);
void *pnegl_owned_image_handle(const struct pnegl_image *image);
int pnegl_owned_image_destroy_completed(struct pnegl_image **image, int completed);
#endif
