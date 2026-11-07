#include "portal_nvenc_egl_attrs.h"
#include <EGL/eglext.h>
#include <drm_fourcc.h>
#include <limits.h>
#include <string.h>

int pnegl_attrs(const struct pnspa_snapshot *s, EGLint out[PNEGL_ATTR_COUNT])
{
    uint32_t fourcc;
    if (!s || !out) return 0;
    switch (s->format) {
    case SPA_VIDEO_FORMAT_BGRA: fourcc = DRM_FORMAT_ARGB8888; break;
    case SPA_VIDEO_FORMAT_BGRx: fourcc = DRM_FORMAT_XRGB8888; break;
    case SPA_VIDEO_FORMAT_RGBA: fourcc = DRM_FORMAT_ABGR8888; break;
    case SPA_VIDEO_FORMAT_RGBx: fourcc = DRM_FORMAT_XBGR8888; break;
    default: return 0;
    }
    uint64_t offset = (uint64_t)s->mapoffset + s->offset;
    if (s->pixel_fd < 0 || !s->width || s->width > INT_MAX ||
        !s->height || s->height > INT_MAX || s->stride <= 0 ||
        offset > INT_MAX || (offset & 3) || (s->stride & 3) ||
        (uint64_t)s->width * 4 > (uint32_t)s->stride ||
        s->modifier == UINT64_C(0x00ffffffffffffff)) return 0;
    const EGLint attrs[PNEGL_ATTR_COUNT] = {
        EGL_WIDTH, (EGLint)s->width, EGL_HEIGHT, (EGLint)s->height,
        EGL_LINUX_DRM_FOURCC_EXT, (EGLint)fourcc,
        EGL_DMA_BUF_PLANE0_FD_EXT, (EGLint)s->pixel_fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)s->stride,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(uint32_t)s->modifier,
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(uint32_t)(s->modifier >> 32),
        EGL_NONE
    };
    memcpy(out, attrs, sizeof(attrs));
    return 1;
}
