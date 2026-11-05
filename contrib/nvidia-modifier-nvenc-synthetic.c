/* Synthetic-only GBM -> EGL -> CUDA -> NVENC -> software FFmpeg test; no capture.
 * Build from repository root:
 * mkdir -p -m 700 "$HOME/.cache/uu-synthetic-test"
 * cc -std=gnu11 -Wall -Wextra -Werror -O2 -Ivendor/ffnvcodec \
 *   contrib/nvidia-modifier-nvenc-synthetic.c contrib/portal-nvenc-experimental.c \
 *   c_engine/portal_nvenc_spa.c c_engine/portal_nvenc_timeline.c c_engine/portal_nvenc_egl_attrs.c $(pkg-config --cflags --libs gbm libdrm egl glesv2 libspa-0.2) \
 *   -ldl -o "$HOME/.cache/uu-synthetic-test/probe"
 * Run: timeout --kill-after=5s 60s "$HOME/.cache/uu-synthetic-test/probe" /dev/dri/renderD128
 * Targets ARGB8888 modifier 0x0300000000606010; a query alone is not a
 * successful import or encode. Based on nvidia-egl-nvenc-synthetic.c.
 * Requires /usr/bin/timeout, /usr/bin/ffmpeg and /usr/bin/ffprobe. Child
 * processes time out after 20s. Outer timeout also bounds stuck driver calls;
 * forced termination may leave the private temporary directory for removal.
 * Exit 0 pass, 1 failure, 2 unsupported/unavailable (timeout uses 124/137).
 * Producer and consumer share a process: this is not cross-process Portal.
 * This includes production implementation solely for private test access; it is
 * not a production API and does not demonstrate Portal import or zero-copy.
 * Consumer import uses a trusted synthetic snapshot under an exclusive lease;
 * this does not prove real Portal buffer revocation or lifetime safety.
 */
#define _GNU_SOURCE
#include "../c_engine/nvenc_engine.c"
#include "portal-nvenc-experimental.h"
#include "../c_engine/portal_nvenc_spa.h"
#include "../c_engine/portal_nvenc_egl_attrs.h"
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <time.h>
#include <errno.h>
#include <strings.h>
#include <fcntl.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <limits.h>
#include <unistd.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>

/* NVIDIA cudaEGL.h CU_EGL_COLOR_FORMAT_ARGB (BGRA byte order). */
#define CU_EGL_COLOR_FORMAT_ARGB 0x06
#define W 256
#define H 256
#define BYTES (W * H * 4)
#define TARGET_MOD UINT64_C(0x0300000000606010)

static int query_target(EGLDisplay dpy) {
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    EGLint n = 0, got = 0; int found = 0, result = 1;
    if (!query) { puts("modifier query unavailable; actual import still attempted"); return 0; }
    if (!query(dpy, DRM_FORMAT_ARGB8888, 0, NULL, NULL, &n) || n < 0 || n > 65536) {
        fprintf(stderr, "modifier count query failed/invalid: n=%d EGL=0x%x\n", n, eglGetError()); return 1;
    }
    printf("ARGB8888 modifier count=%d\n", n);
    if (!n) { puts("target not listed; actual import still attempted"); return 0; }
    EGLuint64KHR *mods = calloc((size_t)n, sizeof(*mods));
    EGLBoolean *ext = calloc((size_t)n, sizeof(*ext));
    if (!mods || !ext) { fprintf(stderr, "modifier query allocation failed\n"); goto out; }
    if (!query(dpy, DRM_FORMAT_ARGB8888, n, mods, ext, &got) || got < 0 || got > n) {
        fprintf(stderr, "modifier list query failed/invalid: got=%d EGL=0x%x\n", got, eglGetError()); goto out;
    }
    for (EGLint i = 0; i < got; i++) if (mods[i] == TARGET_MOD) {
        printf("target modifier 0x%016llx external_only=%s (%u)\n", (unsigned long long)mods[i],
               ext[i] == EGL_TRUE ? "true" : ext[i] == EGL_FALSE ? "false" : "invalid", ext[i]);
        if (found++ || (ext[i] != EGL_TRUE && ext[i] != EGL_FALSE)) goto out;
    }
    if (!found) puts("target not listed; actual import still attempted");
    result = 0;
out:
    free(mods); free(ext); return result;
}

typedef struct { CUDA_MEMCPY2D cp; CUresult (CUDAAPI *copy2d)(const CUDA_MEMCPY2D *); } CopyArgs;
static int copy_surface(void *opaque, CUdeviceptr dst, uint32_t pitch) {
    CopyArgs *args = opaque;
    args->cp.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    args->cp.dstDevice = dst; args->cp.dstPitch = pitch;
    return args->copy2d(&args->cp) != CUDA_SUCCESS;
}

static int check_dimensions(const char *input) {
    int fds[2], status; char buf[64] = {0};
    if (pipe(fds)) { perror("pipe"); return -1; }
    pid_t p = fork();
    if (p < 0) { perror("fork"); close(fds[0]); close(fds[1]); return -1; }
    if (!p) {
        if (dup2(fds[1], STDOUT_FILENO) < 0) _exit(127);
        close(fds[0]); close(fds[1]);
        execl("/usr/bin/timeout", "timeout", "--kill-after=2s", "20s", "/usr/bin/ffprobe", "-v", "error", "-select_streams", "v:0",
               "-show_entries", "stream=width,height", "-of", "csv=s=x:p=0",
               input, (char *)NULL);
        _exit(127);
    }
    close(fds[1]); size_t used = 0;
    while (used < sizeof(buf) - 1) {
        ssize_t n = read(fds[0], buf + used, sizeof(buf) - 1 - used);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) break;
        used += (size_t)n;
    }
    close(fds[0]);
    pid_t r; do { r = waitpid(p, &status, 0); } while (r < 0 && errno == EINTR);
    if (r < 0 || !WIFEXITED(status) || WEXITSTATUS(status) || strcmp(buf, "256x256\n")) {
        fprintf(stderr, "ffprobe dimensions invalid: %s\n", buf); return -1;
    }
    puts("decoded stream dimensions: 256x256"); return 0;
}

