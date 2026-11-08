/* Private synthetic PipeWire GL native fence and CUDA/NVENC test; no capture.
 * Controlled buffer-ownership test: Header generations are not authentication.
 * No Portal, concurrent revocation safety or fence-necessity claim.
 * All resources, including held leases, remain alive until process exit. */
#define _GNU_SOURCE
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include "../../c_engine/nvenc_engine.c"
#include "../../contrib/portal-nvenc-experimental.h"
#include "../../c_engine/portal_nvenc_egl_attrs.h"
#include "../../c_engine/portal_nvenc_spa.h"
#include "../../c_engine/portal_nvenc_timeline.h"
#include <dlfcn.h>
#include <strings.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <xf86drm.h>
#include <drm.h>
#include <errno.h>
#include <limits.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <linux/sync_file.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#define WIDTH 256
#define HEIGHT 256
#define TARGET_MOD UINT64_C(0x0300000000606010)
#define FRAME_GENERATION UINT64_C(1)
#define CONSUMER_ACK_GENERATION UINT64_C(2)
struct endpoint { struct pw_thread_loop *loop; struct pw_stream *stream; int producer; struct spa_video_info_raw format; };
struct allocation { struct gbm_bo *bo; int fd, sync_fd; uint32_t handle; dev_t dev; ino_t ino; uint32_t stride, size, offset; };
static struct endpoint src, dst;
static struct gbm_device *gbm;
static int drmfd = -1, consumer_fd = -1;
static atomic_int failed, received, formats, sent, queue_succeeded, consumer_queued, producer_returned;
static atomic_int native_fence_fd = -1;
static struct pw_buffer *held;
/* Consumer callback owns this until checked destruction or process exit. */
static struct pnt_ctx *timeline_owner;
static _Atomic(struct allocation *) rendered;
static struct allocation *layout;
static EGLDisplay display;
static EGLConfig config;
static PFNEGLCREATEIMAGEKHRPROC create_image;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
/* On an uncertain consumer failure retain the image and lease until exit. */
static EGLImageKHR consumer_image = EGL_NO_IMAGE_KHR;
static const char *output_path;

static void *preflight_cuda;
static CUdevice preflight_device;
static const char *output_path;
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


typedef struct { CUDA_MEMCPY2D cp; CUresult (CUDAAPI *copy)(const CUDA_MEMCPY2D *); } CopyArgs;
static int copy_surface(void *opaque, CUdeviceptr dst, uint32_t pitch) {
    CopyArgs *a = opaque; a->cp.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    a->cp.dstDevice = dst; a->cp.dstPitch = pitch; return a->copy(&a->cp) != CUDA_SUCCESS;
}

/* CUDA calls, including initialization, are confined to the consumer callback.
 * Never release any resources on an uncertain driver call or buffer revocation. */
static int cuda_copy(EGLImageKHR image) {
    void *lib = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib) { fprintf(stderr, "CUDA library unavailable\n"); return -1; }
    CUresult (CUDAAPI *init)(unsigned int), (CUDAAPI *device_get)(CUdevice *, int);
    CUresult (CUDAAPI *device_count)(int *), (CUDAAPI *bus_id)(char *, int, CUdevice);
    CUresult (CUDAAPI *reg)(CUgraphicsResource *, EGLImageKHR, unsigned int);
    CUresult (CUDAAPI *mapped)(CUeglFrame *, CUgraphicsResource, unsigned int, unsigned int);
    CUresult (CUDAAPI *sync)(void), (CUDAAPI *unreg)(CUgraphicsResource);
    CUresult (CUDAAPI *copy)(const CUDA_MEMCPY2D *);
#define LOAD(fn, name) do { *(void **)(&(fn)) = dlsym(lib, (name)); if (!(fn)) { fprintf(stderr, "missing %s\n", (name)); return -1; } } while (0)
    LOAD(init, "cuInit"); LOAD(device_get, "cuDeviceGet"); LOAD(device_count, "cuDeviceGetCount"); LOAD(bus_id, "cuDeviceGetPCIBusId");
    LOAD(reg, "cuGraphicsEGLRegisterImage");
    LOAD(mapped, "cuGraphicsResourceGetMappedEglFrame"); LOAD(sync, "cuCtxSynchronize");
    LOAD(unreg, "cuGraphicsUnregisterResource"); LOAD(copy, "cuMemcpy2D_v2");
