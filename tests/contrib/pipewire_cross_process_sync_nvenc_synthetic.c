/* Independent PipeWire clients; only DMA-BUF metadata crosses the control socket. */
#define main old_main
#include "cross_process_nvenc_dmabuf_base.inc"
#undef main
#include <sys/socket.h>
#include <errno.h>
#include <poll.h>
#include <time.h>
#include "../../c_engine/portal_nvenc_spa.c"
#include "../../c_engine/portal_nvenc_timeline.c"
#include "../../c_engine/nvenc_engine.c"
#include "../../contrib/portal-nvenc-experimental.h"
#include <dlfcn.h>
#include <strings.h>
#include <sys/socket.h>
#include <errno.h>
#include <poll.h>
#include <time.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include "../../c_engine/portal_nvenc_egl_attrs.h"
#include "../../c_engine/portal_nvenc_image.h"
#include "../../c_engine/portal_nvenc_device.h"
#ifdef UU_TEST_FLAGLESS
#define TEST_RELEASE_FLAGS 0u
#define TEST_PROTOCOL 1
#else
#define TEST_RELEASE_FLAGS SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE
#define TEST_PROTOCOL 0
#endif


#ifdef UU_TEST_PORTAL_BRIDGE
/* Real Portal capture callbacks over a private daemon; no consent/session open. */
#include "../../c_engine/portal_engine.c"
static struct uu_portal *bridge;
static struct uu_portal_async_lease *bridge_lease;
static UuPortalAsyncFrame bridge_frame;
#endif

/* Self-created BOs and exclusive system-wide signalers are assumed. Process
 * exit or timeout does not isolate GPU access or prove Portal lease safety. */
static EGLDisplay display;
static EGLConfig config;
static PFNEGLCREATEIMAGEKHRPROC create_image;
static PFNEGLDESTROYIMAGEKHRPROC destroy_image;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
static PFNEGLCREATESYNCKHRPROC create_sync;
static PFNEGLDESTROYSYNCKHRPROC destroy_sync;
static PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence;
static EGLContext producer_ctx;
static EGLImageKHR producer_image;
static EGLSyncKHR producer_sync;
static GLuint producer_tex, producer_fbo;
static uint32_t producer_handle, acquire_check;

enum pixel_result { PIXEL_OK, PIXEL_FAILED_FINISHED, PIXEL_UNKNOWN };
static void *preflight_cuda;
static CUdevice preflight_device;
static const char *output_path;
/* Written only while process callbacks are paused by a loop-thread barrier. */
static unsigned frame_index, frame_count = 1, disconnect_after;
static atomic_int removed_buffers;
static int disconnect_worker;
static pthread_mutex_t gate_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t gate_changed = PTHREAD_COND_INITIALIZER;
static atomic_int gate_requested;
static int gate_open;
/* Deterministic checkpoint after copy returns, before explicit CUDA sync.
 * This proves event-loop responsiveness, not that hardware is still busy. */
static int wait_loop_checkpoint(void) {
    struct timespec until;
    if (clock_gettime(CLOCK_REALTIME, &until)) return -1;
    until.tv_sec += 3;
    pthread_mutex_lock(&gate_lock);
    gate_open = 0;
    atomic_store(&gate_requested, 1);
    int rc = 0;
    while (!gate_open && !rc) rc = pthread_cond_timedwait(&gate_changed, &gate_lock, &until);
    pthread_mutex_unlock(&gate_lock);
    return rc ? -1 : 0;
}
static NvencEngine *persistent_engine;
static PortalNvencExperimental persistent_experiment;
#ifdef UU_TEST_NATIVE_ENCODER
static int native_source_complete;
static __attribute__((unused)) int native_copy_surface(void *opaque, uint64_t dst, uint32_t pitch) ;
#endif
static int bitstream_fd = -1;
#define ACQUIRE_POINT ((uint64_t)frame_index * 2 + 1)
#define RELEASE_POINT (ACQUIRE_POINT + 1)
static void stage(const char *name, const char *phase) {
    fprintf(stderr, "synthetic %s %s\n", name, phase);
    fflush(stderr);
}
static PNVENCENCODEPICTURE real_encode;
static PNVENCLOCKBITSTREAM real_lock;
static PNVENCUNLOCKBITSTREAM real_unlock;
static NVENCSTATUS NVENCAPI trace_encode(void *e, NV_ENC_PIC_PARAMS *p) {
    stage("encode", "begin"); NVENCSTATUS r = real_encode(e, p); stage("encode", "end"); return r;
}
static NVENCSTATUS NVENCAPI trace_lock(void *e, NV_ENC_LOCK_BITSTREAM *p) {
    stage("lock", "begin"); NVENCSTATUS r = real_lock(e, p); stage("lock", "end"); return r;
}
static NVENCSTATUS NVENCAPI trace_unlock(void *e, NV_ENC_OUTPUT_PTR p) {
    stage("unlock", "begin"); NVENCSTATUS r = real_unlock(e, p); stage("unlock", "end"); return r;
}
static __attribute__((unused)) NvencEngine *probe_nvenc_create(const NvencConfig *config) {
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
    stage("nvEncInitializeEncoder", "begin");
    NV_CHECK(e->fn.nvEncInitializeEncoder(e->encoder, &params));
    stage("nvEncInitializeEncoder", "end");
    e->frame_bytes = (size_t)e->width * e->height * 4;
    CU_CHECK(e->cuMemAlloc(&e->cu_dptr, e->frame_bytes));
    e->reg_res.version = NV_ENC_REGISTER_RESOURCE_VER; e->reg_res.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    e->reg_res.width = e->width; e->reg_res.height = e->height; e->reg_res.pitch = e->width * 4;
    e->reg_res.resourceToRegister = (void *)e->cu_dptr; e->reg_res.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
    stage("NVENC register resource", "begin");
    NV_CHECK(e->fn.nvEncRegisterResource(e->encoder, &e->reg_res));
    stage("NVENC register resource", "end");
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
    a->cp.dstDevice = dst; a->cp.dstPitch = pitch;
    stage("CUDA copy", "begin");
    CUresult r = a->copy(&a->cp);
    stage("CUDA copy", "end");
    return r != CUDA_SUCCESS || wait_loop_checkpoint();
}

/* Only after every submitted encode and source read completed successfully.
 * Called after joining the frame worker, or by that worker at its final frame. No recovery from uncertain GPU completion. */