static int run_ffmpeg(const char *input, const char *output) {
    pid_t p = fork();
    if (p < 0) { perror("fork"); return -1; }
    if (!p) {
        execl("/usr/bin/timeout", "timeout", "--kill-after=2s", "20s", "/usr/bin/ffmpeg", "-nostdin", "-v", "error", "-y",
               "-c:v", "h264", "-f", "h264", "-i", input,
               "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24",
               output, (char *)NULL);
        _exit(127);
    }
    int status;
    pid_t waited;
    do { waited = waitpid(p, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited < 0 || !WIFEXITED(status) || WEXITSTATUS(status)) {
        fprintf(stderr, "ffmpeg failed or unavailable\n"); return -1;
    }
    return 0;
}


/* Synthetic producer only: no GL sampling and no desktop access. */
static int node = -1, dma = -1, egl_ready, producer_uncertain;
static struct gbm_device *gbm;
static struct gbm_bo *bo;
static EGLDisplay display = EGL_NO_DISPLAY;
static EGLImageKHR image = EGL_NO_IMAGE_KHR, consumer_image = EGL_NO_IMAGE_KHR;
static PFNEGLDESTROYIMAGEKHRPROC destroy_image;
static CUgraphicsResource resource;
static CUresult (CUDAAPI *unregister_image)(CUgraphicsResource);
static void *preflight_cuda;
static CUdevice preflight_device;
/* Exact GBM-derived parameters used for the producer/consumer EGLImage. */
static struct { uint32_t width, height, fourcc, offset, pitch; uint64_t modifier; int fd; } imported;

static int has_ext(const char *list, const char *name) {
    size_t n = strlen(name); const char *p = list;
    if (!p) return 0;
    while ((p = strstr(p, name))) {
        if ((p == list || p[-1] == ' ') && (p[n] == '\0' || p[n] == ' ')) return 1;
        p += n;
    }
    return 0;
}

/* Keep the producer context and objects alive until the independent wait succeeds. */
static EGLContext producer_ctx = EGL_NO_CONTEXT;
static GLuint producer_tex, producer_fbo;
static EGLSyncKHR producer_sync = EGL_NO_SYNC_KHR;
static int fence_fd = -1;
static int produce_image(void) {
    EGLContext ctx = EGL_NO_CONTEXT; EGLConfig config; EGLint count = 0;
    GLuint tex = 0, fbo = 0; int current = 0, submitted = 0, result = 1;
    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    PFNEGLCREATESYNCKHRPROC create_sync = (void *)eglGetProcAddress("eglCreateSyncKHR");
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    if (!has_ext(extensions, "EGL_KHR_fence_sync") ||
        !has_ext(extensions, "EGL_ANDROID_native_fence_sync") || !create_sync || !dup_fence) {
        fprintf(stderr, "UNSUPPORTED: EGL native fence sync unavailable\n"); return 2;
    }
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    const EGLint attrs[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                             EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!has_ext(eglQueryString(display, EGL_EXTENSIONS), "EGL_KHR_surfaceless_context") || !bind_image) {
        fprintf(stderr, "surfaceless context or GL EGLImage entry point unavailable\n"); return 2;
    }
    if (!eglBindAPI(EGL_OPENGL_ES_API) || !eglChooseConfig(display, attrs, &config, 1, &count) || count != 1) {
        fprintf(stderr, "EGL GLES2 config failed: 0x%x\n", eglGetError()); return 1;
    }
    ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT) { fprintf(stderr, "EGL GLES2 context failed: 0x%x\n", eglGetError()); return 1; }
    if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        fprintf(stderr, "surfaceless make current failed: 0x%x\n", eglGetError()); goto out;
    }
    current = 1;
    printf("GL renderer: %s\n", glGetString(GL_RENDERER));
    if (!has_ext((const char *)glGetString(GL_EXTENSIONS), "GL_OES_EGL_image")) {
        fprintf(stderr, "GL_OES_EGL_image unavailable\n"); result = 2; goto out;
    }
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    bind_image(GL_TEXTURE_2D, image);
    GLenum error = glGetError();
    if (error != GL_NO_ERROR) { fprintf(stderr, "GL image binding failed: 0x%x; retaining resources\n", error); producer_uncertain = 1; return 1; }
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER); error = glGetError();
    printf("GL framebuffer status=0x%x error=0x%x\n", status, error);
    if (error != GL_NO_ERROR) { producer_uncertain = 1; return 1; }
    if (status != GL_FRAMEBUFFER_COMPLETE) { fprintf(stderr, "imported image not framebuffer-renderable\n"); result = 2; goto out; }
    glEnable(GL_SCISSOR_TEST);
    const GLfloat colors[4][4] = {{1,0,0,1},{0,1,0,1},{0,0,1,1},{1,1,1,1}};
    for (unsigned q = 0; q < 4; q++) {
        glScissor((q % 2) * (W / 2), (q / 2) * (H / 2), W / 2, H / 2);
        glClearColor(colors[q][0], colors[q][1], colors[q][2], colors[q][3]);
        glClear(GL_COLOR_BUFFER_BIT); submitted = 1;
    }
    producer_uncertain = 1;
    error = glGetError();
    if (error != GL_NO_ERROR) { fprintf(stderr, "GL draw error: 0x%x; no release\n", error); return 1; }
    const EGLint sync_attrs[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE };
    producer_sync = create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, sync_attrs);
    if (producer_sync == EGL_NO_SYNC_KHR) { fprintf(stderr, "native fence creation failed: 0x%x\n", eglGetError()); return 1; }
    glFlush();
    if ((error = glGetError()) != GL_NO_ERROR) { fprintf(stderr, "glFlush failed: 0x%x\n", error); return 1; }
    fence_fd = dup_fence(display, producer_sync);
    if (fence_fd < 0) { fprintf(stderr, "native fence export failed: 0x%x\n", eglGetError()); return 1; }
    producer_ctx = ctx; producer_tex = tex; producer_fbo = fbo;
    puts("GL producer flushed and native sync_file exported; context retained");
    return 0;