#undef LOAD
#define CHECK(call) do { CUresult r_ = (call); if (r_ != CUDA_SUCCESS) { fprintf(stderr, "%s failed: %d; lease quarantined\n", #call, r_); return -1; } } while (0)
    drmDevicePtr d = NULL; char drm[32], cuda[32] = {0}; CUdevice dev;
    if (drmGetDevice2(drmfd, 0, &d) || !d || d->bustype != DRM_BUS_PCI || !d->businfo.pci) {
        if (d) drmFreeDevice(&d);
        fprintf(stderr, "DRM PCI identity unavailable\n"); return -1;
    }
    snprintf(drm, sizeof(drm), "%04x:%02x:%02x.%x", d->businfo.pci->domain, d->businfo.pci->bus, d->businfo.pci->dev, d->businfo.pci->func);
    drmFreeDevice(&d);
    int count = 0, matches = 0;
    CHECK(init(0)); CHECK(device_count(&count));
    if (count <= 0 || count > 4096) { fprintf(stderr, "invalid CUDA device count\n"); return -1; }
    for (int i = 0; i < count; i++) {
        CUdevice candidate;
        CHECK(device_get(&candidate, i)); CHECK(bus_id(cuda, sizeof(cuda), candidate));
        if (!strcasecmp(drm, cuda)) { dev = candidate; matches++; }
    }
    if (matches != 1) { fprintf(stderr, "DRM/CUDA PCI identity matched %d devices\n", matches); return -1; }
    preflight_cuda = lib; preflight_device = dev;
    NvencConfig config = { .width = WIDTH, .height = HEIGHT, .fps = 30, .gop = 30,
                           .cq = 18, .target_bps = 4000000, .max_bps = 6000000 };
    NvencEngine *engine = probe_nvenc_create(&config);
    if (!engine) { fprintf(stderr, "NVENC initialization failed; quarantined\n"); return -1; }
    CHECK(engine->cuCtxPushCurrent(engine->cu_ctx));
    CUgraphicsResource resource = NULL; CUeglFrame frame = {0};
    CHECK(reg(&resource, image, 0));
    if (!resource) return -1;
    CHECK(mapped(&frame, resource, 0, 0));
    /* CUDA_EGL_COLOR_FORMAT_ARGB = 0x06, BGRA in memory. */
    if (frame.width != WIDTH || frame.height != HEIGHT || frame.planeCount != 1 ||
        frame.numChannels != 4 || frame.cuFormat != CU_AD_FORMAT_UNSIGNED_INT8 || frame.eglColorFormat != 0x06) {
        fprintf(stderr, "unsupported CUDA EGL frame format or dimensions\n"); return -1;
    }
    CUDA_MEMCPY2D cp = {0};
    if (frame.frameType == CU_EGL_FRAME_TYPE_ARRAY && frame.frame.pArray[0]) {
        cp.srcMemoryType = CU_MEMORYTYPE_ARRAY; cp.srcArray = frame.frame.pArray[0];
    } else if (frame.frameType == CU_EGL_FRAME_TYPE_PITCH && frame.frame.pPitch[0] && frame.pitch >= WIDTH * 4) {
        cp.srcMemoryType = CU_MEMORYTYPE_DEVICE; cp.srcDevice = (CUdeviceptr)(uintptr_t)frame.frame.pPitch[0]; cp.srcPitch = frame.pitch;
    } else { fprintf(stderr, "unsupported CUDA frame storage or pitch\n"); return -1; }
    cp.WidthInBytes = WIDTH * 4; cp.Height = HEIGHT;
    CopyArgs args = { .cp = cp, .copy = copy };
    PortalNvencExperimental experiment = { .fn = &engine->fn, .encoder = engine->encoder,
        .surface = engine->reg_res.registeredResource, .bitstream = engine->bitstream_buf,
        .dst = engine->cu_dptr, .width = WIDTH, .height = HEIGHT, .sync = sync,
        .output = engine->output_staging, .capacity = engine->output_staging_cap };
    const uint8_t *data = NULL; uint32_t len = 0;
    if (portal_nvenc_experimental_encode(&experiment, copy_surface, &args, &data, &len)) {
        fprintf(stderr, "NVENC copy/encode failed; lease and resources quarantined\n"); return -1;
    }
    CHECK(unreg(resource)); CHECK(sync());
    int idr = 0;
    for (size_t i = 0; i + 3 < len; i++)
        if (!data[i] && !data[i+1] && data[i+2] == 1 && (data[i+3] & 31) == 5) idr = 1;
    if (!idr) { fprintf(stderr, "encoded bitstream lacks IDR\n"); return -1; }
    int fd = open(output_path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0) { perror("create private bitstream"); return -1; }
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, data + off, len - off);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { perror("write bitstream"); close(fd); return -1; }
        off += (size_t)n;
    }
    if (close(fd)) { perror("close bitstream"); return -1; }
    printf("CUDA same-PCI %s frame copied to NVENC input; IDR bytes=%u; unregister and sync succeeded\n",
           frame.frameType == CU_EGL_FRAME_TYPE_ARRAY ? "ARRAY" : "PITCH", len);
