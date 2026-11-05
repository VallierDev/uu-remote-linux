/* Synthetic synchronization primitive test only; never accesses Portal or a desktop.
 * Build from repository root:
 * cc -std=gnu11 -Wall -Wextra -Werror -O2 contrib/nvidia-native-fence-synthetic.c \
 *   c_engine/portal_nvenc_timeline.c $(pkg-config --cflags --libs gbm libdrm egl glesv2 libspa-0.2) -o /tmp/nvidia-native-fence-synthetic
 * Run in an isolated process: timeout --kill-after=5s 30s /tmp/nvidia-native-fence-synthetic [ /dev/dri/renderD128 ]
 * Exit 0 PASS, 77 UNSUPPORTED, 1 FAIL (timeout has its own exit code).
 * This tests import/transfer and query/wait API success, not wait ordering:
 * producer and readback share a GL context and glReadPixels blocks, so correct
 * pixels do not validate the wait. No cross-process/consumer synchronization or
 * real Portal publication/lifetime is established.
 * All resources are private and intentionally retained until process exit, including
 * on failure: in particular, unknown GPU completion must never cause release.
 */
#define _GNU_SOURCE
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <xf86drm.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>

static int has_ext(const char *s, const char *name) {
    size_t n = strlen(name); const char *p = s;
    if (!p) return 0;
    while ((p = strstr(p, name))) {
        if ((p == s || p[-1] == ' ') && (!p[n] || p[n] == ' ')) return 1;
        p += n;
    }
    return 0;
}
#define FAIL(...) do { fprintf(stderr, __VA_ARGS__); fputc('\n', stderr); return 1; } while (0)
#define UNSUP(...) do { fprintf(stderr, "UNSUPPORTED: " __VA_ARGS__); fputc('\n', stderr); return 77; } while (0)

/* Trusted local /dev/dri and sysfs required; privileged device replacement is
 * out of scope. Pin and inspect before either read/write open. */
