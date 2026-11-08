#define _GNU_SOURCE
#include "portal_nvenc_device.h"
#include <gbm.h>
#include <xf86drm.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static int extension(const char *list, const char *name) {
    if (!list) return 0;
    size_t n = strlen(name);
    for (const char *p = list; (p = strstr(p, name)); p += n)
        if ((p == list || p[-1] == ' ') && (!p[n] || p[n] == ' ')) return 1;
    return 0;
}
int pnegl_device_open(struct pnegl_device **out, int render_fd) {
    if (!out || *out || render_fd < 0) return -EINVAL;
    struct pnegl_device *d = calloc(1, sizeof(*d));
    if (!d) return -ENOMEM;
    d->fd = -1; *out = d;
    d->fd = fcntl(render_fd, F_DUPFD_CLOEXEC, 0);
    if (d->fd < 0) return -errno;
    if (drmGetNodeTypeFromFd(d->fd) != DRM_NODE_RENDER) return -ENODEV;
    drmDevicePtr drm = NULL;
    if (drmGetDevice2(d->fd, 0, &drm)) return -ENODEV;
    int valid = drm && drm->bustype == DRM_BUS_PCI && drm->businfo.pci &&
        drm->deviceinfo.pci && drm->deviceinfo.pci->vendor_id == 0x10de;
    if (valid) snprintf(d->pci, sizeof(d->pci), "%04x:%02x:%02x.%x",
        drm->businfo.pci->domain, drm->businfo.pci->bus, drm->businfo.pci->dev, drm->businfo.pci->func);
    if (drm) drmFreeDevice(&drm);
    if (!valid) return -ENODEV;
    d->gbm = gbm_create_device(d->fd);
    if (!d->gbm) return -ENODEV;
    d->library = dlopen("libEGL.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!d->library) return -ENOSYS;
    PFNEGLGETPROCADDRESSPROC get;
    PFNEGLINITIALIZEPROC init;
    PFNEGLQUERYSTRINGPROC query;
    PFNEGLCHOOSECONFIGPROC choose;
#define LOAD(dst, name) do { *(void **)(&(dst)) = dlsym(d->library, name); if (!(dst)) return -ENOSYS; } while (0)
    LOAD(get, "eglGetProcAddress"); LOAD(init, "eglInitialize");
    LOAD(query, "eglQueryString"); LOAD(choose, "eglChooseConfig"); LOAD(d->terminate, "eglTerminate");
#undef LOAD
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (PFNEGLGETPLATFORMDISPLAYEXTPROC)get("eglGetPlatformDisplayEXT");
    d->create_image = (PFNEGLCREATEIMAGEKHRPROC)get("eglCreateImageKHR");
    d->destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)get("eglDestroyImageKHR");
    if (!platform || !d->create_image || !d->destroy_image) return -ENOTSUP;
    d->display = platform(EGL_PLATFORM_GBM_KHR, d->gbm, NULL);
    if (d->display == EGL_NO_DISPLAY || !init(d->display, NULL, NULL)) return -EIO;
    d->initialized = 1;
    const char *ext = query(d->display, EGL_EXTENSIONS);
    if (!extension(ext, "EGL_EXT_image_dma_buf_import") ||
        !extension(ext, "EGL_EXT_image_dma_buf_import_modifiers") ||
        !extension(ext, "EGL_KHR_surfaceless_context")) return -ENOTSUP;
    EGLint n = 0;
    const EGLint attrs[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    if (!choose(d->display, attrs, &d->config, 1, &n) || n != 1) return -ENOTSUP;
    return 0;
}
int pnegl_device_close_idle(struct pnegl_device **owner, int idle) {
    if (!owner || !*owner || (*owner)->close_attempted) return -EINVAL;
    if (idle != 1) return -EBUSY;
    struct pnegl_device *d = *owner; d->close_attempted = 1;
    if (d->initialized && (!d->terminate || !d->terminate(d->display))) return -EIO;
    if (d->gbm) gbm_device_destroy(d->gbm);
    if (d->fd >= 0) close(d->fd);
    /* Driver threads may outlive the display. Keep the EGL library loaded. */
    free(d); *owner = NULL;
    return 0;
}

#include "portal_nvenc_image.h"
const char *pnegl_device_pci(const struct pnegl_device *device) {
    return device && device->initialized && !device->close_attempted ? device->pci : NULL;
}
int pnegl_owned_image_create(struct pnegl_device *device, const UuDmaFrame *frame,
    struct pnegl_image **out) {
    if (!device || !device->initialized || device->close_attempted || !frame || !out || *out ||
        frame->fourcc != 0x34325241 || frame->stride > INT32_MAX ||
        !frame->size || frame->offset >= frame->size) return -EINVAL;
    struct pnspa_snapshot snapshot = { .format=SPA_VIDEO_FORMAT_BGRA,
        .width=frame->width, .height=frame->height, .pixel_fd=frame->fd,
        .offset=frame->offset, .stride=(int32_t)frame->stride, .modifier=frame->modifier };
    *out = calloc(1, sizeof(**out));
    if (!*out) return -ENOMEM;
    return pnegl_image_create(*out, device->display, &snapshot, device->create_image, device->destroy_image);
}
void *pnegl_owned_image_handle(const struct pnegl_image *image) {
    return image && !image->destroy_attempted ? image->image : NULL;
}
int pnegl_owned_image_destroy_completed(struct pnegl_image **image, int completed) {
    if (!image || !*image) return -EINVAL;
    int result = pnegl_image_destroy_completed(*image, completed);
    if (!result) { free(*image); *image=NULL; }
    return result;
}