out:
    if (current) {
        if (submitted && result) { producer_uncertain = 1; return 1; }
        if (fbo) glDeleteFramebuffers(1, &fbo);
        if (tex) glDeleteTextures(1, &tex);
        error = glGetError();
        if (error != GL_NO_ERROR) { fprintf(stderr, "GL cleanup failed: 0x%x\n", error); producer_uncertain = 1; return 1; }
        if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT)) {
            fprintf(stderr, "EGL context unbind failed: 0x%x\n", eglGetError()); producer_uncertain = 1; return 1;
        }
    }
    if (!eglDestroyContext(display, ctx)) { fprintf(stderr, "EGL context destroy failed: 0x%x\n", eglGetError()); producer_uncertain = 1; return 1; }
    return result;
}
/* Check CUDA ordinal 0, the device used by this probe, before any GBM/EGL work. */
static int preflight(void) {
    drmDevicePtr d = NULL; char cuda[32] = {0}, drm[32];
    tcuInit *init; tcuDeviceGet *get;
    CUresult (CUDAAPI *bus_id)(char *, int, CUdevice);
    if (drmGetNodeTypeFromFd(node) != DRM_NODE_RENDER) { fprintf(stderr, "node is not a DRM render node\n"); return 2; }
    if (drmGetDevice2(node, 0, &d) || !d) { fprintf(stderr, "DRM PCI identity unavailable\n"); return 2; }
    if (d->bustype != DRM_BUS_PCI || !d->businfo.pci) { drmFreeDevice(&d); fprintf(stderr, "DRM node is not PCI\n"); return 2; }
    snprintf(drm, sizeof(drm), "%04x:%02x:%02x.%x", d->businfo.pci->domain, d->businfo.pci->bus, d->businfo.pci->dev, d->businfo.pci->func);
    drmFreeDevice(&d);
    preflight_cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!preflight_cuda) { fprintf(stderr, "CUDA library unavailable\n"); return 2; }
    *(void **)(&init) = dlsym(preflight_cuda, "cuInit");
    *(void **)(&get) = dlsym(preflight_cuda, "cuDeviceGet");
    *(void **)(&bus_id) = dlsym(preflight_cuda, "cuDeviceGetPCIBusId");
    if (!init || !get || !bus_id) { fprintf(stderr, "CUDA identity symbols unavailable\n"); return 2; }
    CUresult rc = init(0);
    if (rc) { fprintf(stderr, "CUDA preflight init failed: %d\n", rc); return 1; }
    rc = get(&preflight_device, 0);
    if (rc) { fprintf(stderr, "CUDA ordinal 0 unavailable: %d\n", rc); return 2; }
    rc = bus_id(cuda, sizeof(cuda), preflight_device);
    if (rc) { fprintf(stderr, "CUDA PCI identity query failed: %d\n", rc); return 1; }
    int mismatch = strcasecmp(drm, cuda) != 0;
    printf("preflight DRM PCI %s CUDA ordinal 0 PCI %s: %s\n", drm, cuda, mismatch ? "mismatch" : "match");
    return mismatch ? 2 : 0;
}
/* Pin the inspected inode before opening it for I/O, so renames cannot swap
 * the target. A privileged actor able to alter device nodes/sysfs remains out
 * of scope; users must supply a trusted local /dev/dri render node. */
