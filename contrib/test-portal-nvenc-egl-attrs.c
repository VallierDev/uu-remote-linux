/* Standalone metadata-only test; no Portal or GPU access.
 * cc -std=gnu11 -Wall -Wextra -Werror contrib/test-portal-nvenc-egl-attrs.c c_engine/portal_nvenc_egl_attrs.c $(pkg-config --cflags --libs libspa-0.2 egl libdrm) -o /tmp/test-pnegl && /tmp/test-pnegl */
#include "../c_engine/portal_nvenc_egl_attrs.h"
#include <EGL/eglext.h>
#include <drm_fourcc.h>
#include <assert.h>
#include <limits.h>
#include <string.h>

int main(void)
{
    struct pnspa_snapshot s = { .format = SPA_VIDEO_FORMAT_BGRA, .width = 256,
        .height = 256, .pixel_fd = 3, .stride = 1024 };
    EGLint a[PNEGL_ATTR_COUNT], before[PNEGL_ATTR_COUNT];
    const uint32_t formats[] = { SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRx,
        SPA_VIDEO_FORMAT_RGBA, SPA_VIDEO_FORMAT_RGBx };
    const uint32_t fourcc[] = { DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888,
        DRM_FORMAT_ABGR8888, DRM_FORMAT_XBGR8888 };
    for (unsigned i = 0; i < 4; i++) {
        s.format = formats[i]; assert(pnegl_attrs(&s, a)); assert((uint32_t)a[5] == fourcc[i]);
    }
    s.modifier = UINT64_C(0x0300000000606010); s.mapoffset = 16; s.offset = 8;
    assert(pnegl_attrs(&s, a)); assert(a[9] == 24);
    assert((uint32_t)a[13] == 0x00606010 && (uint32_t)a[15] == 0x03000000);
    s.modifier = 0; assert(pnegl_attrs(&s, a)); assert(!a[13] && !a[15]);
#define REJECT(field, value) do { __typeof__(s.field) old = s.field; s.field = (value); \
    memset(a, 0x55, sizeof(a)); memcpy(before, a, sizeof(a)); \
    assert(!pnegl_attrs(&s, a)); assert(!memcmp(a, before, sizeof(a))); s.field = old; } while (0)
    REJECT(format, 0); REJECT(pixel_fd, -1); REJECT(width, 0); REJECT(height, 0);
    REJECT(width, UINT32_MAX); REJECT(height, UINT32_MAX); REJECT(stride, 0);
    REJECT(stride, -1); REJECT(stride, 1023); REJECT(stride, 1020);
    REJECT(offset, 1); REJECT(offset, UINT32_MAX); REJECT(mapoffset, UINT32_MAX);
    s.mapoffset = UINT32_MAX - 3; s.offset = 8; assert(!pnegl_attrs(&s, a));
    s.mapoffset = 0; s.offset = 0; s.width = 1; s.height = INT_MAX; s.stride = INT_MAX - 3;
    s.pixel_fd = INT_MAX; assert(pnegl_attrs(&s, a));
    return 0;
}
