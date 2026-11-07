#ifndef UU_PORTAL_NVENC_IMAGE_H
#define UU_PORTAL_NVENC_IMAGE_H
#include "portal_nvenc_egl_attrs.h"
#include <EGL/eglext.h>
/* Caller owns initialized compatible display, function pointers and an exclusive
 * validated frame lease. Metadata validation is not admission or synchronization.
 * Keep display and borrowed pixel fd/lease alive until successful completed destroy.
 * Never copy this owner or call concurrently. Unknown completion requires retention. */
struct pnegl_image {
    EGLDisplay display;
    EGLImageKHR image;
    PFNEGLDESTROYIMAGEKHRPROC destroy;
    int destroy_attempted;
};
/* out must be zero initialized. 0 succeeds, negative errno fails. */
int pnegl_image_create(struct pnegl_image *out, EGLDisplay display,
    const struct pnspa_snapshot *frame, PFNEGLCREATEIMAGEKHRPROC create,
    PFNEGLDESTROYIMAGEKHRPROC destroy);
/* completed==1 attests ALL CUDA/GL/encoder access ended and imported views were
 * unregistered. A failed EGL destroy is never retried; keep owner and lease. */
int pnegl_image_destroy_completed(struct pnegl_image *owner, int completed);
#endif