static int pin_render(const char *path, struct stat *st) {
    const char prefix[] = "/dev/dri/renderD";
    if (strncmp(path, prefix, sizeof(prefix) - 1)) return -1;
    const char *digits = path + sizeof(prefix) - 1;
    if (!*digits || strspn(digits, "0123456789") != strlen(digits)) return -1;
    int fd = open(path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
    if (fd < 0) return -1;
    char sys[128], resolved[PATH_MAX], subsystem[PATH_MAX];
    if (fstat(fd, st) || !S_ISCHR(st->st_mode)) goto fail;
    snprintf(sys, sizeof(sys), "/sys/dev/char/%u:%u", major(st->st_rdev), minor(st->st_rdev));
    if (!realpath(sys, resolved)) goto fail;
    const char *base = strrchr(resolved, '/');
    if (!base || strcmp(base + 1, path + sizeof("/dev/dri/") - 1)) goto fail;
    size_t len = strlen(sys);
    if (len + sizeof("/subsystem") > sizeof(sys)) goto fail;
    memcpy(sys + len, "/subsystem", sizeof("/subsystem"));
    if (!realpath(sys, subsystem) || strcmp(subsystem, "/sys/class/drm")) goto fail;
    return fd;
fail:
    close(fd);
    return -1;
}
static int same_node(const struct stat *a, const struct stat *b) {
    return S_ISCHR(b->st_mode) && a->st_dev == b->st_dev && a->st_ino == b->st_ino && a->st_rdev == b->st_rdev;
}

int main(int argc, char **argv) {
    const char *path = argc == 2 ? argv[1] : "/dev/dri/renderD128";
    if (argc > 2) FAIL("usage: %s [NVIDIA render node]", argv[0]);
    puts("Synthetic primitives only; no Portal or cross-process synchronization proof.");
    struct stat pinned_stat, a, b;
    int pinned = pin_render(path, &pinned_stat);
    if (pinned < 0) FAIL("refusing unverified /dev/dri/renderD<digits> node");
    char proc[64];
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", pinned);
    int producer = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (producer < 0) { perror("open pinned render node"); return 1; }
    int consumer = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (consumer < 0) { perror("open pinned consumer render node"); return 1; }
    if (fstat(producer, &a) || fstat(consumer, &b) || !same_node(&pinned_stat, &a) ||
        !same_node(&pinned_stat, &b) || drmGetNodeTypeFromFd(producer) != DRM_NODE_RENDER ||
        drmGetNodeTypeFromFd(consumer) != DRM_NODE_RENDER)
        FAIL("render node identity/type validation failed");
    drmDevicePtr dev = NULL;
    if (drmGetDevice2(producer, 0, &dev) || !dev) FAIL("DRM device identity unavailable");
    int nvidia = dev->bustype == DRM_BUS_PCI && dev->deviceinfo.pci && dev->deviceinfo.pci->vendor_id == 0x10de;
    drmFreeDevice(&dev);
    if (!nvidia) UNSUP("render node is not NVIDIA PCI");
    uint64_t cap = 0;
    if (drmGetCap(producer, DRM_CAP_SYNCOBJ_TIMELINE, &cap) || !cap) UNSUP("DRM timeline syncobj unavailable");
    struct gbm_device *gbm = gbm_create_device(producer);
    if (!gbm) FAIL("gbm_create_device failed");
    const char *client_ext = eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    if (!has_ext(client_ext, "EGL_EXT_platform_base") || !has_ext(client_ext, "EGL_KHR_platform_gbm"))
        UNSUP("EGL GBM platform extensions unavailable");
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!platform) UNSUP("eglGetPlatformDisplayEXT unavailable");
    EGLDisplay dpy = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &major, &minor)) FAIL("EGL initialize failed: 0x%x", eglGetError());
    printf("EGL %d.%d vendor=%s\n", major, minor, eglQueryString(dpy, EGL_VENDOR));
    if (!has_ext(client_ext, "EGL_EXT_device_query")) UNSUP("EGL_EXT_device_query unavailable");
    PFNEGLQUERYDISPLAYATTRIBEXTPROC query_display = (void *)eglGetProcAddress("eglQueryDisplayAttribEXT");
    PFNEGLQUERYDEVICESTRINGEXTPROC query_device = (void *)eglGetProcAddress("eglQueryDeviceStringEXT");
    EGLAttrib device = 0;
    if (!query_display || !query_device || !query_display(dpy, EGL_DEVICE_EXT, &device) || !device)
        UNSUP("EGL display device query unavailable");
    EGLDeviceEXT egl_device = (EGLDeviceEXT)(uintptr_t)device;
    if (!has_ext(query_device(egl_device, EGL_EXTENSIONS), "EGL_EXT_device_drm_render_node"))
        UNSUP("EGL_EXT_device_drm_render_node unavailable");
    const char *egl_node = query_device(egl_device, EGL_DRM_RENDER_NODE_FILE_EXT);
    if (!egl_node) UNSUP("EGL device render node unavailable");
    int egl_fd = open(egl_node, O_PATH | O_CLOEXEC);
    struct stat egl_stat;
    if (egl_fd < 0) UNSUP("cannot pin EGL device render node");
    int match = !fstat(egl_fd, &egl_stat) && same_node(&pinned_stat, &egl_stat);
    close(egl_fd);
    if (!match) UNSUP("EGL device render node identity unconfirmed or mismatched");
    printf("EGL device render node identity verified: %s\n", egl_node);
    const char *ext = eglQueryString(dpy, EGL_EXTENSIONS);
    if (!has_ext(ext, "EGL_KHR_surfaceless_context") || !has_ext(ext, "EGL_KHR_fence_sync") ||
        !has_ext(ext, "EGL_ANDROID_native_fence_sync")) UNSUP("required surfaceless/native fence extensions unavailable");
    PFNEGLCREATESYNCKHRPROC create_sync = (void *)eglGetProcAddress("eglCreateSyncKHR");
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    if (!create_sync || !dup_fence) UNSUP("native fence entry points unavailable");
    EGLConfig cfg; EGLint count;
    const EGLint config_attrs[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    const EGLint ctx_attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!eglBindAPI(EGL_OPENGL_ES_API) || !eglChooseConfig(dpy, config_attrs, &cfg, 1, &count) || count != 1)
        FAIL("EGL config failed: 0x%x", eglGetError());
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attrs);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
        FAIL("surfaceless context failed: 0x%x", eglGetError());
    printf("GL renderer=%s\n", glGetString(GL_RENDERER));
    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE || glGetError() != GL_NO_ERROR)
        FAIL("synthetic FBO setup failed");
    uint32_t timeline, binary; int timeline_fd;
    if (drmSyncobjCreate(producer, 0, &timeline) || drmSyncobjCreate(producer, 0, &binary) ||
        drmSyncobjHandleToFD(producer, timeline, &timeline_fd)) FAIL("syncobj creation/export failed");
    struct pnt_ctx *pnt = NULL;
    if (pnt_create(&pnt, consumer, timeline_fd, 1, timeline_fd, 2, NULL, NULL)) FAIL("pnt_create failed");
    glEnable(GL_SCISSOR_TEST);
    const GLfloat colors[4][4] = {{1,0,0,1},{0,1,0,1},{0,0,1,1},{1,1,1,1}};
    for (int i = 0; i < 4; i++) {
        glScissor((i % 2)*32, (i / 2)*32, 32, 32);
        glClearColor(colors[i][0], colors[i][1], colors[i][2], colors[i][3]);
        glClear(GL_COLOR_BUFFER_BIT);
    }
    if (glGetError() != GL_NO_ERROR) FAIL("draw failed; GPU completion unknown, no release");
    const EGLint sync_attrs[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE };
    EGLSyncKHR sync = create_sync(dpy, EGL_SYNC_NATIVE_FENCE_ANDROID, sync_attrs);
    if (sync == EGL_NO_SYNC_KHR) FAIL("native fence creation failed: 0x%x; no release", eglGetError());
    glFlush();
    if (glGetError() != GL_NO_ERROR) FAIL("glFlush failed; no release");
    int fence = dup_fence(dpy, sync);
    if (fence < 0) FAIL("native fence fd duplication failed: 0x%x; no release", eglGetError());
    if (drmSyncobjImportSyncFile(producer, binary, fence) ||
        drmSyncobjTransfer(producer, timeline, 1, binary, 0, 0)) FAIL("sync_file import/transfer failed; no release");
    close(fence);
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) FAIL("clock_gettime failed; no release");
    int64_t deadline = (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec + 3000000000LL;
    int r = pnt_wait(pnt, deadline);
    if (r) FAIL("pnt_wait failed: %d; no release", r);
    puts("native fence import/transfer and independent DRM fd pnt_wait/query succeeded; wait ordering not tested");
    if (pnt_gpu_begin(pnt)) FAIL("pnt_gpu_begin failed; no release");
    unsigned char pixels[4][4];
    for (int i = 0; i < 4; i++) glReadPixels((i % 2)*32 + 16, (i / 2)*32 + 16, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixels[i]);
    GLenum err = glGetError();
    glFinish();
    GLenum finish_err = glGetError();
    if (err != GL_NO_ERROR || finish_err != GL_NO_ERROR) FAIL("read/finish error 0x%x/0x%x; no release", err, finish_err);
    if (pnt_gpu_complete(pnt)) FAIL("pnt_gpu_complete failed; no release");
    for (int i = 0; i < 4; i++) for (int c = 0; c < 4; c++)
        if (pixels[i][c] != (unsigned char)(colors[i][c] * 255)) FAIL("synthetic pixel mismatch quadrant=%d channel=%d; no release", i, c);
    struct spa_meta_sync_timeline meta = { .acquire_point = 1, .release_point = 2,
        .flags = SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE };
    if (pnt_release(pnt, &meta)) FAIL("pnt_release failed");
    uint64_t point = 0;
    if (drmSyncobjQuery(producer, &timeline, &point, 1) || point < 2 ||
        (meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE)) FAIL("release verification failed");
    puts("PASS: API operations, synthetic pixels and release verified; same-context blocking readback does not validate wait ordering or cross-process/consumer synchronization; not Portal publication");
    return 0;
}