#ifdef UU_TEST_NATIVE_ENCODER
static __attribute__((unused)) int native_copy_surface(void *opaque, uint64_t dst, uint32_t pitch) {
    return copy_surface(opaque, (CUdeviceptr)dst, pitch);
}
#endif
static int shutdown_idle_encoder(void) {
    NvencEngine *engine = persistent_engine;
    PortalNvencExperimental *p = &persistent_experiment;
#ifdef UU_TEST_NATIVE_ENCODER
    if (!engine || engine->gpu_busy || engine->gpu_quarantined || !native_source_complete || !p->sync) return -1;
#else
    if (!engine || p->busy || p->quarantined || !p->source_complete || !p->sync) return -1;
#endif
#define CU_CLEAN(call) do { CUresult r_ = (call); if (r_ != CUDA_SUCCESS) { fprintf(stderr, "idle cleanup %s failed: %d\n", #call, r_); return -1; } } while (0)
#define NV_CLEAN(call) do { NVENCSTATUS r_ = (call); if (r_ != NV_ENC_SUCCESS) { fprintf(stderr, "idle cleanup %s failed: %d\n", #call, r_); return -1; } } while (0)
    CU_CLEAN(engine->cuCtxPushCurrent(engine->cu_ctx));
    CU_CLEAN(p->sync());
    NV_CLEAN(engine->fn.nvEncDestroyBitstreamBuffer(engine->encoder, engine->bitstream_buf));
    NV_CLEAN(engine->fn.nvEncUnregisterResource(engine->encoder, engine->reg_res.registeredResource));
    NV_CLEAN(engine->fn.nvEncDestroyEncoder(engine->encoder));
    CU_CLEAN(engine->cuMemFree(engine->cu_dptr));
    CU_CLEAN(p->sync());
    CUcontext popped = NULL;
    CU_CLEAN(engine->cuCtxPopCurrent(&popped));
    if (popped != engine->cu_ctx) return -1;
    CU_CLEAN(engine->cuDevicePrimaryCtxRelease(engine->dev));
#ifndef UU_TEST_NATIVE_ENCODER
    if (dlclose(engine->nvenc_lib) || dlclose(preflight_cuda)) return -1;
#endif
    free(engine->output_staging); free(engine);
    persistent_engine = NULL; preflight_cuda = NULL;
    if (bitstream_fd < 0 || close(bitstream_fd)) return -1;
    bitstream_fd = -1;
    fprintf(stderr, "PERSISTENT_ENCODER_CLEANUP_COMPLETED\n");
#undef CU_CLEAN
#undef NV_CLEAN
    return 0;
}

/* Frame CUDA calls, including initialization, run on the consumer worker.
 * Unknown driver completion retains resources. The loop alone decides whether
 * a completed worker still owns a publishable lease. */
