/* Native GPU entry with injected driver functions; no Portal or real GPU access. */
#include <assert.h>
#include "../c_engine/nvenc_engine.c"
static int stage, fail_at, idr;
static CUcontext context = (CUcontext)1;
static int next(void) { return ++stage == fail_at; }
static CUresult CUDAAPI push(CUcontext c) { assert(c == context); return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS; }
static int copy(void *p, uint64_t dst, uint32_t pitch) {
    (void)p; assert(dst == 4 && pitch == 8); return next();
}
static CUresult CUDAAPI sync_gpu(void) { return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS; }
static NVENCSTATUS NVENCAPI map(void *e, NV_ENC_MAP_INPUT_RESOURCE *p) {
    (void)e; p->mappedResource = (NV_ENC_INPUT_PTR)3; p->mappedBufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
    return next() ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI encode(void *e, NV_ENC_PIC_PARAMS *p) {
    (void)e; assert(p->inputPitch == 8 && p->inputWidth == 2 && p->inputHeight == 2);
    assert(p->encodePicFlags == (idr ? NV_ENC_PIC_FLAG_FORCEIDR : 0));
    return next() ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI lock_bits(void *e, NV_ENC_LOCK_BITSTREAM *p) {
    (void)e; static unsigned char bits[] = {1,2,3};
    p->bitstreamBufferPtr = bits; p->bitstreamSizeInBytes = sizeof(bits);
    return next() ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI unlock_bits(void *e, NV_ENC_OUTPUT_PTR p) {
    (void)e; (void)p; return next() ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static NVENCSTATUS NVENCAPI unmap(void *e, NV_ENC_INPUT_PTR p) {
    (void)e; (void)p; return next() ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS;
}
static CUresult CUDAAPI pop(CUcontext *p) { *p = context; return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS; }
static NvencEngine fixture(void) {
    return (NvencEngine){ .cu_ctx = context, .cu_dptr = 4, .width = 2, .height = 2,
        .cuCtxPushCurrent = push, .cuCtxPopCurrent = pop, .cuCtxSynchronize = sync_gpu,
        .encoder = (void*)1, .reg_res.registeredResource = (NV_ENC_REGISTERED_PTR)2,
        .bitstream_buf = (NV_ENC_OUTPUT_PTR)5,
        .fn = { .nvEncMapInputResource = map, .nvEncEncodePicture = encode,
            .nvEncLockBitstream = lock_bits, .nvEncUnlockBitstream = unlock_bits,
            .nvEncUnmapInputResource = unmap } };
}
int main(void) {
    for (int failure = 0; failure <= 9; failure++) {
        NvencEngine e = fixture(); fail_at = failure; stage = 0; idr = failure % 2;
        const uint8_t *data = (void*)1; uint32_t len = 999; int complete = 99;
        int r = nvenc_encode_gpu(&e, copy, NULL, idr, &data, &len, &complete);
        assert(complete == (!failure || failure > 3));
        if (failure) {
            assert(r && !data && !len && e.gpu_quarantined && stage == failure);
            assert(nvenc_encode_gpu(&e, copy, NULL, 1, &data, &len, &complete));
            assert(stage == failure && !complete);
            nvenc_destroy(&e); /* Must not free stack fixture or call any driver. */
        } else {
            assert(!r && data && len == 3 && data[2] == 3 && stage == 9);
            stage = 0;
            assert(!nvenc_encode_gpu(&e, copy, NULL, idr, &data, &len, &complete));
        }
        free(e.output_staging); /* Mock-only cleanup: no GPU owns this allocation. */
    }
    puts("PASS: native GPU encode reuse, IDR, completion, nine injected failures and quarantine");
    return 0;
}