static int open_render_node(const char *path) {
    const char prefix[] = "/dev/dri/renderD";
    const char *digits = path + (strncmp(path, prefix, sizeof(prefix) - 1) == 0 ? sizeof(prefix) - 1 : 0);
    if (digits == path || !*digits || strspn(digits, "0123456789") != strlen(digits)) {
        fprintf(stderr, "refusing path: expected /dev/dri/renderD<digits> (no symlinks)\n"); return 2;
    }
    int pinned = open(path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
    if (pinned < 0) { perror("pin render node"); return 1; }
    int result = 1; struct stat before, after; char sys[128], resolved[PATH_MAX], subsystem[PATH_MAX], proc[64];
    if (fstat(pinned, &before) || !S_ISCHR(before.st_mode)) { fprintf(stderr, "not a character device\n"); goto out; }
    snprintf(sys, sizeof(sys), "/sys/dev/char/%u:%u", major(before.st_rdev), minor(before.st_rdev));
    if (!realpath(sys, resolved)) { perror("resolve DRM sysfs device"); goto out; }
    const char *base = strrchr(resolved, '/');
    if (!base || strcmp(base + 1, path + sizeof("/dev/dri/") - 1)) { fprintf(stderr, "sysfs device name does not match render node\n"); goto out; }
    size_t len = strlen(sys);
    if (len + sizeof("/subsystem") > sizeof(sys)) { fprintf(stderr, "sysfs path too long\n"); goto out; }
    memcpy(sys + len, "/subsystem", sizeof("/subsystem"));
    if (!realpath(sys, subsystem) || strcmp(subsystem, "/sys/class/drm")) { fprintf(stderr, "sysfs subsystem is not DRM\n"); goto out; }
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", pinned);
    node = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (node < 0) { perror("open pinned render node"); goto out; }
    if (fstat(node, &after) || !S_ISCHR(after.st_mode) || before.st_dev != after.st_dev ||
        before.st_ino != after.st_ino || before.st_rdev != after.st_rdev || drmGetNodeTypeFromFd(node) != DRM_NODE_RENDER) {
        fprintf(stderr, "opened node identity/type mismatch\n"); goto out;
    }
    printf("render node pinned and verified: dev=%u:%u inode=%llu; trusted local device required\n", major(after.st_rdev), minor(after.st_rdev), (unsigned long long)after.st_ino);
    result = 0;
out:
    if (close(pinned)) { perror("close pinned node"); result = 1; }
    return result;
}
static int setup_image(const char *path) {
    int opened = open_render_node(path);
    if (opened) return opened;
    int identity = preflight();
    if (identity) return identity;
    gbm = gbm_create_device(node);
    if (!gbm) { fprintf(stderr, "GBM device creation failed\n"); return 1; }
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    PFNEGLCREATEIMAGEKHRPROC create = (void *)eglGetProcAddress("eglCreateImageKHR");
    destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
    if (!platform || !create || !destroy_image) { fprintf(stderr, "EGL image entry points unavailable\n"); return 2; }
    display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, &major, &minor)) {
        fprintf(stderr, "EGL GBM initialize failed: 0x%x\n", eglGetError()); return 1;
    }
    egl_ready = 1;
    printf("node=%s EGL=%d.%d vendor=%s target=ARGB8888 0x%016llx\n", path, major, minor, eglQueryString(display, EGL_VENDOR), (unsigned long long)TARGET_MOD);
    if (query_target(display)) return 1;
    const uint64_t target = TARGET_MOD;
    errno = 0;
    bo = gbm_bo_create_with_modifiers(gbm, W, H, GBM_FORMAT_ARGB8888, &target, 1);
    if (!bo) { int err = errno; fprintf(stderr, "GBM explicit modifier allocation failed: errno=%d (%s)\n", err, err ? strerror(err) : "not supplied"); return err == EINVAL || err == ENOSYS || err == ENOTSUP ? 2 : 1; }
    uint64_t actual = gbm_bo_get_modifier(bo);
    uint32_t pitch = gbm_bo_get_stride(bo), offset = gbm_bo_get_offset(bo, 0);
    printf("GBM layout: %ux%u planes=%d modifier=0x%016llx stride=%u offset=%u\n", gbm_bo_get_width(bo), gbm_bo_get_height(bo), gbm_bo_get_plane_count(bo), (unsigned long long)actual, pitch, offset);
    if (gbm_bo_get_width(bo) != W || gbm_bo_get_height(bo) != H || gbm_bo_get_plane_count(bo) != 1 || actual != target || pitch < W * 4 || pitch > INT32_MAX || offset > INT32_MAX) {
        fprintf(stderr, "GBM returned unexpected layout\n"); return 1;
    }
    uint32_t stride; void *map_data;
    errno = 0;
    unsigned char *pixels = gbm_bo_map(bo, 0, 0, W, H, GBM_BO_TRANSFER_WRITE, &stride, &map_data);
    if (!pixels) { int err = errno; fprintf(stderr, "GBM CPU map unavailable: errno=%d (%s); trying GL producer\n", err, err ? strerror(err) : "not supplied"); }
    else {
        printf("GBM CPU map stride=%u (GL producer used instead)\n", stride);
        if (stride < W * 4) { fprintf(stderr, "invalid map stride\n"); gbm_bo_unmap(bo, map_data); return 1; }
        gbm_bo_unmap(bo, map_data);
    }
    dma = gbm_bo_get_fd(bo);
    if (dma < 0) { perror("gbm_bo_get_fd"); return 1; }
    imported.width = W; imported.height = H; imported.fourcc = DRM_FORMAT_ARGB8888;
    imported.fd = dma; imported.offset = offset; imported.pitch = pitch; imported.modifier = actual;
    const EGLint attrs[] = {EGL_WIDTH,(EGLint)imported.width,EGL_HEIGHT,(EGLint)imported.height,EGL_LINUX_DRM_FOURCC_EXT,(EGLint)imported.fourcc,
        EGL_DMA_BUF_PLANE0_FD_EXT,imported.fd,EGL_DMA_BUF_PLANE0_OFFSET_EXT,(EGLint)imported.offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,(EGLint)imported.pitch,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,(EGLint)(uint32_t)imported.modifier,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,(EGLint)(uint32_t)(imported.modifier >> 32),EGL_NONE};
    image = create(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "EGL DMA-BUF import failed: 0x%x\n", eglGetError()); return 1; }
    puts("EGL import succeeded (not yet CUDA/NVENC verified)");
    return produce_image();
}
/* Private minimal H.264 initializer. No teardown on any failure: a driver may
 * have created resources even when it reports an error. Process exit owns them. */