static int cuda_copy(EGLImageKHR image) {
    void *lib = preflight_cuda ? preflight_cuda : dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
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
#define CHECK(call) do { CUresult r_ = (call); if (r_ != CUDA_SUCCESS) { fprintf(stderr, "%s failed: %d; GPU completion/lease state unknown (synthetic only)\n", #call, r_); return -1; } } while (0)
    drmDevicePtr d = NULL; char drm[32], cuda[32] = {0}; CUdevice dev;
    if (drmGetDevice2(drmfd, 0, &d) || !d || d->bustype != DRM_BUS_PCI || !d->businfo.pci) {
        if (d) drmFreeDevice(&d);
        fprintf(stderr, "DRM PCI identity unavailable\n"); return -1;
    }
    snprintf(drm, sizeof(drm), "%04x:%02x:%02x.%x", d->businfo.pci->domain, d->businfo.pci->bus, d->businfo.pci->dev, d->businfo.pci->func);
    drmFreeDevice(&d);
    int count = 0, matches = 0;
    stage("cuInit", "begin"); CHECK(init(0)); stage("cuInit", "end");
    CHECK(device_count(&count));
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
    if (!persistent_engine) {
        stage("NVENC initialization", "begin");
#ifdef UU_TEST_NATIVE_ENCODER
        persistent_engine = nvenc_create(&config);
        if (persistent_engine && persistent_engine->dev != preflight_device) {
            fprintf(stderr, "native encoder DRM/CUDA device mismatch\n"); return -1;
        }
#else
        persistent_engine = probe_nvenc_create(&config);
#endif
        stage("NVENC initialization", "end");
    }
    NvencEngine *engine = persistent_engine;
    if (!engine) { fprintf(stderr, "NVENC initialization failed; GPU completion/lease state unknown (synthetic only)\n"); return -1; }
    CHECK(engine->cuCtxPushCurrent(engine->cu_ctx));
#ifndef UU_TEST_NATIVE_EGL
    CUeglFrame frame = {0};
    CUgraphicsResource resource = NULL;
    stage("CUDA EGL register", "begin"); CHECK(reg(&resource, image, 0)); stage("CUDA EGL register", "end");
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
#endif
    if (!frame_index) {
        real_encode = engine->fn.nvEncEncodePicture; engine->fn.nvEncEncodePicture = trace_encode;
        real_lock = engine->fn.nvEncLockBitstream; engine->fn.nvEncLockBitstream = trace_lock;
        real_unlock = engine->fn.nvEncUnlockBitstream; engine->fn.nvEncUnlockBitstream = trace_unlock;
        persistent_experiment = (PortalNvencExperimental){ .fn = &engine->fn, .encoder = engine->encoder,
            .surface = engine->reg_res.registeredResource, .bitstream = engine->bitstream_buf,
            .dst = engine->cu_dptr, .width = WIDTH, .height = HEIGHT, .sync = sync,
            .output = engine->output_staging, .capacity = engine->output_staging_cap };
    }
#ifndef UU_TEST_NATIVE_ENCODER
    PortalNvencExperimental *experiment = &persistent_experiment;
#endif
    const uint8_t *data = NULL; uint32_t len = 0;
#ifdef UU_TEST_NATIVE_ENCODER
#ifdef UU_TEST_NATIVE_EGL
    int encoded = nvenc_encode_egl(engine, image, drm, 1, &data, &len, &native_source_complete);
#else
    int encoded = nvenc_encode_gpu(engine, native_copy_surface, &args, 1, &data, &len, &native_source_complete);
#endif
    fprintf(stderr, "NATIVE_GPU_ENCODE_RESULT result=%d complete=%d\n", encoded, native_source_complete);
#else
    int encoded = portal_nvenc_experimental_encode(experiment, copy_surface, &args, &data, &len);
#endif
    if (encoded) {
        fprintf(stderr, "NVENC copy/encode failed; GPU completion/lease state unknown (synthetic only)\n"); return -1;
    }
    /* Successful blocking lock/unlock and unmap establish NVENC completion;
     * copy synchronization is separately recorded by the experiment. */
    if (
#ifdef UU_TEST_NATIVE_ENCODER
        engine->gpu_busy || engine->gpu_quarantined || !native_source_complete ||
#else
        experiment->busy || experiment->quarantined || !experiment->source_complete ||
#endif
        data != engine->output_staging || !len || len > engine->output_staging_cap) {
        fprintf(stderr, "unexpected encode state; GPU completion/lease state UNKNOWN\n"); return -1;
    }
    CHECK(sync());
#ifndef UU_TEST_NATIVE_EGL
    CHECK(unreg(resource));
#endif
    CHECK(sync());
    int idr = 0;
    for (size_t i = 0; i + 3 < len; i++)
        if (!data[i] && !data[i+1] && data[i+2] == 1 && (data[i+3] & 31) == 5) idr = 1;
    if (!idr) { fprintf(stderr, "encoded bitstream lacks IDR\n"); return -1; }
    if (!frame_index)
        bitstream_fd = open(output_path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    int fd = bitstream_fd;
    if (fd < 0) { perror("create private bitstream"); return -1; }
    size_t off = 0;
    while (off < len) {
        ssize_t n = write(fd, data + off, len - off);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { perror("write bitstream"); close(fd); return -1; }
        off += (size_t)n;
    }
    fprintf(stderr, "ENCODED_FRAME index=%u bytes=%u\n", frame_index, len);
    if (frame_index + 1 < frame_count) {
        CUcontext popped = NULL;
        CHECK(engine->cuCtxPopCurrent(&popped));
        if (popped != engine->cu_ctx) return -1;
        return 0;
    }
    CUcontext popped = NULL;
    CHECK(engine->cuCtxPopCurrent(&popped));
    if (popped != engine->cu_ctx || shutdown_idle_encoder()) return -1;
    printf("CUDA same-PCI %s frame copied to NVENC input; IDR bytes=%u; unregister and sync succeeded\n",
           #ifdef UU_TEST_NATIVE_EGL
           "native-validated",
#else
           frame.frameType == CU_EGL_FRAME_TYPE_ARRAY ? "ARRAY" : "PITCH",
#endif
           len);
#undef CHECK
    return 0;
}



static int force_mismatch;
static int has_extension(const char *extensions, const char *name) {
    if (!extensions) return 0;
    size_t n = strlen(name);
    const char *p = extensions;
    while ((p = strstr(p, name))) {
        if ((p == extensions || p[-1] == ' ') && (p[n] == ' ' || p[n] == '\0')) return 1;
        p += n;
    }
    return 0;
}
static int gl_errors(int *unknown) {
    int bad = 0;
    for (int i = 0; i < 32; i++) {
        GLenum error = glGetError();
        if (error == GL_NO_ERROR) return bad;
        fprintf(stderr, "GL operation error 0x%x\n", error);
        bad = 1;
        if (error == 0x0507 /* GL_CONTEXT_LOST */) *unknown = 1;
    }
    *unknown = 1;
    return 1;
}
static enum pixel_result pixels(const struct spa_video_info_raw *format, const struct spa_data *data, int producer) {
    struct pnspa_snapshot s = { .format = format->format, .width = format->size.width,
        .height = format->size.height, .modifier = format->modifier, .pixel_fd = (int)data->fd,
        .mapoffset = data->mapoffset, .offset = data->chunk->offset, .stride = data->chunk->stride };
    stage(producer ? "producer EGL import" : "consumer EGL import", "begin");
    EGLint attrs[PNEGL_ATTR_COUNT];
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!pnegl_attrs(&s, attrs) || !eglBindAPI(EGL_OPENGL_ES_API)) goto egl_unknown;
    EGLContext ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
        goto egl_unknown;
    struct pnegl_image image_owner = {0};
    EGLImageKHR image;
    if (!producer) {
        if (pnegl_image_create(&image_owner, display, &s, create_image, destroy_image)) goto egl_unknown;
        image = image_owner.image;
    } else image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) goto egl_unknown;
    stage(producer ? "producer EGL import" : "consumer EGL import", "end");
    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    bind_image(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    int unknown = 0;
    int injected = 0;
    int bad = status != GL_FRAMEBUFFER_COMPLETE;
    if (bad) fprintf(stderr, "EGL image framebuffer status=0x%x\n", status);
    bad |= gl_errors(&unknown);
    if (!bad && producer) {
        glEnable(GL_SCISSOR_TEST);
        const GLfloat colors[4][3] = {{1,0,0},{0,1,0},{0,0,1},{1,1,1}};
        for (int q = 0; q < 4; q++) {
            glScissor((q % 2) * WIDTH / 2, (q / 2) * HEIGHT / 2, WIDTH / 2, HEIGHT / 2);
            glClearColor(colors[(q + frame_index) % 4][0], colors[(q + frame_index) % 4][1], colors[(q + frame_index) % 4][2], 1);
            glClear(GL_COLOR_BUFFER_BIT);
        }
    } else if (!bad) {
        if (cuda_copy(image)) {
            fprintf(stderr, "CUDA failure; GPU completion unknown (synthetic only)\n");
            return PIXEL_UNKNOWN;
        }
        if (force_mismatch) {
            fprintf(stderr, "INJECTED_MISMATCH_AFTER_ENCODE\n");
            injected = 1;
        }
    }
    /* Producer retains GL objects until the consumer handshake, without glFinish. */
    bad |= gl_errors(&unknown);
    if (producer) {
        if (bad || unknown) return PIXEL_UNKNOWN;
        const EGLint sa[] = { EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE };
        EGLSyncKHR sync = create_sync(display, EGL_SYNC_NATIVE_FENCE_ANDROID, sa);
        if (sync == EGL_NO_SYNC_KHR) goto egl_unknown;
        glFlush();
        if (gl_errors(&unknown) || unknown) goto egl_unknown;
        int fence = dup_fence(display, sync);
        uint32_t binary;
        if (fence < 0 || drmSyncobjCreate(drmfd, 0, &binary) ||
            drmSyncobjImportSyncFile(drmfd, binary, fence) ||
            drmSyncobjTransfer(drmfd, producer_handle, ACQUIRE_POINT, binary, 0, 0) ||
            close(fence) || drmSyncobjDestroy(drmfd, binary)) goto egl_unknown;
        fprintf(stderr, "NATIVE_FENCE_TRANSFERRED_TO_ACQUIRE\n");
        producer_ctx = ctx; producer_image = image; producer_sync = sync;
        producer_tex = tex; producer_fbo = fbo;
        if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT)) goto egl_unknown;
        return PIXEL_OK;
    }
    glFinish();
    if (glGetError() != GL_NO_ERROR) unknown = 1;
    if (unknown) {
        fprintf(stderr, "GPU completion unknown; BO retention after process exit/timeout is not proven (synthetic only)\n");
        return PIXEL_UNKNOWN;
    }
    if (bad) return PIXEL_UNKNOWN;
    /* On this callback thread, with the importing context current. This does
     * not prove driver implementation correctness or real Portal revocation. */
    glDeleteFramebuffers(1, &fbo);
    glDeleteTextures(1, &tex);
    if (gl_errors(&unknown)) return PIXEL_UNKNOWN;
    glFinish();
    if (gl_errors(&unknown) || unknown || pnegl_image_destroy_completed(&image_owner, 1) ||
        !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT) ||
        !eglDestroyContext(display, ctx)) goto egl_unknown;
    return injected ? PIXEL_FAILED_FINISHED : PIXEL_OK;
egl_unknown:
    fprintf(stderr, "EGL initialization/import failed (error 0x%x); GPU completion unknown (synthetic only)\n", eglGetError());
    return PIXEL_UNKNOWN;
}


static int consumer_drm = -1;
static struct pnt_ctx *owner;
static struct spa_video_info_raw fixed_format;
static uint64_t sentinel = UINT64_C(0x4000000000000123);
struct sync_allocation { struct allocation *allocation; uint32_t handle; int fd; };


