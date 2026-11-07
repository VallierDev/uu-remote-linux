/* Build with the same flags/libs as nvidia-egl-nvenc-synthetic.c. No GPU needed. */
#define main probe_main
#include "nvidia-egl-nvenc-synthetic.c"
#undef main

static int failure, unmaps;
static unsigned char output = 42;
static CUresult CUDAAPI fake_push(CUcontext ctx) { (void)ctx; return CUDA_SUCCESS; }
static CUresult CUDAAPI fake_pop(CUcontext *ctx) { *ctx = expected_ctx; return CUDA_SUCCESS; }
static NVENCSTATUS NVENCAPI fake_map(void *e, NV_ENC_MAP_INPUT_RESOURCE *p) {
    (void)e; p->mappedResource = (NV_ENC_INPUT_PTR)(uintptr_t)1; return NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_encode(void *e, NV_ENC_PIC_PARAMS *p) {
    (void)e; (void)p; return failure == 1 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_lock(void *e, NV_ENC_LOCK_BITSTREAM *p) {
    (void)e; p->bitstreamBufferPtr = &output; p->bitstreamSizeInBytes = 1;
    return failure == 2 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_unlock(void *e, NV_ENC_OUTPUT_PTR p) {
    (void)e; (void)p; return failure == 3 ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI fake_unmap(void *e, NV_ENC_INPUT_PTR p) {
    (void)e; (void)p; unmaps++; return NV_ENC_SUCCESS;
}
int main(void) {
    for (failure = 0; failure <= 3; failure++) {
        NvencEngine e = {0}; const uint8_t *data; uint32_t len;
        e.cu_ctx = (CUcontext)(uintptr_t)1;
        e.cuCtxPushCurrent = fake_push; e.cuCtxPopCurrent = fake_pop;
        e.reg_res.registeredResource = (NV_ENC_REGISTERED_PTR)(uintptr_t)1;
        e.width = W; e.height = H;
        e.fn.nvEncMapInputResource = fake_map;
        e.fn.nvEncEncodePicture = fake_encode; e.fn.nvEncLockBitstream = fake_lock;
        e.fn.nvEncUnlockBitstream = fake_unlock; e.fn.nvEncUnmapInputResource = fake_unmap;
        unmaps = 0; encode_uncertain = 1; encode_cleanup_failed = 1;
        int rc = guarded_encode(&e, &data, &len);
        if (e.is_zero_copy || e.cuCtxPopCurrent != fake_pop ||
            e.fn.nvEncEncodePicture != fake_encode || e.fn.nvEncLockBitstream != fake_lock ||
            e.fn.nvEncUnlockBitstream != fake_unlock || e.fn.nvEncUnmapInputResource != fake_unmap ||
            (!failure && (rc || encode_uncertain || encode_cleanup_failed || unmaps != 1 || len != 1)) ||
            (failure && (!encode_uncertain || unmaps != 0 || (failure != 3 && !rc)))) {
            fprintf(stderr, "fake case %d failed: rc=%d uncertain=%d unmaps=%d\n", failure, rc, encode_uncertain, unmaps);
            free(e.output_staging); return 1;
        }
        free(e.output_staging);
        printf("fake case %d: real unmaps=%d\n", failure, unmaps);
    }
    return 0;
}