static NvencEngine *probe_nvenc_create(const NvencConfig *config) {
    NvencEngine *e = calloc(1, sizeof(*e));
    if (!e) return NULL;
    e->cuda_lib = preflight_cuda; e->dev = preflight_device;
    e->width = config->width; e->height = config->height;
#define LOAD(field, name) do { *(void **)(&e->field) = dlsym(e->cuda_lib, name); if (!e->field) { fprintf(stderr, "missing CUDA symbol %s\n", name); return NULL; } } while (0)
    LOAD(cuDevicePrimaryCtxRetain, "cuDevicePrimaryCtxRetain");
    LOAD(cuDevicePrimaryCtxRelease, "cuDevicePrimaryCtxRelease_v2");
    LOAD(cuCtxPushCurrent, "cuCtxPushCurrent_v2"); LOAD(cuCtxPopCurrent, "cuCtxPopCurrent_v2");
    LOAD(cuMemAlloc, "cuMemAlloc_v2"); LOAD(cuMemFree, "cuMemFree_v2");
    LOAD(cuMemcpyHtoD, "cuMemcpyHtoD_v2");
#undef LOAD
#define CU_CHECK(call) do { CUresult r_ = (call); if (r_ != CUDA_SUCCESS) { fprintf(stderr, "initialization %s failed: %d\n", #call, r_); return NULL; } } while (0)
#define NV_CHECK(call) do { NVENCSTATUS r_ = (call); if (r_ != NV_ENC_SUCCESS) { fprintf(stderr, "initialization %s failed: %d\n", #call, r_); return NULL; } } while (0)
    CU_CHECK(e->cuDevicePrimaryCtxRetain(&e->cu_ctx, e->dev));
    if (!e->cu_ctx) { fprintf(stderr, "primary context is null\n"); return NULL; }
    CU_CHECK(e->cuCtxPushCurrent(e->cu_ctx));
    e->nvenc_lib = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!e->nvenc_lib) { fprintf(stderr, "NVENC library unavailable\n"); return NULL; }
    tNvEncodeAPIGetMaxSupportedVersion get_max = (tNvEncodeAPIGetMaxSupportedVersion)dlsym(e->nvenc_lib, "NvEncodeAPIGetMaxSupportedVersion");
    tNvEncodeAPICreateInstance create = (tNvEncodeAPICreateInstance)dlsym(e->nvenc_lib, "NvEncodeAPICreateInstance");
    if (!get_max || !create) { fprintf(stderr, "NVENC entry points unavailable\n"); return NULL; }
    uint32_t max = 0; NV_CHECK(get_max(&max));
    if (max < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) { fprintf(stderr, "NVENC API too old: 0x%x\n", max); return NULL; }
    e->fn.version = NV_ENCODE_API_FUNCTION_LIST_VER; NV_CHECK(create(&e->fn));
    if (!e->fn.nvEncOpenEncodeSessionEx || !e->fn.nvEncGetEncodePresetConfigEx || !e->fn.nvEncInitializeEncoder ||
        !e->fn.nvEncRegisterResource || !e->fn.nvEncCreateBitstreamBuffer || !e->fn.nvEncEncodePicture ||
        !e->fn.nvEncLockBitstream || !e->fn.nvEncUnlockBitstream || !e->fn.nvEncMapInputResource ||
        !e->fn.nvEncUnmapInputResource || !e->fn.nvEncDestroyBitstreamBuffer || !e->fn.nvEncUnregisterResource || !e->fn.nvEncDestroyEncoder) {
        fprintf(stderr, "NVENC function table incomplete\n"); return NULL;
    }
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open = {0};
    open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER; open.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    open.device = e->cu_ctx; open.apiVersion = NVENCAPI_VERSION;
    NV_CHECK(e->fn.nvEncOpenEncodeSessionEx(&open, &e->encoder));
    if (!e->encoder) { fprintf(stderr, "NVENC session is null\n"); return NULL; }
    NV_ENC_PRESET_CONFIG preset = {0}; preset.version = NV_ENC_PRESET_CONFIG_VER; preset.presetCfg.version = NV_ENC_CONFIG_VER;
    NV_CHECK(e->fn.nvEncGetEncodePresetConfigEx(e->encoder, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset));
    NV_ENC_CONFIG cfg = preset.presetCfg;
    cfg.gopLength = config->gop; cfg.frameIntervalP = 1;
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR; cfg.rcParams.averageBitRate = config->target_bps;
    cfg.rcParams.maxBitRate = config->max_bps; cfg.rcParams.vbvBufferSize = config->max_bps / 10;
    cfg.rcParams.targetQuality = config->cq;
    cfg.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
    cfg.encodeCodecConfig.h264Config.chromaFormatIDC = 1;
    cfg.encodeCodecConfig.h264Config.idrPeriod = cfg.gopLength;
    cfg.encodeCodecConfig.h264Config.repeatSPSPPS = 1; cfg.encodeCodecConfig.h264Config.outputAUD = 1;
    NV_ENC_INITIALIZE_PARAMS params = {0}; params.version = NV_ENC_INITIALIZE_PARAMS_VER;
    params.encodeGUID = NV_ENC_CODEC_H264_GUID; params.presetGUID = NV_ENC_PRESET_P4_GUID;
    params.encodeWidth = params.darWidth = e->width; params.encodeHeight = params.darHeight = e->height;
    params.frameRateNum = config->fps; params.frameRateDen = 1; params.enablePTD = 1;
    params.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY; params.encodeConfig = &cfg;
    NV_CHECK(e->fn.nvEncInitializeEncoder(e->encoder, &params));
    e->frame_bytes = (size_t)e->width * e->height * 4;
    CU_CHECK(e->cuMemAlloc(&e->cu_dptr, e->frame_bytes));
    e->reg_res.version = NV_ENC_REGISTER_RESOURCE_VER; e->reg_res.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    e->reg_res.width = e->width; e->reg_res.height = e->height; e->reg_res.pitch = e->width * 4;
    e->reg_res.resourceToRegister = (void *)e->cu_dptr; e->reg_res.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
    NV_CHECK(e->fn.nvEncRegisterResource(e->encoder, &e->reg_res));
    NV_ENC_CREATE_BITSTREAM_BUFFER bs = {0}; bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    NV_CHECK(e->fn.nvEncCreateBitstreamBuffer(e->encoder, &bs)); e->bitstream_buf = bs.bitstreamBuffer;
    if (!e->cu_dptr || !e->reg_res.registeredResource || !e->bitstream_buf) { fprintf(stderr, "NVENC initialization returned null resource\n"); return NULL; }
    e->output_staging_cap = 2 * 1024 * 1024; e->output_staging = malloc(e->output_staging_cap);
    if (!e->output_staging) { fprintf(stderr, "staging allocation failed\n"); return NULL; }
    CUcontext popped = NULL; CU_CHECK(e->cuCtxPopCurrent(&popped));
    if (popped != e->cu_ctx) { fprintf(stderr, "initialization popped wrong context\n"); return NULL; }
#undef CU_CHECK
#undef NV_CHECK
    return e;
}

static int cleanup_image(void) {
    int result = 0;
    if (image != EGL_NO_IMAGE_KHR && !destroy_image(display, image)) { fprintf(stderr, "EGL image destruction failed: 0x%x\n", eglGetError()); return 1; }
    if (egl_ready && !eglTerminate(display)) { fprintf(stderr, "EGL terminate failed: 0x%x\n", eglGetError()); return 1; }
    if (dma >= 0 && close(dma)) { perror("close dma"); result = 1; }
    if (bo) gbm_bo_destroy(bo);
    if (gbm) gbm_device_destroy(gbm);
    if (node >= 0 && close(node)) { perror("close node"); result = 1; }
    return result;
}
/* All objects, the lease and signaling authority belong exclusively to this
 * single-process synthetic test. No conclusion about a Portal producer follows. */
