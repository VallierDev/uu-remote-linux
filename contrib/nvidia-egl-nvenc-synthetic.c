/* Synthetic-only GBM -> EGL -> CUDA -> NVENC -> software FFmpeg test; no capture.
 * Build from repository root:
 * mkdir -p -m 700 "$HOME/.cache/uu-synthetic-test"
 * cc -std=c11 -Wall -Wextra -Werror -O2 -Ivendor/ffnvcodec \
 *   contrib/nvidia-egl-nvenc-synthetic.c $(pkg-config --cflags --libs gbm libdrm egl) \
 *   -ldl -o "$HOME/.cache/uu-synthetic-test/probe"
 * Run: timeout --kill-after=5s 60s "$HOME/.cache/uu-synthetic-test/probe" /dev/dri/renderD128
 * Build and run nvidia-egl-nvenc-synthetic-fake-test.c with the same flags
 * to exercise error-path suppression of the real NVENC unmap.
 * Requires /usr/bin/timeout, /usr/bin/ffmpeg and /usr/bin/ffprobe. Child
 * processes time out after 20s. Outer timeout also bounds stuck driver calls;
 * forced termination may leave the private temporary directory for removal.
 * Exit 0 pass, 1 failure, 2 unavailable (timeout uses 124/137).
 * This includes production implementation solely for private test access; it is
 * not a production API and does not demonstrate Portal import or zero-copy.
 */
#define _POSIX_C_SOURCE 200809L
#include "../c_engine/nvenc_engine.c"
#include <errno.h>
#include <strings.h>
#include <fcntl.h>
#include <sys/wait.h>
#include <unistd.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>

/* NVIDIA cudaEGL.h CU_EGL_COLOR_FORMAT_ARGB (BGRA byte order). */
#define CU_EGL_COLOR_FORMAT_ARGB 0x06
#define W 256
#define H 256
#define BYTES (W * H * 4)

/* Observe cleanup calls whose return values nvenc_encode_frame ignores. */
static PNVENCENCODEPICTURE real_encode;
static PNVENCLOCKBITSTREAM real_lock;
static PNVENCUNLOCKBITSTREAM real_unlock;
static PNVENCUNMAPINPUTRESOURCE real_unmap;
static tcuCtxPopCurrent_v2 *real_pop;
static CUcontext expected_ctx;
static int encode_cleanup_failed, encode_uncertain;
static NVENCSTATUS NVENCAPI checked_encode(void *encoder, NV_ENC_PIC_PARAMS *params) {
    NVENCSTATUS r = real_encode(encoder, params);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode submission failed: %d\n", r); encode_uncertain = 1; }
    return r;
}
static NVENCSTATUS NVENCAPI checked_lock(void *encoder, NV_ENC_LOCK_BITSTREAM *params) {
    NVENCSTATUS r = real_lock(encoder, params);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode lock failed: %d\n", r); encode_uncertain = 1; }
    return r;
}
static NVENCSTATUS NVENCAPI checked_unlock(void *encoder, NV_ENC_OUTPUT_PTR buffer) {
    NVENCSTATUS r = real_unlock(encoder, buffer);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode unlock failed: %d\n", r); encode_cleanup_failed = 1; encode_uncertain = 1; }
    return r;
}
static NVENCSTATUS NVENCAPI checked_unmap(void *encoder, NV_ENC_INPUT_PTR buffer) {
    if (encode_uncertain) { fprintf(stderr, "unmap suppressed: encode completion uncertain\n"); return NV_ENC_ERR_GENERIC; }
    NVENCSTATUS r = real_unmap(encoder, buffer);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode unmap failed: %d\n", r); encode_cleanup_failed = 1; encode_uncertain = 1; }
    return r;
}
static CUresult CUDAAPI checked_pop(CUcontext *ctx) {
    CUresult r = real_pop(ctx);
    if (r != CUDA_SUCCESS || *ctx != expected_ctx) {
        fprintf(stderr, "encode context pop failed or mismatched: %d\n", r); encode_cleanup_failed = 1;
    }
    return r;
}

/* Shared by the probe and fake test: failures must be observed before the
 * engine attempts its internal unmap, and wrappers must always be restored. */
