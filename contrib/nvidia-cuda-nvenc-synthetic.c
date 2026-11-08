/* Synthetic-only CUDA -> CUDA -> NVENC -> software FFmpeg test; no capture.
 * Build from repository root:
 * mkdir -p -m 700 "$HOME/.cache/uu-synthetic-test"
 * cc -std=c11 -Wall -Wextra -Werror -O2 -Ivendor/ffnvcodec \
 *   contrib/nvidia-cuda-nvenc-synthetic.c -ldl -o "$HOME/.cache/uu-synthetic-test/probe"
 * Run fake test: "$HOME/.cache/uu-synthetic-test/probe" --selftest
 * Run: timeout --kill-after=5s 60s "$HOME/.cache/uu-synthetic-test/probe"
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
#include <fcntl.h>
#include <sys/wait.h>
#include <unistd.h>

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
static int encode_cleanup_failed, completion_uncertain;
static NVENCSTATUS NVENCAPI checked_encode(void *encoder, NV_ENC_PIC_PARAMS *pic) {
    NVENCSTATUS r = real_encode(encoder, pic);
    if (r != NV_ENC_SUCCESS) completion_uncertain = 1;
    return r;
}
static NVENCSTATUS NVENCAPI checked_lock(void *encoder, NV_ENC_LOCK_BITSTREAM *lock) {
    NVENCSTATUS r = real_lock(encoder, lock);
    if (r != NV_ENC_SUCCESS) completion_uncertain = 1;
    return r;
}
static NVENCSTATUS NVENCAPI checked_unlock(void *encoder, NV_ENC_OUTPUT_PTR buffer) {
    NVENCSTATUS r = real_unlock(encoder, buffer);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode unlock failed: %d\n", r); encode_cleanup_failed = 1; completion_uncertain = 1; }
    return r;
}
static NVENCSTATUS NVENCAPI checked_unmap(void *encoder, NV_ENC_INPUT_PTR buffer) {
    if (completion_uncertain) {
        fprintf(stderr, "skipping unmap: NVENC completion uncertain\n");
        encode_cleanup_failed = 1;
        return NV_ENC_ERR_GENERIC;
    }
    NVENCSTATUS r = real_unmap(encoder, buffer);
    if (r != NV_ENC_SUCCESS) { fprintf(stderr, "encode unmap failed: %d\n", r); encode_cleanup_failed = 1; completion_uncertain = 1; }
    return r;
}
static CUresult CUDAAPI checked_pop(CUcontext *ctx) {
    CUresult r = real_pop(ctx);
    if (r != CUDA_SUCCESS || *ctx != expected_ctx) {
        fprintf(stderr, "encode context pop failed or mismatched: %d\n", r); encode_cleanup_failed = 1; completion_uncertain = 1;
    }
    return r;
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

static int must_abandon_encode(int rc, const uint8_t *data, uint32_t len) {
    return rc || encode_cleanup_failed || completion_uncertain || !data || !len;
}

static int fake_failure;
static char fake_events[16];
static size_t fake_event_count;
static void fake_event(char event) {
    if (fake_event_count < sizeof(fake_events) - 1) fake_events[fake_event_count++] = event;
    else fake_event_count = sizeof(fake_events); /* Make an overflow fail the assertion. */
}
static CUresult CUDAAPI fake_push(CUcontext ctx) { (void)ctx; return CUDA_SUCCESS; }
static CUresult CUDAAPI fake_pop(CUcontext *ctx) {
    fake_event('P'); *ctx = fake_failure == 6 ? NULL : expected_ctx;
    return fake_failure == 5 ? (CUresult)1 : CUDA_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_map(void *encoder, NV_ENC_MAP_INPUT_RESOURCE *map) {
    (void)encoder; map->mappedResource = (NV_ENC_INPUT_PTR)(uintptr_t)1; return NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_encode(void *encoder, NV_ENC_PIC_PARAMS *pic) {
    (void)encoder; (void)pic; fake_event('E'); return fake_failure == 1 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_lock(void *encoder, NV_ENC_LOCK_BITSTREAM *lock) {
    static const unsigned char byte = 1;
    (void)encoder; fake_event('L');
    lock->bitstreamBufferPtr = (void *)&byte; lock->bitstreamSizeInBytes = 1;
    return fake_failure == 2 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_unlock(void *encoder, NV_ENC_OUTPUT_PTR buffer) {
    (void)encoder; (void)buffer; fake_event('U'); return fake_failure == 3 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_unmap(void *encoder, NV_ENC_INPUT_PTR buffer) {
    (void)encoder; (void)buffer; fake_event('M'); return fake_failure == 4 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static int selftest(void) {
    static const char *const events[] = { "ELUMP", "EP", "ELP", "ELUP", "ELUMP", "ELUMP", "ELUMP" };
    for (fake_failure = 0; fake_failure <= 6; fake_failure++) {
        NvencEngine e = {0}; const uint8_t *data; uint32_t len;
        memset(fake_events, 0, sizeof(fake_events)); fake_event_count = 0;
        encode_cleanup_failed = completion_uncertain = 0;
        e.cu_ctx = (CUcontext)(uintptr_t)1; expected_ctx = e.cu_ctx;
        e.cuCtxPushCurrent = fake_push; real_pop = fake_pop; e.cuCtxPopCurrent = checked_pop;
        e.reg_res.registeredResource = (NV_ENC_REGISTERED_PTR)(uintptr_t)1;
        e.is_zero_copy = 1; e.fn.nvEncMapInputResource = fake_map;
        real_encode = fake_encode; real_lock = fake_lock; real_unlock = fake_unlock; real_unmap = fake_unmap;
        e.fn.nvEncEncodePicture = checked_encode; e.fn.nvEncLockBitstream = checked_lock;
        e.fn.nvEncUnlockBitstream = checked_unlock; e.fn.nvEncUnmapInputResource = checked_unmap;
        int rc = nvenc_encode_frame(&e, NULL, W * 4, 1, &data, &len);
        if (fake_event_count >= sizeof(fake_events) || strcmp(fake_events, events[fake_failure]) ||
            completion_uncertain != (fake_failure != 0) || encode_cleanup_failed != (fake_failure != 0) ||
            must_abandon_encode(rc, data, len) != (fake_failure != 0) ||
            (fake_failure == 1 ? rc != -4 : fake_failure == 2 ? rc != -5 : rc != 0)) {
            fprintf(stderr, "selftest failed: case %d\n", fake_failure); free(e.output_staging); return 1;
        }
        free(e.output_staging);
    }
    puts("PASS: fake event order and encode/lock/unlock/unmap/pop failures"); return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--selftest")) return selftest();
    if (argc != 1) { fprintf(stderr, "usage: %s [--selftest]\n", argv[0]); return 1; }
    NvencConfig config = { .width = W, .height = H, .fps = 30, .gop = 30,
                           .cq = 18, .target_bps = 4000000, .max_bps = 6000000 };
    NvencEngine *e = NULL;
    CUdeviceptr src = 0;
    CUcontext popped = NULL;
    int pushed = 0, result = 1, fd = -1, rawfd = -1, encode_uncertain = 0;
    char dir[] = "/tmp/uu-synthetic-XXXXXX";
    char bitpath[128], rawpath[128];
    int dir_created = 0, bit_created = 0, raw_created = 0;
    unsigned char rgb[W * H * 3];
    const uint8_t *data = NULL; uint32_t len = 0;
    CUresult (*fill)(CUdeviceptr, unsigned int, size_t) = NULL;
    CUresult (*copy)(CUdeviceptr, CUdeviceptr, size_t) = NULL;
    CUresult (*sync)(void) = NULL;
    /* BGRA little-endian: red, green, blue, white, one quadrant per fill. */
    const unsigned colors[] = { 0xffff0000U, 0xff00ff00U, 0xff0000ffU, 0xffffffffU };
    const unsigned char expected[4][3] = {{255,0,0},{0,255,0},{0,0,255},{255,255,255}};

    e = nvenc_create(&config);
    if (!e) { fprintf(stderr, "FAIL: CUDA/NVENC initialization failed; engine initialization cleanup is unchecked, process exiting\n"); return 1; }
    *(void **)(&fill) = dlsym(e->cuda_lib, "cuMemsetD32_v2");
    *(void **)(&copy) = dlsym(e->cuda_lib, "cuMemcpyDtoD_v2");
    *(void **)(&sync) = dlsym(e->cuda_lib, "cuCtxSynchronize");
    if (!fill || !copy || !sync) { fprintf(stderr, "UNSUPPORTED: missing CUDA symbols\n"); result = 2; goto cleanup; }
    if (e->cuCtxPushCurrent(e->cu_ctx) != CUDA_SUCCESS) { fprintf(stderr, "context push failed\n"); goto cleanup; }
    pushed = 1;
    if (e->cuMemAlloc(&src, BYTES) != CUDA_SUCCESS || !src) { fprintf(stderr, "source allocation failed\n"); goto cleanup; }
    for (unsigned y = 0; y < H; y++) for (unsigned x = 0; x < 2; x++) {
        unsigned quadrant = (y >= H / 2) * 2 + x;
        if (fill(src + ((size_t)y * W + x * W / 2) * 4, colors[quadrant], W / 2) != CUDA_SUCCESS) {
            fprintf(stderr, "CUDA synthetic fill failed\n"); goto cleanup;
        }
    }
    if (copy(e->cu_dptr, src, BYTES) != CUDA_SUCCESS || sync() != CUDA_SUCCESS) {
        fprintf(stderr, "CUDA GPU copy or synchronization failed\n"); goto cleanup;
    }
    puts("CUDA synthetic GPU-to-GPU copy synchronized");
    /* Private test hack: skip the engine's HtoD, not a zero-copy API. Restore
     * immediately even on encode failure, before any normal teardown. */
    real_encode = e->fn.nvEncEncodePicture; real_lock = e->fn.nvEncLockBitstream;
    real_unlock = e->fn.nvEncUnlockBitstream; real_unmap = e->fn.nvEncUnmapInputResource;
    e->fn.nvEncEncodePicture = checked_encode; e->fn.nvEncLockBitstream = checked_lock;
    real_pop = e->cuCtxPopCurrent; expected_ctx = e->cu_ctx;
    e->fn.nvEncUnlockBitstream = checked_unlock; e->fn.nvEncUnmapInputResource = checked_unmap;
    e->cuCtxPopCurrent = checked_pop;
    e->is_zero_copy = 1;
    int enc = nvenc_encode_frame(e, NULL, W * 4, 1, &data, &len);
    e->is_zero_copy = 0;
    e->fn.nvEncEncodePicture = real_encode; e->fn.nvEncLockBitstream = real_lock;
    e->fn.nvEncUnlockBitstream = real_unlock; e->fn.nvEncUnmapInputResource = real_unmap;
    e->cuCtxPopCurrent = real_pop;
    if (must_abandon_encode(enc, data, len)) {
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
    if (encode_uncertain) { fprintf(stderr, "uncertain NVENC completion; leaving GPU resources to isolated process exit\n"); result = 1; goto abandon; }
    if (!pushed && e->cuCtxPushCurrent(e->cu_ctx) == CUDA_SUCCESS) pushed = 1;
    if (!pushed) { fprintf(stderr, "cleanup context push failed; resources leaked until process exit\n"); result = 1; }
    else {
#define CLEAN(call) do { int r_ = (int)(call); if (r_) { fprintf(stderr, "cleanup %s failed: %d; leaving remaining resources to process exit\n", #call, r_); result = 1; goto abandon; } } while (0)
        if (sync) CLEAN(sync());
        if (src) CLEAN(e->cuMemFree(src));
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
abandon:
    free(e->output_staging); free(e);
    printf("%s: synthetic only; no Portal or zero-copy claim\n", result == 0 ? "PASS" : result == 2 ? "UNSUPPORTED" : "FAIL");
    return result;
}