static struct pnt_ctx *timeline;
static struct spa_meta_sync_timeline timeline_meta;
static uint32_t producer_handle;
static int sync_fd = -1, consumer_node = -1;
static struct pnspa_snapshot consumer_snap;
static int synthetic_acquire(void) {
    struct stat st;
    if (fstat(dma, &st) || st.st_size <= 0 || (uint64_t)st.st_size > UINT32_MAX ||
        gbm_bo_get_offset(bo, 0) >= (uint64_t)st.st_size) return -1;
    if (drmSyncobjCreate(node, 0, &producer_handle) ||
        drmSyncobjHandleToFD(node, producer_handle, &sync_fd)) return -1;
    struct spa_chunk chunk = { .offset = gbm_bo_get_offset(bo, 0),
        .size = (uint32_t)st.st_size - gbm_bo_get_offset(bo, 0), .stride = (int32_t)gbm_bo_get_stride(bo) };
    struct spa_data datas[3] = {
        { .type = SPA_DATA_DmaBuf, .fd = dma, .maxsize = (uint32_t)st.st_size, .chunk = &chunk },
        { .type = SPA_DATA_SyncObj, .fd = sync_fd }, { .type = SPA_DATA_SyncObj, .fd = sync_fd }
    };
    timeline_meta = (struct spa_meta_sync_timeline) { .acquire_point = 1, .release_point = 2,
        .flags = SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE };
    struct spa_meta meta = { .type = SPA_META_SyncTimeline, .size = sizeof(timeline_meta), .data = &timeline_meta };
    struct spa_buffer buffer = { .n_metas = 1, .metas = &meta, .n_datas = 3, .datas = datas };
    struct spa_video_info_raw format = { .format = SPA_VIDEO_FORMAT_BGRA, .flags = SPA_VIDEO_FLAG_MODIFIER,
        .size = { W, H }, .modifier = TARGET_MOD };
    struct pnspa_snapshot snap;
    if (!pnspa_snapshot(&buffer, &format, TARGET_MOD, &snap)) return -1;
    /* SPA BGRA corresponds to DRM ARGB8888 on this test's byte layout.
     * mapoffset must be zero: EGL imported the plane offset directly. */
    if (snap.format != SPA_VIDEO_FORMAT_BGRA || imported.fourcc != DRM_FORMAT_ARGB8888 ||
        snap.width != imported.width || snap.height != imported.height ||
        snap.modifier != imported.modifier || snap.pixel_fd != imported.fd ||
        snap.mapoffset != 0 || snap.offset != imported.offset ||
        snap.stride <= 0 || (uint32_t)snap.stride != imported.pitch) {
        fprintf(stderr, "snapshot differs from actual GBM-derived EGL import parameters\n"); return -1;
    }
    consumer_snap = snap;
    uint32_t binary;
    char proc[64];
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", node);
    consumer_node = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    struct stat a, b;
    if (consumer_node < 0 || fstat(node, &a) || fstat(consumer_node, &b) ||
        a.st_dev != b.st_dev || a.st_ino != b.st_ino || a.st_rdev != b.st_rdev ||
        drmSyncobjCreate(node, 0, &binary) ||
        drmSyncobjImportSyncFile(node, binary, fence_fd) ||
        drmSyncobjTransfer(node, producer_handle, snap.acquire_point, binary, 0, 0) ||
        pnt_create(&timeline, consumer_node, snap.acquire_fd, snap.acquire_point,
                   snap.release_fd, snap.release_point, NULL, NULL)) return -1;
    if (close(fence_fd) || drmSyncobjDestroy(node, binary)) return -1;
    fence_fd = -1;
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    int64_t until = (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 1000000000;
    if (pnt_wait(timeline, until)) return -1;
    /* The wait, not context destruction or glFinish, establishes producer completion. */
    glDeleteFramebuffers(1, &producer_fbo);
    glDeleteTextures(1, &producer_tex);
    PFNEGLDESTROYSYNCKHRPROC destroy_sync = (void *)eglGetProcAddress("eglDestroySyncKHR");
    if (glGetError() != GL_NO_ERROR || !destroy_sync || !destroy_sync(display, producer_sync) ||
        !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT) ||
        !eglDestroyContext(display, producer_ctx)) return -1;
    producer_uncertain = 0;
    if (pnt_gpu_begin(timeline)) return -1;
    puts("native fence transferred to timeline; independent render fd pnt wait succeeded; same process, not Portal");
    EGLint attrs[PNEGL_ATTR_COUNT];
    PFNEGLCREATEIMAGEKHRPROC create = (void *)eglGetProcAddress("eglCreateImageKHR");
    if (!create || !pnegl_attrs(&consumer_snap, attrs)) return -1;
    consumer_image = create(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (consumer_image == EGL_NO_IMAGE_KHR) {
        fprintf(stderr, "consumer EGL import failed: 0x%x\n", eglGetError()); return -1;
    }
    puts("independent consumer EGL image imported from snapshot");
    return 0;
}
static int synthetic_release(void) {
    uint64_t current = 0;
    if (pnt_gpu_complete(timeline)) {
        fprintf(stderr, "synthetic GPU completion check failed before release; no signal attempted\n");
        return -1;
    }
    if (pnt_release(timeline, &timeline_meta)) {
        /* A failed driver call may still have side effects; never retry it. */
        fprintf(stderr, "synthetic release failed; signal outcome unknown, do not retry\n");
        return -1;
    }
    if (drmSyncobjQuery(node, &producer_handle, &current, 1) || current < 2 ||
        (timeline_meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE) ||
        pnt_destroy(&timeline) || drmSyncobjDestroy(node, producer_handle) || close(sync_fd) || close(consumer_node)) {
        fprintf(stderr, "synthetic release returned success; later verification/cleanup failed, signal may have occurred\n");
        return -1;
    }
    sync_fd = -1;
    puts("real pnt release observed on synthetic producer timeline");
    return 0;
}
int main(int argc, char **argv) {
    NvencConfig config = { .width = W, .height = H, .fps = 30, .gop = 30,
                           .cq = 18, .target_bps = 4000000, .max_bps = 6000000 };
    NvencEngine *e = NULL;
    int uncertain = 0;
    CUcontext popped = NULL;
    int pushed = 0, result = 1, fd = -1, rawfd = -1;
    char dir[] = "/tmp/uu-synthetic-XXXXXX";
    char bitpath[128], rawpath[128];
    int dir_created = 0, bit_created = 0, raw_created = 0;
    unsigned char rgb[W * H * 3];
    const uint8_t *data = NULL; uint32_t len = 0;
    CUresult (CUDAAPI *reg)(CUgraphicsResource *, EGLImageKHR, unsigned int) = NULL;
    CUresult (CUDAAPI *mapped)(CUeglFrame *, CUgraphicsResource, unsigned int, unsigned int) = NULL;
    CUresult (CUDAAPI *copy2d)(const CUDA_MEMCPY2D *) = NULL;
    CUresult (CUDAAPI *sync)(void) = NULL;
    const unsigned char expected[4][3] = {{255,0,0},{0,255,0},{0,0,255},{255,255,255}};

    if (argc != 2) { fprintf(stderr, "usage: %s NVIDIA-render-node\n", argv[0]); return 2; }
    result = setup_image(argv[1]);
    if (result) {
        if (producer_uncertain) { fprintf(stderr, "uncertain GL completion/cleanup; retaining GPU resources until process exit\n"); result = 1; }
        else if (cleanup_image()) result = 1;
        printf("OVERALL %s exit=%d; query is not import/encode verification\n", result == 2 ? "UNAVAILABLE" : "FAIL", result); return result;
    }
    result = 1;
    e = probe_nvenc_create(&config);
    if (!e) { fprintf(stderr, "NVENC initialization failed; retaining all resources until isolated process exit\n"); return 1; }
    result = 1;
    *(void **)(&reg) = dlsym(e->cuda_lib, "cuGraphicsEGLRegisterImage");
    *(void **)(&mapped) = dlsym(e->cuda_lib, "cuGraphicsResourceGetMappedEglFrame");
    *(void **)(&unregister_image) = dlsym(e->cuda_lib, "cuGraphicsUnregisterResource");
    *(void **)(&copy2d) = dlsym(e->cuda_lib, "cuMemcpy2D_v2");
    *(void **)(&sync) = dlsym(e->cuda_lib, "cuCtxSynchronize");
    if (!reg || !mapped || !unregister_image || !copy2d || !sync) {
        fprintf(stderr, "CUDA EGL entry points unavailable; retaining pending producer until process exit\n"); return 2;
    }
    if (synthetic_acquire()) { fprintf(stderr, "synthetic acquire failed; retaining all resources until process exit\n"); return 1; }
    /* Any failure after begin and before release quarantines the whole lease. */
    uncertain = 1;
    if (e->cuCtxPushCurrent(e->cu_ctx) != CUDA_SUCCESS) { fprintf(stderr, "context push failed\n"); uncertain = 1; goto cleanup; }
    pushed = 1;
    CUresult cr = reg(&resource, consumer_image, 0);
    if (cr) { fprintf(stderr, "CUDA EGL registration failed: %d\n", cr); result = cr == 801 ? 2 : 1; if (cr != 801) uncertain = 1; goto cleanup; }
    CUeglFrame frame = {0};
    cr = mapped(&frame, resource, 0, 0);
    if (cr) { fprintf(stderr, "mapped EGL frame failed: %d\n", cr); result = cr == 801 ? 2 : 1; if (cr != 801) uncertain = 1; goto cleanup; }
    printf("mapped EGL frame: %ux%u type=%d planes=%u channels=%u color=%d format=%d\n", frame.width, frame.height, frame.frameType, frame.planeCount, frame.numChannels, frame.eglColorFormat, frame.cuFormat);
    if (frame.width != W || frame.height != H || frame.planeCount != 1 || frame.numChannels != 4 || frame.cuFormat != CU_AD_FORMAT_UNSIGNED_INT8 || frame.eglColorFormat != CU_EGL_COLOR_FORMAT_ARGB) {
        fprintf(stderr, "unsupported mapped frame dimensions or format\n"); result = 2; goto cleanup;
    }
    CUDA_MEMCPY2D cp = {0};
    if (frame.frameType == CU_EGL_FRAME_TYPE_ARRAY && frame.frame.pArray[0]) {
        cp.srcMemoryType = CU_MEMORYTYPE_ARRAY; cp.srcArray = frame.frame.pArray[0];
    } else if (frame.frameType == CU_EGL_FRAME_TYPE_PITCH && frame.frame.pPitch[0] && frame.pitch >= W * 4) {
        cp.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        cp.srcDevice = (CUdeviceptr)(uintptr_t)frame.frame.pPitch[0]; cp.srcPitch = frame.pitch;
    } else {
        fprintf(stderr, "unsupported mapped frame storage or invalid pointer/pitch\n"); result = 2; goto cleanup;
    }
    cp.WidthInBytes = W*4; cp.Height = H;
    CopyArgs args = { .cp = cp, .copy2d = copy2d };
    PortalNvencExperimental experiment = { .fn = &e->fn, .encoder = e->encoder,
        .surface = e->reg_res.registeredResource, .bitstream = e->bitstream_buf,
        .dst = e->cu_dptr, .width = W, .height = H, .sync = sync,
        .output = e->output_staging, .capacity = e->output_staging_cap };
    if (portal_nvenc_experimental_encode(&experiment, copy_surface, &args, &data, &len)) {
        fprintf(stderr, "copy/encode failed; GPU resources quarantined until process exit\n");
        uncertain = 1; goto cleanup;
    }
    /* The copy was synchronized by encode; retire the source registration
     * while its context and synthetic lease are still held. */
    cr = unregister_image(resource);
    if (cr != CUDA_SUCCESS) { fprintf(stderr, "CUDA source unregister failed: %d; retaining lease until process exit\n", cr); goto cleanup; }
    resource = NULL;
    cr = sync();
    if (cr != CUDA_SUCCESS) { fprintf(stderr, "CUDA source unregister sync failed: %d; retaining lease until process exit\n", cr); goto cleanup; }
    if (!destroy_image(display, consumer_image)) {
        fprintf(stderr, "consumer EGL image destruction failed: 0x%x; retaining lease\n", eglGetError()); goto cleanup;
    }
    consumer_image = EGL_NO_IMAGE_KHR;
    puts("CUDA source unregistered and synchronized; consumer image destroyed before synthetic release");
    if (synthetic_release()) { fprintf(stderr, "synthetic release failed; retaining resources until process exit\n"); return 1; }
    uncertain = 0;
    printf("EGL %s -> NVENC CUDA buffer GPU copy synchronized\n", frame.frameType == CU_EGL_FRAME_TYPE_ARRAY ? "array" : "pitch");
    int idr = 0;
    for (size_t i = 0; i + 3 < len; i++) {
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 && (data[i + 3] & 31) == 5) idr = 1;
    }
    if (!idr) { fprintf(stderr, "bitstream has no Annex B IDR NAL\n"); goto cleanup; }
    printf("NVENC IDR bitstream: %u bytes\n", len);
    if (!mkdtemp(dir)) { perror("mkdtemp"); goto cleanup; }
    dir_created = 1;
    if (snprintf(bitpath, sizeof(bitpath), "%s/frame-XXXXXX", dir) >= (int)sizeof(bitpath) ||
        snprintf(rawpath, sizeof(rawpath), "%s/rgb-XXXXXX", dir) >= (int)sizeof(rawpath)) {
        fprintf(stderr, "temporary path too long\n"); goto cleanup;
    }
    fd = mkstemp(bitpath); if (fd < 0) { perror("mkstemp bitstream"); goto cleanup; } bit_created = 1;
    for (size_t off = 0; off < len;) {
        ssize_t n = write(fd, data + off, len - off);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { perror("write bitstream"); goto cleanup; }
        off += (size_t)n;
    }
    if (close(fd)) { fd = -1; perror("close bitstream"); goto cleanup; } fd = -1;
    rawfd = mkstemp(rawpath); if (rawfd < 0) { perror("mkstemp raw"); goto cleanup; } raw_created = 1;
    if (close(rawfd)) { rawfd = -1; perror("close raw"); goto cleanup; } rawfd = -1;
    if (check_dimensions(bitpath) || run_ffmpeg(bitpath, rawpath)) goto cleanup;
    rawfd = open(rawpath, O_RDONLY);
    if (rawfd < 0) { perror("open raw"); goto cleanup; }
    size_t got = 0;
    while (got < sizeof(rgb)) {
        ssize_t n = read(rawfd, rgb + got, sizeof(rgb) - got);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) { perror("read raw"); goto cleanup; }
        if (!n) break;
        got += (size_t)n;
    }
    unsigned char extra;
    ssize_t more;
    do { more = read(rawfd, &extra, 1); } while (more < 0 && errno == EINTR);
    if (got != sizeof(rgb) || more != 0) { fprintf(stderr, "decoded size mismatch: %zu, extra=%zd\n", got, more); goto cleanup; }
    for (unsigned q = 0; q < 4; q++) {
        for (unsigned dy = 16; dy < H / 2 - 16; dy += 8) for (unsigned dx = 16; dx < W / 2 - 16; dx += 8) {
            unsigned x = (q % 2) * (W / 2) + dx, y = (q / 2) * (H / 2) + dy;
            const unsigned char *p = rgb + (y * W + x) * 3;
            for (unsigned c = 0; c < 3; c++) if (abs((int)p[c] - expected[q][c]) > 35) {
                fprintf(stderr, "decoded color mismatch quadrant=%u x=%u y=%u RGB=%u,%u,%u\n", q, x, y, p[0], p[1], p[2]); goto cleanup;
            }
        }
        printf("quadrant %u: 144 sampled pixels within tolerance 35\n", q);
    }
    result = 0;