struct record { uint64_t pid, dev, ino, size, stride, modifier, format, width, height; };
static int control_fd;
static atomic_int queued, queue_succeeded, negative_finished, return_requested, producer_returned;
static struct pw_buffer *submitted;
static int reuse_submitted;
static atomic_int frame_paused;
#define FRAME_GENERATION (UINT64_C(17) + (uint64_t)frame_index * 2)
#define ACK_GENERATION (FRAME_GENERATION + 1)
static pid_t producer_pid;
static int wait_message(const char *expected) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    int64_t deadline = (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000 + 15000;
    for (;;) {
        if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
        int64_t remaining = deadline - ((int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
        if (remaining <= 0) return -1;
        struct pollfd p = { .fd = control_fd, .events = POLLIN | POLLRDHUP };
        int n = poll(&p, 1, (int)remaining);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0 || (p.revents & (POLLERR | POLLNVAL))) return -1;
        char message[4];
        ssize_t received_bytes = recv(control_fd, message, sizeof(message), MSG_DONTWAIT | MSG_TRUNC);
        if (received_bytes == 4 && !memcmp(message, expected, 4)) return 0;
        if (received_bytes < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) continue;
        return -1;
    }
}
static int receive_record(struct record *r) {
    char ancillary[CMSG_SPACE(sizeof(struct ucred))];
    struct iovec iov = { .iov_base = r, .iov_len = sizeof(*r) };
    struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1,
                          .msg_control = ancillary, .msg_controllen = sizeof(ancillary) };
    ssize_t n = recvmsg(control_fd, &msg, MSG_DONTWAIT | MSG_TRUNC);
    if (n != sizeof(*r) || (msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC))) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (!c || c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_CREDENTIALS ||
        c->cmsg_len != CMSG_LEN(sizeof(struct ucred)) || CMSG_NXTHDR(&msg, c)) return -1;
    struct ucred cred;
    memcpy(&cred, CMSG_DATA(c), sizeof(cred));
    return cred.pid == producer_pid && r->pid == (uint64_t)producer_pid && producer_pid != getpid() ? 0 : -1;
}
static void sync_add(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer) return;
    struct spa_buffer *b = buffer->buffer;
    struct spa_meta_sync_timeline *m;
    if (!b || b->n_datas != 3 || !b->datas || !b->datas[0].chunk ||
        !(b->datas[0].type & (1u << SPA_DATA_DmaBuf)) ||
        !(m = spa_buffer_find_meta_data(b, SPA_META_SyncTimeline, sizeof(*m)))) goto bad;
    uint32_t s, z;
    struct sync_allocation *x = calloc(1, sizeof(*x));
    if (!x || !(x->allocation = allocate(&s, &z)) || s != stride || z != size ||
        drmSyncobjCreate(drmfd, 0, &x->handle) || drmSyncobjHandleToFD(drmfd, x->handle, &x->fd)) goto bad;
    buffer->user_data = x;
    struct spa_data *d = &b->datas[0];
    d->type = SPA_DATA_DmaBuf; d->flags = SPA_DATA_FLAG_READABLE; d->fd = x->allocation->fd;
    d->mapoffset = 0; d->maxsize = size; d->data = NULL;
    d->chunk->offset = 0; d->chunk->size = size; d->chunk->stride = (int32_t)stride; d->chunk->flags = 0;
    for (unsigned i = 1; i < 3; i++) {
        b->datas[i].type = SPA_DATA_SyncObj; b->datas[i].flags = SPA_DATA_FLAG_READABLE; b->datas[i].fd = x->fd;
    }
    *m = (struct spa_meta_sync_timeline){ .acquire_point = 1, .release_point = 2,
        .flags = TEST_RELEASE_FLAGS };
    return;
bad: atomic_store(&failed, 1);
}
static void sync_param(void *data, uint32_t id, const struct spa_pod *param) {
    struct endpoint *e = data;
    if (id != SPA_PARAM_Format) return;
    int bit = e->producer ? 1 : 2;
    atomic_fetch_and(&formats, ~bit);
    if (!param) {
#ifdef UU_TEST_PORTAL_BRIDGE
        if (!e->producer && bridge) stream_format(bridge, id, param);
#endif
        return;
    }
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0 || info.format != SPA_VIDEO_FORMAT_BGRA ||
        info.size.width != WIDTH || info.size.height != HEIGHT || info.framerate.num != 30 || info.framerate.denom != 1 ||
        !(info.flags & SPA_VIDEO_FLAG_MODIFIER) || (info.flags & SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED) || info.modifier != modifier) {
        atomic_store(&failed, 1); return;
    }
    fixed_format = info; atomic_fetch_or(&formats, bit);
#ifdef UU_TEST_PORTAL_BRIDGE
    if (!e->producer) {
        stream_format(bridge, id, param);
        if (bridge->error) atomic_store(&failed, 1);
        else fprintf(stderr, "PORTAL_ENGINE_FORMAT_NEGOTIATED\n");
        return;
    }
#endif
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_pod_frame f;
    spa_pod_builder_push_object(&b, &f, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers);
    spa_pod_builder_add(&b, SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(3), SPA_PARAM_BUFFERS_size, SPA_POD_Int((int32_t)size),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int32_t)stride),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf), 0);
    spa_pod_builder_prop(&b, SPA_PARAM_BUFFERS_metaType, SPA_POD_PROP_FLAG_MANDATORY);
    /* Header is only a harness ACK marker, not part of the Portal contract. */
#ifdef UU_TEST_PORTAL_BRIDGE
    spa_pod_builder_int(&b, 1 << SPA_META_SyncTimeline);
#else
    spa_pod_builder_int(&b, (1 << SPA_META_SyncTimeline) | (1 << SPA_META_Header));
#endif
    const struct spa_pod *params[3]; params[0] = spa_pod_builder_pop(&b, &f);
    params[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_SyncTimeline),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_sync_timeline)));
    params[2] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_Header),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_header)));
    if (pw_stream_update_params(e->stream, params, 3) < 0) atomic_store(&failed, 1);
}
/* This controlled producer sets exactly READABLE on every block. Require the
 * same on receipt; no inference about flags from other PipeWire producers. */