#undef CHECK
    return 0;
}

/* Objects intentionally remain alive until process exit, including on errors.
 * Callbacks can revoke buffers; retaining objects cannot prevent revocation or
 * reuse by a real producer. This test uses only its own retained GBM allocations. */
static int pixels(const struct spa_video_info_raw *format, const struct spa_data *data, int producer) {
    struct pnspa_snapshot s = { .format = format->format, .width = format->size.width,
        .height = format->size.height, .modifier = format->modifier, .pixel_fd = (int)data->fd,
        .mapoffset = data->mapoffset, .offset = data->chunk->offset, .stride = data->chunk->stride };
    EGLint attrs[PNEGL_ATTR_COUNT];
    if (!pnegl_attrs(&s, attrs)) return -1;
    if (!producer) {
        PFNEGLDESTROYIMAGEKHRPROC destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
        if (!destroy_image || consumer_image != EGL_NO_IMAGE_KHR) return -1;
        consumer_image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
        if (consumer_image == EGL_NO_IMAGE_KHR) {
            fprintf(stderr, "consumer EGL DMA-BUF import error 0x%x\n", eglGetError()); return -1;
        }
        /* No consumer GL context or GL access: CUDA unregister and sync precede destruction. */
        if (cuda_copy(consumer_image)) return -1;
        if (!destroy_image(display, consumer_image)) {
            fprintf(stderr, "consumer EGL image destroy failed: 0x%x; lease retained\n", eglGetError()); return -1;
        }
        consumer_image = EGL_NO_IMAGE_KHR;
        fprintf(stderr, "consumer: CUDA source unregistered and synchronized; EGL image destroyed\n");
        return 0;
    }
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!eglBindAPI(EGL_OPENGL_ES_API)) return -1;
    EGLContext ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) return -1;
    EGLImageKHR image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "EGL DMA-BUF import error 0x%x\n", eglGetError()); return -1; }
    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    bind_image(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum error = glGetError(), status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (error != GL_NO_ERROR || status != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "EGL image framebuffer error=0x%x status=0x%x\n", error, status); return -1;
    }
    if (producer) {
        glEnable(GL_SCISSOR_TEST);
        const GLfloat colors[4][3] = {{1,0,0},{0,1,0},{0,0,1},{1,1,1}};
        for (int q = 0; q < 4; q++) {
            glScissor((q % 2) * WIDTH / 2, (q / 2) * HEIGHT / 2, WIDTH / 2, HEIGHT / 2);
            glClearColor(colors[q][0], colors[q][1], colors[q][2], 1);
            glClear(GL_COLOR_BUFFER_BIT);
        }
        if (glGetError() != GL_NO_ERROR) return -1;
        PFNEGLCREATESYNCKHRPROC create_sync = (void *)eglGetProcAddress("eglCreateSyncKHR");
        PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
        const char *ext = eglQueryString(display, EGL_EXTENSIONS);
        if (!ext || !strstr(ext, "EGL_ANDROID_native_fence_sync") || !create_sync || !dup_fence) {
            fprintf(stderr, "EGL native fence unavailable\n"); return -1;
        }
        const EGLint sa[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE };
        EGLSyncKHR sync = create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, sa);
        if (sync == EGL_NO_SYNC_KHR) { fprintf(stderr, "native fence create failed: 0x%x\n", eglGetError()); return -1; }
        glFlush();
        if (glGetError() != GL_NO_ERROR) return -1;
        int fence = dup_fence(display, sync);
        if (fence < 0) { fprintf(stderr, "native fence export failed: EGL=0x%x\n", eglGetError()); return -1; }
        struct sync_file_info fi = {0};
        if (ioctl(fence, SYNC_IOC_FILE_INFO, &fi) || !fi.num_fences || fi.status < 0) {
            fprintf(stderr, "native sync_file missing fence or in error: count=%u status=%d errno=%d\n", fi.num_fences, fi.status, errno); return -1;
        }
        fprintf(stderr, "producer: native sync_file fences=%u status=%d (completion may precede wait)\n", fi.num_fences, fi.status);
        struct allocation *a = atomic_load(&rendered);
        uint32_t binary;
        if (!a || drmSyncobjCreate(drmfd, 0, &binary) || drmSyncobjImportSyncFile(drmfd, binary, fence) ||
            drmSyncobjTransfer(drmfd, a->handle, 1, binary, 0, 0)) {
            perror("native fence import/transfer"); return -1;
        }
        atomic_store(&native_fence_fd, fence);
        fprintf(stderr, "producer: GL native sync_file imported and transferred to acquire=1; GL context retained\n");
    }
    return 0;
}

