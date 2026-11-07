/* Validate the native CUDA EGL copy's layout boundary without a GPU. */
#define main gpu_entry_regression_main
#include "nvenc_gpu.c"
#undef main
static CUeglFrame offered;
static int copies, inject;
static CUresult CUDAAPI register_image(CUgraphicsResource *r, void *image, unsigned int flags) {
    assert(image && !flags); *r = (CUgraphicsResource)7;
    return inject == 1 ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static CUresult CUDAAPI mapped_image(CUeglFrame *f, CUgraphicsResource r, unsigned int i, unsigned int m) {
    assert(r && !i && !m); *f = offered;
    return inject == 2 ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static CUresult CUDAAPI copy_image(const CUDA_MEMCPY2D *cp) {
    copies++;
    assert(cp->dstDevice == 4 && cp->dstPitch == 8 && cp->WidthInBytes == 8 && cp->Height == 2);
    assert(cp->srcMemoryType == CU_MEMORYTYPE_ARRAY || cp->srcPitch >= 8);
    return inject == 3 ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
int main(void) {
    for (int mode = 0; mode < 12; mode++) {
        NvencEngine e = fixture(); copies = inject = 0;
        offered = (CUeglFrame){ .width=2, .height=2, .planeCount=1, .numChannels=4,
            .cuFormat=CU_AD_FORMAT_UNSIGNED_INT8, .eglColorFormat=0x06,
            .frameType=CU_EGL_FRAME_TYPE_ARRAY, .frame.pArray={ (CUarray)1 } };
        if (mode == 1 || mode == 2) {
            offered.frameType=CU_EGL_FRAME_TYPE_PITCH; offered.frame.pPitch[0]=(void*)1;
            offered.pitch= mode == 1 ? 8 : 7;
        }
        if (mode == 3) offered.width=3;
        if (mode == 4) offered.planeCount=2;
        if (mode == 5) offered.numChannels=3;
        if (mode == 6) offered.cuFormat=CU_AD_FORMAT_FLOAT;
        if (mode == 7) offered.eglColorFormat=0;
        if (mode == 8) offered.frame.pArray[0]=NULL;
        if (mode >= 9) inject=mode-8;
        struct NvencEglCopy args = { .engine=&e, .image=(void*)1,
            .reg=register_image, .mapped=mapped_image, .copy=copy_image };
        int result=nvenc_copy_egl(&args,4,8);
        assert((result == 0) == (mode < 2));
        assert(copies == (mode < 2 || mode == 11));
        assert(e.egl_resource == (CUgraphicsResource)7); /* Retained until outer completion. */
    }
    puts("PASS: native EGL ARRAY/PITCH copy, malformed layouts and CUDA failure rejection");
    return 0;
}