static int guarded_encode(NvencEngine *e, const uint8_t **data, uint32_t *len) {
    encode_uncertain = 0; encode_cleanup_failed = 0;
    *data = NULL; *len = 0;
    real_encode = e->fn.nvEncEncodePicture; real_lock = e->fn.nvEncLockBitstream;
    real_unlock = e->fn.nvEncUnlockBitstream; real_unmap = e->fn.nvEncUnmapInputResource;
    real_pop = e->cuCtxPopCurrent; expected_ctx = e->cu_ctx;
    int old_zero_copy = e->is_zero_copy;
    e->fn.nvEncEncodePicture = checked_encode; e->fn.nvEncLockBitstream = checked_lock;
    e->fn.nvEncUnlockBitstream = checked_unlock; e->fn.nvEncUnmapInputResource = checked_unmap;
    e->cuCtxPopCurrent = checked_pop; e->is_zero_copy = 1;
    int rc = nvenc_encode_frame(e, NULL, W * 4, 1, data, len);
    e->is_zero_copy = old_zero_copy;
    e->fn.nvEncEncodePicture = real_encode; e->fn.nvEncLockBitstream = real_lock;
    e->fn.nvEncUnlockBitstream = real_unlock; e->fn.nvEncUnmapInputResource = real_unmap;
    e->cuCtxPopCurrent = real_pop;
    if (rc || encode_cleanup_failed || !*data || !*len) encode_uncertain = 1;
    return rc;
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
static int node = -1, dma = -1, egl_ready;
static struct gbm_device *gbm;
static struct gbm_bo *bo;
static EGLDisplay display = EGL_NO_DISPLAY;
static EGLImageKHR image = EGL_NO_IMAGE_KHR;
static PFNEGLDESTROYIMAGEKHRPROC destroy_image;
static CUgraphicsResource resource;
static CUresult (CUDAAPI *unregister_image)(CUgraphicsResource);
/* Prove that the opened DRM node and the engine's CUDA device identify the
 * same PCI function. Non-PCI devices and missing identity are inconclusive. */
static int same_device(NvencEngine *e) {
    drmDevicePtr d = NULL;
    CUresult (CUDAAPI *bus_id)(char *, int, CUdevice) = NULL;
    char cuda[32] = {0}, drm[32];
    *(void **)(&bus_id) = dlsym(e->cuda_lib, "cuDeviceGetPCIBusId");
    if (!bus_id || drmGetNodeTypeFromFd(node) != DRM_NODE_RENDER || drmGetDevice2(node, 0, &d) || !d) {
        fprintf(stderr, "cannot establish DRM/CUDA PCI identity\n"); return 1;
    }
    int result = 1;
    if (d->bustype == DRM_BUS_PCI && d->businfo.pci) {
        CUresult rc = bus_id(cuda, sizeof(cuda), e->dev);
        if (rc != CUDA_SUCCESS) fprintf(stderr, "CUDA PCI identity query failed: %d\n", rc);
        else {
            snprintf(drm, sizeof(drm), "%04x:%02x:%02x.%x", d->businfo.pci->domain,
                     d->businfo.pci->bus, d->businfo.pci->dev, d->businfo.pci->func);
            result = strcasecmp(drm, cuda) ? 2 : 0;
            printf("DRM PCI %s CUDA PCI %s: %s\n", drm, cuda, result ? "mismatch" : "match");
        }
    } else fprintf(stderr, "DRM PCI identity unavailable\n");
    drmFreeDevice(&d);
    return result;
}
static int setup_image(const char *path) {
    node = open(path, O_RDWR | O_CLOEXEC);
    if (node < 0) { perror("open render node"); return 1; }
    gbm = gbm_create_device(node);
    if (!gbm) { fprintf(stderr, "GBM device creation failed\n"); return 1; }
    const uint64_t linear = DRM_FORMAT_MOD_LINEAR;
    bo = gbm_bo_create(gbm, W, H, GBM_FORMAT_ARGB8888, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    if (!bo) bo = gbm_bo_create_with_modifiers(gbm, W, H, GBM_FORMAT_ARGB8888, &linear, 1);
    if (!bo || gbm_bo_get_plane_count(bo) != 1 || gbm_bo_get_modifier(bo) != linear) {
        fprintf(stderr, "linear single-plane GBM creation or layout failed\n"); return 1;
    }
    uint32_t stride; void *map_data;
    unsigned char *pixels = gbm_bo_map(bo, 0, 0, W, H, GBM_BO_TRANSFER_WRITE, &stride, &map_data);
    if (!pixels) { fprintf(stderr, "GBM CPU map failed\n"); return 1; }
    if (stride < W * 4) { gbm_bo_unmap(bo, map_data); return 1; }
    const uint32_t colors[] = {0xffff0000U, 0xff00ff00U, 0xff0000ffU, 0xffffffffU};
    for (unsigned y = 0; y < H; y++) for (unsigned x = 0; x < W; x++) {
        uint32_t c = colors[(y >= H/2)*2 + (x >= W/2)];
        memcpy(pixels + y*stride + x*4, &c, 4);
    }
    gbm_bo_unmap(bo, map_data);
    dma = gbm_bo_get_fd(bo);
    if (dma < 0) { perror("gbm_bo_get_fd"); return 1; }
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    PFNEGLCREATEIMAGEKHRPROC create = (void *)eglGetProcAddress("eglCreateImageKHR");
    destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
    if (!platform || !create || !destroy_image) { fprintf(stderr, "EGL image entry points unavailable\n"); return 2; }
    display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, NULL, NULL)) {
        fprintf(stderr, "EGL GBM initialize failed: 0x%x\n", eglGetError()); return 1;
    }
    egl_ready = 1;
    const EGLint attrs[] = {EGL_WIDTH,W,EGL_HEIGHT,H,EGL_LINUX_DRM_FOURCC_EXT,DRM_FORMAT_ARGB8888,
        EGL_DMA_BUF_PLANE0_FD_EXT,dma,EGL_DMA_BUF_PLANE0_OFFSET_EXT,(EGLint)gbm_bo_get_offset(bo,0),
        EGL_DMA_BUF_PLANE0_PITCH_EXT,(EGLint)gbm_bo_get_stride(bo),
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,0,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,0,EGL_NONE};
    image = create(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "EGL DMA-BUF import failed: 0x%x\n", eglGetError()); return 1; }
    return 0;
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
    if (result) { if (cleanup_image()) result = 1; return result; }
    result = 1;
    e = nvenc_create(&config);
    if (!e) { fprintf(stderr, "NVENC initialization failed; cleanup uncertain, exiting isolated process\n"); return 1; }
    result = same_device(e);
    if (result) goto cleanup;
    result = 1;
    *(void **)(&reg) = dlsym(e->cuda_lib, "cuGraphicsEGLRegisterImage");
    *(void **)(&mapped) = dlsym(e->cuda_lib, "cuGraphicsResourceGetMappedEglFrame");
    *(void **)(&unregister_image) = dlsym(e->cuda_lib, "cuGraphicsUnregisterResource");
    *(void **)(&copy2d) = dlsym(e->cuda_lib, "cuMemcpy2D_v2");
    *(void **)(&sync) = dlsym(e->cuda_lib, "cuCtxSynchronize");
    if (!reg || !mapped || !unregister_image || !copy2d || !sync) { fprintf(stderr, "CUDA EGL entry points unavailable\n"); result = 2; goto cleanup; }
    if (e->cuCtxPushCurrent(e->cu_ctx) != CUDA_SUCCESS) { fprintf(stderr, "context push failed\n"); uncertain = 1; goto cleanup; }
    pushed = 1;
    CUresult cr = reg(&resource, image, 0);
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
    cp.dstMemoryType = CU_MEMORYTYPE_DEVICE; cp.dstDevice = e->cu_dptr; cp.dstPitch = W*4;
    cp.WidthInBytes = W*4; cp.Height = H;
    cr = copy2d(&cp);
    if (cr) { fprintf(stderr, "cuMemcpy2D failed: %d; completion uncertain\n", cr); uncertain = 1; goto cleanup; }
    cr = sync();
    if (cr) { fprintf(stderr, "CUDA synchronize failed: %d; completion uncertain\n", cr); uncertain = 1; goto cleanup; }
    printf("EGL %s -> NVENC CUDA buffer GPU copy synchronized\n", frame.frameType == CU_EGL_FRAME_TYPE_ARRAY ? "array" : "pitch");
    /* Private test hack: skip the engine's HtoD, not a zero-copy API. Restore
     * immediately even on encode failure, before any normal teardown. */
    int enc = guarded_encode(e, &data, &len);
    if (enc || encode_cleanup_failed || !data || !len) {
        fprintf(stderr, "encode failed or empty: rc=%d bytes=%u; submission completion uncertain\n", enc, len);
        encode_uncertain = 1;
        goto cleanup;
    }
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
    if (encode_uncertain || uncertain) { fprintf(stderr, "uncertain NVENC completion; leaving GPU resources to isolated process exit\n"); result = 1; goto abandon; }
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
    printf("%s: synthetic only; decoded pixels sampled, no Portal or zero-copy claim\n", result == 0 ? "PASS" : result == 2 ? "UNSUPPORTED" : "FAIL");
    return result;
}