static int valid_data_flags(const struct spa_buffer *b) {
    if (!b || b->n_datas != 3 || !b->datas) return 0;
    for (unsigned i = 0; i < 3; i++)
        if (b->datas[i].flags != SPA_DATA_FLAG_READABLE) return 0;
    return 1;
}
/* Forge descriptors in a private copy, never mutate the live PipeWire lease. */
static int flags_negative_test(const struct spa_buffer *b) {
    struct spa_data data[3];
    if (!valid_data_flags(b)) return 0;
    memcpy(data, b->datas, sizeof(data));
    struct spa_buffer copy = *b; copy.datas = data;
    for (unsigned i = 0; i < 3; i++) {
        data[i].flags = 0;
        if (valid_data_flags(&copy)) return 0;
        data[i].flags = SPA_DATA_FLAG_READABLE | (1u << 31);
        if (valid_data_flags(&copy)) return 0;
        data[i].flags = SPA_DATA_FLAG_READABLE;
    }
    return valid_data_flags(&copy);
}
struct gpu_job {
    struct spa_video_info_raw format;
    struct spa_data data;
    struct spa_chunk chunk;
    enum pixel_result result;
};
static struct gpu_job job;
static pthread_t worker;
static int worker_started; /* Loop thread, or main after loop stop. */
static atomic_int worker_done;
static int worker_cancelled(void *unused) {
    (void)unused;
#ifdef UU_TEST_PORTAL_BRIDGE
    if (bridge_lease && uu_portal_async_cancelled(bridge_lease)) return 1;
#endif
    return atomic_load(&failed);
}
static void *gpu_worker(void *unused) {
    (void)unused;
    struct timespec now;
    job.result = PIXEL_UNKNOWN;
    int r = clock_gettime(CLOCK_MONOTONIC, &now) ? -EIO :
        pnt_wait_cancelable(owner, (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000,
                            worker_cancelled, NULL, NULL);
    if (!r && atomic_load(&failed)) r = -ECANCELED;
    if (!r) r = pnt_gpu_begin(owner);
    if (r) {
        /* No source access was submitted. Only this worker touches owner until
         * joined; callbacks cancel through failed, never pnt_cancel concurrently. */
        pnt_cancel(owner);
        close((int)job.data.fd);
        fprintf(stderr, "WORKER_ACQUIRE_FAILED result=%d\n", r);
        atomic_store(&worker_done, 1);
        return NULL;
    }
    fprintf(stderr, "CONSUMER_ACQUIRE_WAIT_COMPLETED\n");
    job.result = pixels(&job.format, &job.data, 0);
    /* On unknown completion retain the duplicate until process exit. */
    if (job.result != PIXEL_UNKNOWN && close((int)job.data.fd)) job.result = PIXEL_UNKNOWN;
    atomic_store(&worker_done, 1);
    return NULL;
}
#ifdef UU_TEST_PORTAL_BRIDGE
static int bridge_release(void *unused, struct spa_meta_sync_timeline *metadata) {
    (void)unused;
    uint64_t point = 0;
    stage("pnt release", "begin");
    if (pnt_release(owner, metadata)) return -1;
    stage("pnt release", "end");
    if (pnt_destroy(&owner) || drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) || point >= sentinel) return -1;
    return 0;
}
#endif
static int finish_worker(struct endpoint *e) {
    if (!worker_started || !atomic_load(&worker_done)) return 0;
    if (pthread_join(worker, NULL)) return -1;
    worker_started = 0;
    /* The loop owns SPA pointers. A removed lease must never be dereferenced. */
    if (atomic_load(&failed) || !held) return -1;
#ifndef UU_TEST_PORTAL_BRIDGE
    struct pw_buffer *b = held;
    struct spa_meta_header *header = spa_buffer_find_meta_data(b->buffer, SPA_META_Header, sizeof(*header));
    if (!header || header->seq != FRAME_GENERATION) return -1;
#endif
    enum pixel_result result = job.result;
    if (result == PIXEL_FAILED_FINISHED && force_mismatch) {
        atomic_store(&negative_finished, 1);
        atomic_store(&received, 1);
        return 0; /* No complete, release or consumer queue. */
    }
    if (result != PIXEL_OK || pnt_gpu_complete(owner)) return -1;
#ifdef UU_TEST_PORTAL_BRIDGE
    (void)e;
    if (uu_portal_async_finish(&bridge_lease, 1, bridge_release, NULL)) return -1;
    fprintf(stderr, "PORTAL_ENGINE_LEASE_FINISHED\n");
#else
    struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(b->buffer, SPA_META_SyncTimeline, sizeof(*m));
    uint64_t before = 0;
    stage("pnt release", "begin");
    if (pnt_release(owner, m)) return -1;
    stage("pnt release", "end");
    /* Release is already signaled: a later handle-destroy failure cannot
     * retract it, even though this test will not queue or ACK on failure. */
    if (pnt_destroy(&owner) ||
        drmSyncobjQuery(consumer_drm, &acquire_check, &before, 1) || before >= sentinel) return -1;
    header->seq = ACK_GENERATION;
    if (pw_stream_queue_buffer(e->stream, b) < 0) return -1;
#endif
    /* Producer separately dequeues and validates this generation. */
    held = NULL; atomic_store(&received, 1);
    return 0;
}
static void cross_remove(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
#ifdef UU_TEST_PORTAL_BRIDGE
    if (!e->producer && bridge) stream_remove_buffer(bridge, buffer);
#endif
    if (!e->producer && held == buffer) held = NULL;
    atomic_fetch_add(&removed_buffers, 1);
    atomic_store(&failed, 1);
    /* Removal invalidates the session, including while frame processing pauses. */
}
static void cross_process(void *data) {
    struct endpoint *e = data;
    if (atomic_load(&frame_paused) || atomic_load(&failed) || atomic_load(&producer_returned)) return;
    if (e->producer && atomic_load(&queued) && !atomic_load(&return_requested)) return;
    if (!e->producer && (held || atomic_load(&received))) return;
    struct pw_buffer *b;
#ifdef UU_TEST_PORTAL_BRIDGE
    if (!e->producer) {
        int result = uu_portal_async_acquire(bridge, &bridge_lease, &bridge_frame);
        if (result == -EAGAIN) return;
        if (result) goto bad;
        b = bridge_lease->buffer;
        fprintf(stderr, "PORTAL_ENGINE_LEASE_ACQUIRED\n");
    } else
#endif
    do {
        if (e->producer && reuse_submitted) {
            b = submitted; reuse_submitted = 0;
        } else b = pw_stream_dequeue_buffer(e->stream);
        if (!b) return;
        /* Retain unused pool buffers; only the submitted pointer proves return. */
    } while (e->producer && atomic_load(&queued) && b != submitted);
#ifndef UU_TEST_PORTAL_BRIDGE
    struct spa_meta_header *header = b->buffer ?
        spa_buffer_find_meta_data(b->buffer, SPA_META_Header, sizeof(*header)) : NULL;
    if (!header) goto bad;
#endif
    if (e->producer && atomic_load(&queued)) {
        struct pnspa_snapshot s;
        struct sync_allocation *x = b->user_data;
        struct stat st;
        uint64_t point = 0;
        if (!x || !pnspa_snapshot(b->buffer, &fixed_format, modifier, &s) ||
#ifndef UU_TEST_PORTAL_BRIDGE
            header->seq != ACK_GENERATION || header->flags ||
#endif
            !valid_data_flags(b->buffer) ||
            s.acquire_point != ACQUIRE_POINT || s.release_point != RELEASE_POINT || s.sync_flags ||
            s.mapoffset || s.offset || s.size != size || s.maxsize != size || s.stride != (int32_t)stride ||
            fstat(s.pixel_fd, &st) || st.st_dev != x->allocation->dev || st.st_ino != x->allocation->ino ||
            st.st_size != size || drmSyncobjQuery(drmfd, &x->handle, &point, 1) || point != RELEASE_POINT) goto bad;
        fprintf(stderr, "PRODUCER_RETURNED_ORIGINAL_BUFFER_GENERATION_RELEASE\n");
        atomic_store(&producer_returned, 1);
        return; /* The main-thread handshake gates the next generation. */
    }
    if (e->producer) {
#ifndef UU_TEST_PORTAL_BRIDGE
        *header = (struct spa_meta_header){ .seq = FRAME_GENERATION };
#endif
        submitted = b;
        struct sync_allocation *x = b->user_data;
        if (!x || atomic_load(&formats) != 1) goto bad;
        struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(b->buffer, SPA_META_SyncTimeline, sizeof(*m));
        if (!m) goto bad;
        *m = (struct spa_meta_sync_timeline){ .acquire_point = ACQUIRE_POINT,
            .release_point = RELEASE_POINT, .flags = TEST_RELEASE_FLAGS };
        struct allocation *a = x->allocation;
        struct record r = { getpid(), a->dev, a->ino, size, stride, modifier, SPA_VIDEO_FORMAT_BGRA, WIDTH, HEIGHT };
        producer_handle = x->handle;
        if (pixels(&fixed_format, &b->buffer->datas[0], 1) != PIXEL_OK ||
            send(control_fd, &r, sizeof(r), MSG_NOSIGNAL) != sizeof(r)) goto bad;
        atomic_store(&queued, 1);
        if (pw_stream_queue_buffer(e->stream, b) < 0) goto bad;
        atomic_store(&queue_succeeded, 1);
    } else {
        struct stat st; struct record r; struct pnspa_snapshot s;
        held = b; /* Do not queue on failure; process exit or revocation may still release it. */
        if (
#ifndef UU_TEST_PORTAL_BRIDGE
            header->seq != FRAME_GENERATION || header->flags ||
#endif
            atomic_load(&formats) != 2 || !pnspa_snapshot(b->buffer, &fixed_format, modifier, &s) ||
            s.format != SPA_VIDEO_FORMAT_BGRA || s.width != WIDTH || s.height != HEIGHT ||
            s.acquire_point != ACQUIRE_POINT || s.release_point != RELEASE_POINT || s.sync_flags != TEST_RELEASE_FLAGS ||
            !valid_data_flags(b->buffer) || !flags_negative_test(b->buffer) ||
            fstat(s.pixel_fd, &st) || s.mapoffset || s.offset || s.size != size || s.maxsize != size ||
            s.stride != (int32_t)stride || st.st_size != size || receive_record(&r) ||
            r.dev != (uint64_t)st.st_dev || r.ino != (uint64_t)st.st_ino || r.size != size || r.stride != stride ||
            r.modifier != modifier || r.format != s.format || r.width != s.width || r.height != s.height) goto bad;
#ifdef UU_TEST_PORTAL_BRIDGE
        /* Admit the public lease snapshot, not borrowed SPA synchronization fds. */
        if (bridge_frame.acquire_point != s.acquire_point || bridge_frame.release_point != s.release_point ||
            bridge_frame.pixels.width != s.width || bridge_frame.pixels.height != s.height ||
            bridge_frame.pixels.stride != (uint32_t)s.stride || bridge_frame.pixels.offset != s.offset ||
            bridge_frame.pixels.modifier != s.modifier) goto bad;
        s.acquire_fd = bridge_frame.acquire_fd; s.release_fd = bridge_frame.release_fd;
#endif
        if (drmSyncobjFDToHandle(consumer_drm, s.acquire_fd, &acquire_check) ||
            pnt_create_protocol(&owner, consumer_drm, s.acquire_fd, ACQUIRE_POINT, s.release_fd, RELEASE_POINT, NULL, NULL, TEST_PROTOCOL)) goto bad;
        if (worker_started) goto bad;
        job.format = fixed_format;
        job.data = b->buffer->datas[0];
        job.chunk = *job.data.chunk; job.data.chunk = &job.chunk;
        job.data.data = NULL;
#ifdef UU_TEST_PORTAL_BRIDGE
        job.data.fd = fcntl(bridge_frame.pixels.fd, F_DUPFD_CLOEXEC, 0);
#else
        job.data.fd = fcntl(s.pixel_fd, F_DUPFD_CLOEXEC, 0);
#endif
        if (job.data.fd < 0) goto bad;
        atomic_store(&worker_done, 0);
        if (pthread_create(&worker, NULL, gpu_worker, NULL)) {
            close((int)job.data.fd); goto bad;
        }
        worker_started = 1;
        return;

    }
    return;
bad:
    fprintf(stderr, "cross-process buffer/record validation failed\n");
    atomic_store(&failed, 1);
}
static int invoke_worker_progress(struct spa_loop *loop, bool async, uint32_t seq,
                                  const void *data, size_t size, void *user_data) {
    (void)loop; (void)async; (void)seq; (void)data; (void)size;
    if (atomic_exchange(&gate_requested, 0)) {
        fprintf(stderr, "LOOP_EVENT_DURING_GPU_WORKER\n");
        if (disconnect_worker) {
            struct endpoint *e = user_data;
            if (pw_stream_disconnect(e->stream) < 0) atomic_store(&failed, 1);
            /* Keep the worker at the checkpoint until removal was processed.
             * Cleanup joins it without touching the removed SPA storage. */
            return 0;
        }
        pthread_mutex_lock(&gate_lock);
        gate_open = 1; pthread_cond_signal(&gate_changed);
        pthread_mutex_unlock(&gate_lock);
    }
    if (finish_worker(user_data)) atomic_store(&failed, 1);
    return 0;
}
static int invoke_return(struct spa_loop *loop, bool async, uint32_t seq,
                         const void *data, size_t size, void *user_data) {
    (void)loop; (void)async; (void)seq; (void)data; (void)size;
    cross_process(user_data);
    return 0;
}
/* A blocking invocation waits for any preceding process callback to finish.
 * Only frame processing pauses; remove/format/state events keep running. */
