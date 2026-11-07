#include "portal_nvenc_image.h"
#include <errno.h>
#include <string.h>
int pnegl_image_create(struct pnegl_image *out, EGLDisplay display,
    const struct pnspa_snapshot *frame, PFNEGLCREATEIMAGEKHRPROC create,
    PFNEGLDESTROYIMAGEKHRPROC destroy) {
    if (!out || out->image || out->destroy_attempted || display == EGL_NO_DISPLAY || !create || !destroy) return -EINVAL;
    EGLint attrs[PNEGL_ATTR_COUNT];
    if (!pnegl_attrs(frame, attrs)) return -EINVAL;
    EGLImageKHR image = create(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) return -EIO;
    *out = (struct pnegl_image){ .display=display, .image=image, .destroy=destroy };
    return 0;
}
int pnegl_image_destroy_completed(struct pnegl_image *owner, int completed) {
    if (!owner || !owner->image || !owner->destroy || owner->destroy_attempted) return -EINVAL;
    if (completed != 1) return -EBUSY;
    owner->destroy_attempted = 1;
    if (!owner->destroy(owner->display, owner->image)) return -EIO;
    memset(owner, 0, sizeof(*owner));
    return 0;
}