cleanup:
    if (fd >= 0 && close(fd)) { perror("close bitstream"); result = 1; }
    if (rawfd >= 0 && close(rawfd)) { perror("close raw"); result = 1; }
    if (bit_created && unlink(bitpath)) { perror("unlink bitstream"); result = 1; }
    if (raw_created && unlink(rawpath)) { perror("unlink raw"); result = 1; }
    if (dir_created && rmdir(dir)) { perror("rmdir temporary directory"); result = 1; }
    /* The engine's destroy ignores API results. Check teardown here instead,
     * in dependency order, retaining libraries just as the engine does. */
    if (uncertain) { fprintf(stderr, "pnt begun: unsupported or failure is FAIL; uncertain lease/GPU resources retained until process exit\n"); result = 1; goto abandon; }
    if (!pushed && e->cuCtxPushCurrent(e->cu_ctx) == CUDA_SUCCESS) pushed = 1;
    if (!pushed) { fprintf(stderr, "cleanup context push failed; resources leaked until process exit\n"); result = 1; goto abandon; }
    else {
#define CLEAN(call) do { int r_ = (int)(call); if (r_) { fprintf(stderr, "cleanup %s failed: %d; leaving remaining resources to process exit\n", #call, r_); result = 1; goto abandon; } } while (0)
        if (sync) CLEAN(sync());
        if (resource) CLEAN(unregister_image(resource));
        if (e->bitstream_buf) CLEAN(e->fn.nvEncDestroyBitstreamBuffer(e->encoder, e->bitstream_buf));
        if (e->reg_res.registeredResource) CLEAN(e->fn.nvEncUnregisterResource(e->encoder, e->reg_res.registeredResource));
        if (e->cu_dptr) CLEAN(e->cuMemFree(e->cu_dptr));
        if (e->encoder) CLEAN(e->fn.nvEncDestroyEncoder(e->encoder));
        CLEAN(e->cuCtxPopCurrent(&popped));
        if (popped != e->cu_ctx) { fprintf(stderr, "cleanup popped wrong context\n"); result = 1; goto abandon; }
        if (e->is_standalone_ctx) CLEAN(e->cuCtxDestroy(e->cu_ctx));
        else CLEAN(e->cuDevicePrimaryCtxRelease(e->dev));
#undef CLEAN
    }
    if (cleanup_image()) result = 1;
    free(e->output_staging); free(e);
abandon:
    /* Retain even host-side engine state on uncertain driver/cleanup paths. */
    printf("%s: synthetic only; no Portal or zero-copy claim\n", result == 0 ? "PASS" : result == 2 ? "UNSUPPORTED" : "FAIL");
    return result;
}