static int invoke_pause(struct spa_loop *loop, bool async, uint32_t seq,
                        const void *data, size_t size, void *user_data) {
    (void)loop; (void)async; (void)data; (void)size; (void)user_data;
    atomic_store(&frame_paused, seq != 0);
    return 0;
}
static int invoke_disconnect(struct spa_loop *loop, bool async, uint32_t seq,
                             const void *data, size_t size, void *user_data) {
    (void)loop; (void)async; (void)seq; (void)data; (void)size;
    struct endpoint *e = user_data;
    int result = pw_stream_disconnect(e->stream);
    if (result < 0 || shutdown_idle_encoder()) return -1;
    return result;
}
static void cross_state(void *data, enum pw_stream_state old, enum pw_stream_state now, const char *error) {
    state(data, old, now, error);
#ifdef UU_TEST_PORTAL_BRIDGE
    struct endpoint *e = data;
    if (!e->producer && bridge) stream_state(bridge, old, now, error);
#endif
}
static const struct pw_stream_events cross_events = { PW_VERSION_STREAM_EVENTS,
    .state_changed = cross_state, .param_changed = sync_param, .add_buffer = sync_add,
    .remove_buffer = cross_remove, .process = cross_process };
static int cross_setup(struct endpoint *e, const char *name, int producer) {
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA, .flags = SPA_VIDEO_FLAG_MODIFIER,
        .modifier = modifier, .size = SPA_RECTANGLE(WIDTH, HEIGHT), .framerate = SPA_FRACTION(30, 1) };
    e->producer = producer; e->loop = pw_thread_loop_new(name, NULL);
    if (!e->loop) return -1;
    e->stream = pw_stream_new_simple(pw_thread_loop_get_loop(e->loop), name,
        pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, producer ? "Capture" : "Playback",
                          PW_KEY_MEDIA_ROLE, "Test", PW_KEY_NODE_NAME, name, PW_KEY_NODE_AUTOCONNECT, "false", NULL), &cross_events, e);
    if (!e->stream) return -1;
    const struct spa_pod *params[3]; unsigned param_count = 1;