static int open_render_node(const char *path) {
    const char prefix[] = "/dev/dri/renderD";
    if (strncmp(path, prefix, sizeof(prefix) - 1)) goto invalid;
    const char *digits = path + sizeof(prefix) - 1;
    if (!*digits || strspn(digits, "0123456789") != strlen(digits)) goto invalid;
    int pin = open(path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
    if (pin < 0) { perror("pin render node"); return -1; }
    int fd = -1; struct stat before, after;
    char sys[128], resolved[PATH_MAX], subsystem[PATH_MAX], proc[64];
    if (fstat(pin, &before) || !S_ISCHR(before.st_mode)) { fprintf(stderr, "not a character device\n"); goto out; }
    snprintf(sys, sizeof(sys), "/sys/dev/char/%u:%u", major(before.st_rdev), minor(before.st_rdev));
    if (!realpath(sys, resolved)) { perror("resolve render sysfs"); goto out; }
    const char *base = strrchr(resolved, '/');
    if (!base || strcmp(base + 1, path + sizeof("/dev/dri/") - 1)) { fprintf(stderr, "render sysfs name mismatch\n"); goto out; }
    size_t len = strlen(sys);
    if (len + sizeof("/subsystem") > sizeof(sys)) { fprintf(stderr, "sysfs path too long\n"); goto out; }
    memcpy(sys + len, "/subsystem", sizeof("/subsystem"));
    if (!realpath(sys, subsystem) || strcmp(subsystem, "/sys/class/drm")) { fprintf(stderr, "render sysfs subsystem is not DRM\n"); goto out; }
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", pin);
    fd = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) { perror("open pinned render node"); goto out; }
    if (fstat(fd, &after) || !S_ISCHR(after.st_mode) || before.st_dev != after.st_dev ||
        before.st_ino != after.st_ino || before.st_rdev != after.st_rdev || drmGetNodeTypeFromFd(fd) != DRM_NODE_RENDER) {
        fprintf(stderr, "opened render node identity/type mismatch\n"); close(fd); fd = -1;
    }
out:
    close(pin);
    return fd;
invalid:
    fprintf(stderr, "refusing device: expected /dev/dri/renderD<digits> (no symlinks)\n");
    return -1;
}

/* Measure each actual exported allocation; no tiled size is inferred from pitch. */
static struct allocation *allocate(void) {
    struct allocation *a = calloc(1, sizeof(*a));
    if (!a) return NULL;
    a->fd = -1;
    const uint64_t modifier = TARGET_MOD;
    a->bo = gbm_bo_create_with_modifiers(gbm, WIDTH, HEIGHT, GBM_FORMAT_ARGB8888, &modifier, 1);
    if (!a->bo || gbm_bo_get_width(a->bo) != WIDTH || gbm_bo_get_height(a->bo) != HEIGHT ||
        gbm_bo_get_plane_count(a->bo) != 1 || gbm_bo_get_modifier(a->bo) != modifier ||
        (a->fd = gbm_bo_get_fd(a->bo)) < 0) {
        fprintf(stderr, "GBM tiled single-plane allocation/export failed or returned unexpected layout\n");
        if (a->bo) gbm_bo_destroy(a->bo);
        free(a); return NULL;
    }
    struct stat st;
    /* ParamBuffers size and stride are signed SPA integers. */
    if (fstat(a->fd, &st) || st.st_size <= 0 || (uint64_t)st.st_size > INT32_MAX) {
        fprintf(stderr, "invalid producer DMA-BUF fstat size\n"); close(a->fd); gbm_bo_destroy(a->bo); free(a); return NULL;
    }
    a->dev = st.st_dev; a->ino = st.st_ino;
    a->size = (uint32_t)st.st_size; a->stride = gbm_bo_get_stride(a->bo); a->offset = gbm_bo_get_offset(a->bo, 0);
    if (a->stride < WIDTH * 4 || a->stride > INT32_MAX || a->offset >= a->size || a->offset > INT32_MAX) {
        fprintf(stderr, "unexpected GBM plane layout\n"); close(a->fd); gbm_bo_destroy(a->bo); free(a); return NULL;
    }
    return a;
}

