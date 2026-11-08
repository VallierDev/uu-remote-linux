/* Offline fake objects only; not driver validation.
 * cc -std=c11 -Wall -Wextra -Werror -fsanitize=address,undefined -g -Ivendor/ffnvcodec \
 *   contrib/nvidia-modifier-nvenc-failure-fake-test.c contrib/portal-nvenc-experimental.c -o /tmp/nvenc-failure-test
 */
#include "portal-nvenc-experimental.h"
#include <stdio.h>
#include <string.h>

static PortalNvencExperimental *current;
static char trace[32];
static size_t trace_len;
static char fail_at, inject_at;
static int recursive, bad_map, bad_lock, nested_ok, invalid;
static int copy(void *, CUdeviceptr, uint32_t);

static int event(char c) {
    if (trace_len + 1 >= sizeof(trace)) return 1;
    trace[trace_len++] = c; trace[trace_len] = 0;
    if (c == inject_at) {
        if (recursive) {
            const uint8_t *data = (void *)1; uint32_t len = 9;
            int rc = portal_nvenc_experimental_encode(current, copy, NULL, &data, &len);
            nested_ok = rc != 0 && !data && !len && current->quarantined;
        } else portal_nvenc_experimental_quarantine(current);
    }
    return c == fail_at;
}
static int copy(void *opaque, CUdeviceptr dst, uint32_t pitch) {
    (void)opaque; (void)dst; (void)pitch; return event('C');
}
static CUresult CUDAAPI sync_ctx(void) { return event('S') ? 1 : CUDA_SUCCESS; }
static NVENCSTATUS NVENCAPI map(void *e, NV_ENC_MAP_INPUT_RESOURCE *p) {
    (void)e; p->mappedResource = bad_map == 1 ? NULL : (void *)1;
    p->mappedBufferFmt = bad_map == 2 ? NV_ENC_BUFFER_FORMAT_NV12 : NV_ENC_BUFFER_FORMAT_ARGB;
    return event('M') ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI encode(void *e, NV_ENC_PIC_PARAMS *p) {
    (void)e; (void)p; return event('E') ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI lock(void *e, NV_ENC_LOCK_BITSTREAM *p) {
    static const char byte = 42; (void)e;
    p->bitstreamBufferPtr = bad_lock == 1 ? NULL : (void *)&byte;
    p->bitstreamSizeInBytes = bad_lock == 2 ? 5 : bad_lock == 3 ? 0 : 1;
    return event('L') ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI unlock(void *e, NV_ENC_OUTPUT_PTR p) {
    (void)e; (void)p; return event('U') ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI unmap(void *e, NV_ENC_INPUT_PTR p) {
    (void)e; (void)p; return event('N') ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}

static int run(const char *expected, int success, int complete) {
    uint8_t output[4] = {0}; const uint8_t *data = (void *)1; uint32_t len = 9;
    NV_ENCODE_API_FUNCTION_LIST fn = {0};
    fn.nvEncMapInputResource = map; fn.nvEncEncodePicture = encode;
    fn.nvEncLockBitstream = lock; fn.nvEncUnlockBitstream = unlock; fn.nvEncUnmapInputResource = unmap;
    PortalNvencExperimental p = { .fn=&fn, .encoder=(void *)1, .surface=(void *)1,
        .bitstream=(void *)1, .dst=1, .width=1, .height=1, .sync=sync_ctx,
        .output=output, .capacity=sizeof(output), .source_complete=1 };
    switch (invalid) {
    case 1: p.width = UINT32_MAX / 4 + 1; break;
    case 2: fn.nvEncMapInputResource = NULL; break;
    case 3: fn.nvEncEncodePicture = NULL; break;
    case 4: fn.nvEncLockBitstream = NULL; break;
    case 5: fn.nvEncUnlockBitstream = NULL; break;
    case 6: fn.nvEncUnmapInputResource = NULL; break;
    }
    current = &p; trace_len = 0; trace[0] = 0; nested_ok = 0;
    int rc = portal_nvenc_experimental_encode(&p, copy, NULL, &data, &len);
    if (strcmp(trace, expected) || p.busy || p.source_complete != complete ||
        (success ? (rc || p.quarantined || data != output || len != 1 || output[0] != 42) :
                   (!rc || !p.quarantined || data || len)) ||
        (inject_at && recursive && !nested_ok)) {
        fprintf(stderr, "failure: expected trace %s, got %s (fail=%c inject=%c recursive=%d map=%d lock=%d complete=%d)\n",
                expected, trace, fail_at ? fail_at : '-', inject_at ? inject_at : '-', recursive, bad_map, bad_lock, p.source_complete);
        return 1;
    }
    if (!success) {
        data = (void *)1; len = 9;
        if (!portal_nvenc_experimental_encode(&p, copy, NULL, &data, &len) || data || len ||
            p.source_complete || p.busy || !p.quarantined || strcmp(trace, expected)) {
            fprintf(stderr, "quarantined object reused: %s\n", expected); return 1;
        }
    }
    return 0;
}
int main(void) {
    const char *stages = "CSMELUN";
    char expected[8] = {0};
    if (run(stages, 1, 1)) return 1;
    for (invalid = 1; invalid <= 6; invalid++)
        if (run("", 0, 0)) return 1;
    invalid = 0;
    for (size_t i = 0; stages[i]; i++) {
        expected[i] = stages[i]; expected[i+1] = 0;
        fail_at = stages[i];
        if (run(expected, 0, i >= 2)) return 1;
        fail_at = 0;
        for (recursive = 0; recursive <= 1; recursive++) {
            inject_at = stages[i];
            if (run(expected, 0, recursive ? 0 : i >= 2)) return 1;
        }
        inject_at = 0; recursive = 0;
    }
    for (bad_map = 1; bad_map <= 2; bad_map++)
        if (run("CSM", 0, 1)) return 1;
    bad_map = 0;
    for (bad_lock = 1; bad_lock <= 3; bad_lock++)
        if (run("CSMEL", 0, 1)) return 1;
    puts("offline fake callback ordering and quarantine checks pass (not driver validation)");
    return 0;
}