#ifdef UU_TEST_PORTAL_BRIDGE
    if (!producer) {
        bridge = calloc(1, sizeof(*bridge));
        if (!bridge || pthread_mutex_init(&bridge->lock, NULL) || pthread_cond_init(&bridge->changed, NULL)) return -1;
        bridge->pw_loop = e->loop; bridge->stream = e->stream;
        /* Synthetic producer dimensions/device contract only. No Portal open. */
        bridge->width = WIDTH; bridge->height = HEIGHT;
        bridge->explicit_requested = bridge->dma_requested = 1;
        bridge->explicit_modifier = modifier; bridge->explicit_protocol = TEST_PROTOCOL;
        bridge->dma_requested_at = g_get_monotonic_time();
        param_count = capture_formats(bridge, &b, params, 1);
    } else
#endif
        params[0] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    if (pw_stream_connect(e->stream, producer ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT, PW_ID_ANY,
        producer ? PW_STREAM_FLAG_ALLOC_BUFFERS : PW_STREAM_FLAG_NONE, params, param_count) < 0) return -1;
    int rc = pw_thread_loop_start(e->loop);
    if (!rc) fprintf(stderr, "PIPEWIRE_LOOP_STARTED\n");
    return rc;
}
int main(int argc, char **argv) {
    setvbuf(stderr, NULL, _IONBF, 0);
    if (argc != 7 || (strcmp(argv[1], "producer") && strcmp(argv[1], "consumer"))) return 2;
    char *end; long fd = strtol(argv[2], &end, 10);
    if (*end || fd < 0 || fd > INT_MAX) return 2;
    control_fd = (int)fd;
    output_path = argv[6];
    int producer = !strcmp(argv[1], "producer"), started = 0, rc = 1, got_ack = 0;
    int negative = getenv("UU_PW_SYNC_CUDA_NEGATIVE") != NULL;
    force_mismatch = negative && !producer;
    const char *frames = getenv("UU_PW_NVENC_FRAMES");
    if (frames) {
        long count = strtol(frames, &end, 10);
        if (!*frames || *end || count < 1 || count > 32 || (negative && count != 1)) return 2;
        frame_count = (unsigned)count;
    }
    const char *disconnect = getenv("UU_PW_NVENC_DISCONNECT_AFTER");
    if (disconnect) {
        long count = strtol(disconnect, &end, 10);
        if (!*disconnect || *end || count < 1 || count >= frame_count || negative) return 2;
        disconnect_after = (unsigned)count;
    }
    disconnect_worker = getenv("UU_PW_NVENC_DISCONNECT_WORKER") != NULL;
    if (disconnect_worker && (frame_count != 1 || negative || disconnect_after)) return 2;
    long pid = strtol(argv[5], &end, 10);
    if (*end || pid < 0 || pid > INT_MAX || (!producer && pid == 0)) return 2;
    producer_pid = (pid_t)pid;
    if (!producer) {
        int enabled = 0; socklen_t len = sizeof(enabled);
        if (getsockopt(control_fd, SOL_SOCKET, SO_PASSCRED, &enabled, &len) || !enabled) return 2;
    }
    struct endpoint e = {0}; modifier = TARGET_MOD;
    if ((drmfd = open_render_node(argv[3])) < 0 || !(gbm = gbm_create_device(drmfd))) goto done;
    if (!producer) {
        struct stat a, b;
        consumer_drm = open_render_node(argv[3]);
        if (consumer_drm < 0 || fstat(drmfd, &a) || fstat(consumer_drm, &b) ||
            a.st_dev != b.st_dev || a.st_ino != b.st_ino || a.st_rdev != b.st_rdev) goto done;
    }
    struct allocation *probe = allocate(&stride, &size);
    if (!probe) goto done;
    close(probe->fd); gbm_bo_destroy(probe->bo); free(probe);
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
    destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
    bind_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    create_sync = (void *)eglGetProcAddress("eglCreateSyncKHR");
    destroy_sync = (void *)eglGetProcAddress("eglDestroySyncKHR");
    dup_fence = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    EGLint n = 0;
    const EGLint cfg[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    struct pnegl_device *consumer_device = NULL;
    if (!producer) {
        if (pnegl_device_open(&consumer_device, drmfd)) goto done;
        display = consumer_device->display; config = consumer_device->config;
        create_image = consumer_device->create_image; destroy_image = consumer_device->destroy_image;
        fprintf(stderr, "NATIVE_EGL_DEVICE_INITIALIZED\n");
    } else
    if (!platform || !create_image || !destroy_image || !bind_image ||
        (display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL)) == EGL_NO_DISPLAY ||
        !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API) ||
        !eglChooseConfig(display, cfg, &config, 1, &n) || n != 1) {
        fprintf(stderr, "EGL GBM initialization failed\n"); goto done;
    }
    const char *ext = eglQueryString(display, EGL_EXTENSIONS);
    if (!has_extension(ext, "EGL_ANDROID_native_fence_sync") ||
        !has_extension(ext, "EGL_KHR_fence_sync") || !create_sync || !destroy_sync || !dup_fence) {
        fprintf(stderr, "FAIL: EGL native fence sync unsupported\n"); goto done;
    }
    pw_init(NULL, NULL);
    if (cross_setup(&e, argv[4], producer) < 0) goto cleanup;
    started = 1;
    for (; frame_index < frame_count; ) {
        got_ack = 0; rc = 1;
        for (int i = 0; i < 1500 && !atomic_load(&failed); i++) {
            if (!producer && (atomic_load(&gate_requested) || atomic_load(&worker_done))) {
                if (pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_worker_progress,
                                   0, NULL, 0, true, &e) < 0) { atomic_store(&failed, 1); break; }
            }
            if (!producer && atomic_load(&received)) break;
            if (producer) {
                if (!got_ack) {
                    char ack[4]; ssize_t n = recv(control_fd, ack, sizeof(ack), MSG_DONTWAIT | MSG_TRUNC);
                    if (n >= 0) {
                        if (n != 3 || memcmp(ack, negative ? "NAK" : "ACK", 3) || !atomic_load(&queued)) {
                            fprintf(stderr, "unexpected ACK or peer closure\n"); break;
                        }
                        got_ack = 1;
                    } else if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) break;
                }
                if (got_ack && atomic_load(&queue_succeeded)) {
                    if (negative) break;
                    atomic_store(&return_requested, 1);
                    if (pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_return,
                                       0, NULL, 0, true, &e) < 0) {
                        atomic_store(&failed, 1); break;
                    }
                    if (atomic_load(&producer_returned)) break;
                }
            }
            usleep(10000);
        }
        if (pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_pause,
                           1, NULL, 0, true, NULL) < 0) { rc = 1; break; }
        /* Final teardown is not a mid-stream revocation: stop once before
         * the peer may exit and remove its stream after the final handshake. */
        if (negative || frame_index + 1 == frame_count) {
            pw_thread_loop_stop(e.loop); started = 0;
            fprintf(stderr, "PIPEWIRE_LOOP_STOPPED\n");
        }
        if (producer && negative && !atomic_load(&failed) && got_ack && atomic_load(&queue_succeeded) && atomic_load(&formats) == 1) {
            uint64_t point = 0;
            if (producer_handle && !drmSyncobjQuery(drmfd, &producer_handle, &point, 1) && point == 1 &&
                send(control_fd, "DONE", 4, MSG_NOSIGNAL) == 4) {
                fprintf(stderr, "NEGATIVE_PRODUCER_POINT_1_NO_SENTINEL\n"); rc = 0;
            }
        } else if (!producer && negative && !atomic_load(&failed) && atomic_load(&negative_finished) && held && atomic_load(&formats) == 2) {
            uint64_t point = 0;
            if (!drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) && point == 1 &&
                send(control_fd, "NAK", 3, MSG_NOSIGNAL) == 3 && wait_message("DONE") == 0 &&
                !drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) && point == 1) {
                fprintf(stderr, "NEGATIVE_CONSUMER_NO_COMPLETE_RELEASE_QUEUE_ACK\n"); rc = 0;
            }
        } else if (!negative && producer && !atomic_load(&failed) && got_ack && atomic_load(&queue_succeeded) && atomic_load(&formats) == 1) {
            uint64_t point = 0;
            if (atomic_load(&producer_returned) && producer_handle &&
                !drmSyncobjQuery(drmfd, &producer_handle, &point, 1) && point == RELEASE_POINT &&
                (frame_index + 1 < frame_count || !drmSyncobjTimelineSignal(drmfd, &producer_handle, &sentinel, 1)) &&
                send(control_fd, "DONE", 4, MSG_NOSIGNAL) == 4) rc = 0;
        } else if (!negative && !producer && !atomic_load(&failed) && atomic_load(&received) && atomic_load(&formats) == 2) {
            struct timespec now; uint64_t point = 0;
            uint64_t target = frame_index + 1 < frame_count ? RELEASE_POINT : sentinel;
            if (send(control_fd, "ACK", 3, MSG_NOSIGNAL) == 3 && wait_message("DONE") == 0 &&
                !clock_gettime(CLOCK_MONOTONIC, &now) &&
                !drmSyncobjTimelineWait(consumer_drm, &acquire_check, &target, 1,
                    (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000,
                    DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, NULL) &&
                !drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) && point == target) rc = 0;
        }
        if (atomic_load(&failed)) rc = 1;
        /* ACK/NAK is sent only after the consumer waited and finished its GPU work.
         * Cleanup is deliberately after that handshake, never a prerequisite to queue. */
        if (producer && rc == 0) {
            int unknown = 0;
            if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, producer_ctx)) rc = 1;
            else {
                glDeleteFramebuffers(1, &producer_fbo); glDeleteTextures(1, &producer_tex);
                if (gl_errors(&unknown) || unknown || !destroy_sync(display, producer_sync) ||
                    !destroy_image(display, producer_image) ||
                    !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT) ||
                    !eglDestroyContext(display, producer_ctx)) rc = 1;
            }
        }
        if (rc || negative) break;
        if (!producer && acquire_check) {
            if (drmSyncobjDestroy(consumer_drm, acquire_check)) { rc = 1; break; }
            acquire_check = 0;
        }
        if (!producer && disconnect_after == frame_index + 1) {
            int result = pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_disconnect,
                                       0, NULL, 0, true, &e);
            for (int i = 0; i < 200 && !atomic_load(&removed_buffers); i++) usleep(10000);
            if (!result && atomic_load(&removed_buffers) && atomic_load(&failed))
                fprintf(stderr, "CONSUMER_DISCONNECT_REMOVAL_OBSERVED\n");
            rc = 1;
            break; /* No NEXT handshake, so the producer cannot reuse the buffer. */
        }
        if (frame_index + 1 == frame_count) break;
        /* The completed-frame handshake establishes producer ownership and
         * completed source accesses. The consumer-ready handshake permits reuse. */
        if (producer && wait_message("NEXT")) { rc = 1; break; }
        frame_index++;
        atomic_store(&queued, 0); atomic_store(&queue_succeeded, 0);
        atomic_store(&return_requested, 0); atomic_store(&producer_returned, 0);
        atomic_store(&received, 0);
        if (producer) reuse_submitted = 1;
        if (pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_pause,
                           0, NULL, 0, true, NULL) < 0) { rc = 1; break; }
        if (!producer) {
            if (send(control_fd, "NEXT", 4, MSG_NOSIGNAL) != 4) { rc = 1; break; }
        } else if (pw_loop_invoke(pw_thread_loop_get_loop(e.loop), invoke_return,
                                  0, NULL, 0, true, &e) < 0) { rc = 1; break; }
    }