static int valid_chunk(const struct spa_buffer *b, const struct allocation *a) {
    if (!b || !a || !b->datas || !b->n_datas || !b->datas[0].chunk) return 0;
    const struct spa_data *d = &b->datas[0];
    const struct spa_chunk *c = d->chunk;
    return d->type == SPA_DATA_DmaBuf && d->maxsize == a->size && !d->mapoffset &&
        c->offset == a->offset && c->size == a->size - a->offset && c->size > 0 &&
        c->stride == (int32_t)a->stride && !(c->flags & (SPA_CHUNK_FLAG_EMPTY | SPA_CHUNK_FLAG_CORRUPTED));
}
static void add_buffer(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer) return;
    struct spa_buffer *b = buffer->buffer;
    struct spa_meta_sync_timeline *m;
    if (!b || b->n_datas != 3 || !b->datas || !b->datas[0].chunk ||
        !(b->datas[0].type & (1u << SPA_DATA_DmaBuf)) ||
        !(m = spa_buffer_find_meta_data(b, SPA_META_SyncTimeline, sizeof(*m)))) {
        fprintf(stderr, "producer: missing three blocks, DMA-BUF or timeline meta\n"); atomic_store(&failed, 1); return;
    }
    struct allocation *a = allocate();
    if (!a) { atomic_store(&failed, 1); return; }
    a->sync_fd = -1;
    if (a->size != layout->size || a->stride != layout->stride || a->offset != layout->offset ||
        drmSyncobjCreate(drmfd, 0, &a->handle) || drmSyncobjHandleToFD(drmfd, a->handle, &a->sync_fd)) {
        perror("producer allocation/syncobj"); atomic_store(&failed, 1); return;
    }
    buffer->user_data = a;
    struct spa_data *d = &b->datas[0];
    d->type = SPA_DATA_DmaBuf; d->flags = SPA_DATA_FLAG_READABLE; d->fd = a->fd;
    d->mapoffset = 0; d->maxsize = a->size; d->data = NULL;
    d->chunk->offset = a->offset; d->chunk->size = a->size - a->offset; d->chunk->stride = (int32_t)a->stride;
    d->chunk->flags = SPA_CHUNK_FLAG_NONE;
    for (unsigned i = 1; i < 3; i++) {
        b->datas[i].type = SPA_DATA_SyncObj; b->datas[i].flags = SPA_DATA_FLAG_READABLE;
        b->datas[i].fd = a->sync_fd;
    }
    *m = (struct spa_meta_sync_timeline){ .acquire_point = 1, .release_point = 2,
        .flags = SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE };
    fprintf(stderr, "producer: allocated DMA-BUF and same syncobj for slots 1/2\n");
}
static void remove_buffer(void *data, struct pw_buffer *buffer) {
    (void)data; (void)buffer; atomic_store(&failed, 1);
}
static void process(void *data);
static int invoke_producer(struct spa_loop *loop, bool async, uint32_t seq,
                           const void *data, size_t size, void *user_data) {
    (void)loop; (void)async; (void)seq; (void)data; (void)size;
    process(user_data);
    return 0;
}
static void process(void *data) {
    struct endpoint *e = data;
    if (atomic_load(&failed) || (!e->producer && held) ||
        (e->producer && (atomic_load(&producer_returned) ||
                         (atomic_load(&sent) && !atomic_load(&consumer_queued))))) return;
    struct pw_buffer *b;
    do {
        b = pw_stream_dequeue_buffer(e->stream);
        if (!b) return;
        /* Drain and retain unrelated producer buffers while awaiting the return. */
    } while (e->producer && atomic_load(&sent) && b->user_data != atomic_load(&rendered));
    if (e->producer) {
        struct allocation *a = b->user_data;
        if (atomic_load(&sent)) {
            struct spa_meta_sync_timeline *m = b->buffer ?
                spa_buffer_find_meta_data(b->buffer, SPA_META_SyncTimeline, sizeof(*m)) : NULL;
            struct spa_meta_header *header = b->buffer ?
                spa_buffer_find_meta_data(b->buffer, SPA_META_Header, sizeof(*header)) : NULL;
            uint64_t point = 0;
            int query = a ? drmSyncobjQuery(drmfd, &a->handle, &point, 1) : -1;
            if (!a || !valid_chunk(b->buffer, a) || !header || header->seq != CONSUMER_ACK_GENERATION ||
                (header->flags & SPA_META_HEADER_FLAG_CORRUPTED) || !m || m->acquire_point != 1 || m->release_point != 2 ||
                (m->flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE) || query || point != 2) {
                fprintf(stderr, "producer: invalid return: chunk=%d header=%d seq=%llu header_flags=0x%x meta=%d acquire=%llu release=%llu flags=0x%x query=%d point=%llu\n",
                        valid_chunk(b->buffer, a), header != NULL, (unsigned long long)(header ? header->seq : 0),
                        header ? header->flags : 0, m != NULL, (unsigned long long)(m ? m->acquire_point : 0),
                        (unsigned long long)(m ? m->release_point : 0), m ? m->flags : 0,
                        query, (unsigned long long)point);
                atomic_store(&failed, 1); return;
            }
            fprintf(stderr, "producer: rendered allocation returned, ACK generation and chunk valid, release scheduled and timeline=2\n");
            atomic_store(&producer_returned, 1);
            return; /* Do not reuse or requeue this allocation. */
        }
        if (!a) { atomic_store(&failed, 1); return; }
        struct spa_meta_header *header = b->buffer ?
            spa_buffer_find_meta_data(b->buffer, SPA_META_Header, sizeof(*header)) : NULL;
        if (!header || !valid_chunk(b->buffer, a)) {
            fprintf(stderr, "producer: missing header or invalid initial chunk\n"); atomic_store(&failed, 1); return;
        }
        *header = (struct spa_meta_header){ .seq = FRAME_GENERATION };
        atomic_store(&rendered, a);
        if (pixels(&e->format, &b->buffer->datas[0], 1)) {
            fprintf(stderr, "producer GL/native fence failed; resources retained\n"); atomic_store(&failed, 1); return;
        }
        atomic_store(&sent, 1);
        int r = pw_stream_queue_buffer(e->stream, b);
        if (r < 0) { fprintf(stderr, "producer queue failed: %d\n", r); atomic_store(&failed, 1); }
        else { atomic_store(&queue_succeeded, 1); fprintf(stderr, "producer queued buffer\n"); }
        return;
    }
    held = b;
    struct pnspa_snapshot snap;
    struct stat st;
    struct allocation *a = atomic_load(&rendered);
    struct spa_meta_header *header = b->buffer ?
        spa_buffer_find_meta_data(b->buffer, SPA_META_Header, sizeof(*header)) : NULL;
    if (atomic_load(&formats) != 3 || !a || !valid_chunk(b->buffer, a) || !header ||
        header->seq != FRAME_GENERATION || (header->flags & SPA_META_HEADER_FLAG_CORRUPTED) || !pnspa_snapshot(b->buffer, &e->format, TARGET_MOD, &snap) ||
        snap.format != SPA_VIDEO_FORMAT_BGRA || snap.width != WIDTH || snap.height != HEIGHT ||
        fstat(snap.pixel_fd, &st) || st.st_dev != a->dev || st.st_ino != a->ino ||
        st.st_size != a->size || snap.maxsize != a->size || snap.mapoffset ||
        snap.offset != a->offset || snap.size != a->size - a->offset || snap.stride != (int32_t)a->stride ||
        snap.acquire_point != 1 || snap.release_point != 2 ||
        snap.sync_flags != SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE ||
        !(b->buffer->datas[1].flags & SPA_DATA_FLAG_READABLE) || !(b->buffer->datas[2].flags & SPA_DATA_FLAG_READABLE)) {
        fprintf(stderr, "consumer: received buffer identity/format/meta validation failed\n"); atomic_store(&failed, 1); return;
    }
    int r = pnt_create(&timeline_owner, consumer_fd, snap.acquire_fd, snap.acquire_point,
                       snap.release_fd, snap.release_point, NULL, NULL);
    struct timespec now;
    if (r || clock_gettime(CLOCK_MONOTONIC, &now)) { fprintf(stderr, "consumer: pnt_create failed: %d\n", r); atomic_store(&failed, 1); return; }
    int64_t until = (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000;
    r = pnt_wait(timeline_owner, until);
    if (r) { fprintf(stderr, "consumer: acquire wait failed: %d\n", r); atomic_store(&failed, 1); return; }
    struct sync_file_info fi = {0};
    int fence_fd = atomic_load(&native_fence_fd);
    if (fence_fd < 0 || ioctl(fence_fd, SYNC_IOC_FILE_INFO, &fi) || !fi.num_fences || fi.status != 1) {
        fprintf(stderr, "consumer: producer sync_file not successfully completed: fences=%u status=%d\n", fi.num_fences, fi.status);
        atomic_store(&failed, 1); return;
    }
    fprintf(stderr, "consumer: received acquire fd waited at point 1; producer sync_file completed\n");
    struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(b->buffer, SPA_META_SyncTimeline, sizeof(*m));
    if ((r = pnt_gpu_begin(timeline_owner))) { fprintf(stderr, "consumer: GPU begin failed: %d\n", r); atomic_store(&failed, 1); return; }
    if (pixels(&e->format, &b->buffer->datas[0], 0)) {
        fprintf(stderr, "consumer: GPU/NVENC failed; lease retained\n"); atomic_store(&failed, 1); return;
    }
    /* This ACK relies on exclusive PipeWire buffer ownership, not authentication. */
    header->seq = CONSUMER_ACK_GENERATION;
    if ((r = pnt_gpu_complete(timeline_owner)) || (r = pnt_release(timeline_owner, m))) {
        fprintf(stderr, "consumer: GPU completion/release failed: %d; not retried\n", r); atomic_store(&failed, 1); return;
    }
    /* Import while the received fd is still borrowed, then destroy the owner before return. */
    uint32_t acquire_check;
    if (drmSyncobjFDToHandle(consumer_fd, snap.acquire_fd, &acquire_check)) {
        fprintf(stderr, "consumer: acquire handle import failed\n"); atomic_store(&failed, 1); return;
    }
    r = pnt_destroy(&timeline_owner);
    if (r) {
        fprintf(stderr, "consumer: timeline destroy failed: %d; owner retained until process exit\n", r);
        atomic_store(&failed, 1); return;
    }
    r = pw_stream_queue_buffer(e->stream, b);
    if (r < 0) { fprintf(stderr, "consumer: return queue failed: %d\n", r); atomic_store(&failed, 1); return; }
    /* b, metadata and borrowed fds are no longer ours. */
    atomic_store(&consumer_queued, 1);
    for (int i = 0; i < 200 && !atomic_load(&producer_returned) && !atomic_load(&failed); i++) {
        r = pw_loop_invoke(pw_thread_loop_get_loop(src.loop), invoke_producer, 0, NULL, 0, false, &src);
        if (r < 0) { fprintf(stderr, "consumer: producer drain invoke failed: %d\n", r); atomic_store(&failed, 1); break; }
        usleep(10000);
    }
    if (!atomic_load(&producer_returned) || atomic_load(&failed)) {
        fprintf(stderr, "consumer: producer return failed or timed out\n"); atomic_store(&failed, 1); return;
    }
    /* All timelines and signalers in this single-process synthetic test are ours.
     * After GPU completion and release, challenge the received acquire object:
     * no actor has scheduled or may signal the sentinel except this callback
     * using the producer's handle. This is not system-wide exclusivity proof. */
    uint64_t sentinel = UINT64_C(0x4000000000000123);
    uint64_t producer_before = 0, acquire_before = 0, acquire_after = 0;
    if (drmSyncobjQuery(drmfd, &a->handle, &producer_before, 1) ||
        drmSyncobjQuery(consumer_fd, &acquire_check, &acquire_before, 1) ||
        producer_before >= sentinel || acquire_before >= sentinel) {
        fprintf(stderr, "synthetic acquire identity challenge precondition failed\n"); atomic_store(&failed, 1); return;
    }
    if (drmSyncobjTimelineSignal(drmfd, &a->handle, &sentinel, 1) ||
        clock_gettime(CLOCK_MONOTONIC, &now)) {
        fprintf(stderr, "synthetic producer-handle sentinel signal failed\n"); atomic_store(&failed, 1); return;
    }
    until = (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000;
    if (drmSyncobjTimelineWait(consumer_fd, &acquire_check, &sentinel, 1, until,
                               DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, NULL) ||
        drmSyncobjQuery(consumer_fd, &acquire_check, &acquire_after, 1) || acquire_after < sentinel) {
        fprintf(stderr, "synthetic received acquire identity challenge failed\n"); atomic_store(&failed, 1); return;
    }
    fprintf(stderr, "consumer callback: received acquire followed producer-handle sentinel; synthetic exclusive signaller only\n");
    atomic_store(&received, 1);
}
static void param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    struct endpoint *e = data;
    if (id != SPA_PARAM_Format) return;
    int bit = e->producer ? 1 : 2;
    if (!param) { atomic_fetch_and(&formats, ~bit); return; }
    atomic_fetch_and(&formats, ~bit);
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0 || info.format != SPA_VIDEO_FORMAT_BGRA ||
        info.size.width != WIDTH || info.size.height != HEIGHT || info.framerate.num != 30 || info.framerate.denom != 1 ||
        !(info.flags & SPA_VIDEO_FLAG_MODIFIER) || (info.flags & SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED) ||
        info.modifier != TARGET_MOD) {
        fprintf(stderr, "unexpected negotiated video format\n"); atomic_store(&failed, 1); return;
    }
    e->format = info;
    atomic_fetch_or(&formats, bit);
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_pod_frame frame;
    spa_pod_builder_push_object(&b, &frame, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers);
    spa_pod_builder_add(&b,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(3),
        SPA_PARAM_BUFFERS_size, SPA_POD_Int((int32_t)layout->size),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int32_t)layout->stride),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf), 0);
    spa_pod_builder_prop(&b, SPA_PARAM_BUFFERS_metaType, SPA_POD_PROP_FLAG_MANDATORY);
    spa_pod_builder_int(&b, (1 << SPA_META_SyncTimeline) | (1 << SPA_META_Header));
    const struct spa_pod *params[3];
    params[0] = spa_pod_builder_pop(&b, &frame);
    params[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_SyncTimeline),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_sync_timeline)));
    params[2] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_header)));
    int rc = pw_stream_update_params(e->stream, params, 3);
    fprintf(stderr, "STAGE: %s requested 3 blocks and mandatory SyncTimeline/Header: update=%d\n", e->producer ? "producer" : "consumer", rc);
    if (rc < 0) atomic_store(&failed, 1);
}
static void state(void *data, enum pw_stream_state old, enum pw_stream_state now, const char *error) {
    (void)data; (void)old;
    if (now == PW_STREAM_STATE_ERROR) { fprintf(stderr, "stream error: %s\n", error ? error : "unknown"); atomic_store(&failed, 1); }
}
static const struct pw_stream_events events = { PW_VERSION_STREAM_EVENTS, .state_changed = state, .param_changed = param_changed, .add_buffer = add_buffer, .remove_buffer = remove_buffer, .process = process };
static int setup(struct endpoint *e, const char *name, int producer) {
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA, .flags = SPA_VIDEO_FLAG_MODIFIER,
        .modifier = TARGET_MOD, .size = SPA_RECTANGLE(WIDTH, HEIGHT), .framerate = SPA_FRACTION(30, 1) };
    e->producer = producer; e->loop = pw_thread_loop_new(name, NULL);
    if (!e->loop) return -1;
    e->stream = pw_stream_new_simple(pw_thread_loop_get_loop(e->loop), name,
        pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, producer ? "Capture" : "Playback",
                          PW_KEY_MEDIA_ROLE, "Test", PW_KEY_NODE_NAME, name, PW_KEY_NODE_AUTOCONNECT, "false", NULL), &events, e);
    if (!e->stream) return -1;
    const struct spa_pod *p = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    if (pw_stream_connect(e->stream, producer ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT, PW_ID_ANY,
        producer ? PW_STREAM_FLAG_ALLOC_BUFFERS : PW_STREAM_FLAG_NONE, &p, 1) < 0) return -1;
    return pw_thread_loop_start(e->loop);
}
int main(int argc, char **argv) {
    int rc = 1, started_src = 0, started_dst = 0;
    if (argc != 3) { fprintf(stderr, "usage: %s render-node output\n", argv[0]); return 2; }
    output_path = argv[2];
    drmfd = open_render_node(argv[1]);
    if (drmfd < 0) goto done;
    consumer_fd = open_render_node(argv[1]);
    struct stat a, b;
    if (consumer_fd < 0 || fstat(drmfd, &a) || fstat(consumer_fd, &b) ||
        a.st_dev != b.st_dev || a.st_ino != b.st_ino || a.st_rdev != b.st_rdev) {
        fprintf(stderr, "independent consumer DRM fd identity mismatch\n"); goto done;
    }
    gbm = gbm_create_device(drmfd);
    if (!gbm || !(layout = allocate())) { fprintf(stderr, "GBM target allocation failed\n"); goto done; }
    printf("GBM target layout size=%u stride=%u offset=%u\n", layout->size, layout->stride, layout->offset);
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
    bind_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    EGLint n = 0;
    const EGLint cfg[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    if (!platform || !create_image || !bind_image ||
        (display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL)) == EGL_NO_DISPLAY ||
        !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API) ||
        !eglChooseConfig(display, cfg, &config, 1, &n) || n != 1) {
        fprintf(stderr, "EGL GBM initialization failed\n"); goto done;
    }
    pw_init(NULL, NULL);
    if (setup(&src, "uu-dmabuf-producer", 1) < 0) goto cleanup;
    started_src = 1;
    if (setup(&dst, "uu-dmabuf-consumer", 0) < 0) goto cleanup;
    started_dst = 1;
    puts("READY"); fflush(stdout);
    for (int i = 0; i < 500 && !atomic_load(&received) && !atomic_load(&failed); i++) usleep(10000);
    if (started_dst) { pw_thread_loop_stop(dst.loop); started_dst = 0; }
    if (started_src) { pw_thread_loop_stop(src.loop); started_src = 0; }
    if (!atomic_load(&failed) && atomic_load(&received) && atomic_load(&producer_returned) && atomic_load(&queue_succeeded) && atomic_load(&formats) == 3) {
        puts("OBSERVED: native fence, CUDA/NVENC and producer return; software decode pending; no Portal claim"); rc = 0;
    } else fprintf(stderr, "FAIL: private timeline roundtrip failed or timed out\n");
cleanup:
    if (started_dst) pw_thread_loop_stop(dst.loop);
    if (started_src) pw_thread_loop_stop(src.loop);
    if (atomic_load(&failed)) rc = 1;
done:
    fflush(NULL); _exit(rc);
}