cleanup:
    if (started) {
        pw_thread_loop_stop(e.loop);
        fprintf(stderr, "PIPEWIRE_LOOP_STOPPED\n");
    }
    if (worker_started) {
        /* Never destroy the loop or copied job storage while a worker uses it.
         * This may still block on a driver; timeout is not GPU isolation. */
        pthread_mutex_lock(&gate_lock);
        gate_open = 1; pthread_cond_signal(&gate_changed);
        pthread_mutex_unlock(&gate_lock);
        if (pthread_join(worker, NULL)) rc = 1;
        worker_started = 0;
        if (disconnect_worker && atomic_load(&removed_buffers) && job.result == PIXEL_OK) {
            uint64_t point = 0;
            if (owner && pnt_state(owner) == PNT_GPU &&
                !drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) && point == ACQUIRE_POINT)
                fprintf(stderr, "REMOVED_WORKER_DRAINED_NO_RELEASE_QUEUE_ACK\n");
        }
    }
#ifdef UU_TEST_PORTAL_BRIDGE
    if (bridge_lease && atomic_load(&worker_done) && job.result != PIXEL_UNKNOWN) {
        uu_portal_end(bridge);
        if (uu_portal_async_abandon(&bridge_lease, 1)) rc = 1;
        else fprintf(stderr, "PORTAL_ENGINE_LEASE_ABANDONED\n");
    }
    /* Keep bridge/listener storage alive through process exit, as for the base
     * harness. This is not a complete host shutdown or revocation proof. */
#endif
    if (atomic_load(&failed)) rc = 1;
    /* Process exit can release BOs even on failure or timeout; no isolation is claimed. */
done:
    if (rc) fprintf(stderr, "%s FAIL\n", argv[1]);
    close(control_fd);
    return rc;
}
