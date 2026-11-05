/* CUDA view lifecycle only: injected functions, no real GPU or Portal. */
#include <dlfcn.h>
static void *symbol(void *,const char *);
#define dlsym symbol
#define main gpu_regression_main
#include "nvenc_gpu.c"
#undef main
#undef dlsym
static int malformed;
static CUresult CUDAAPI pci(char *out,int size,CUdevice dev) {
    (void)dev; assert(size>=13); strcpy(out,"0000:01:00.0"); return CUDA_SUCCESS;
}
static CUresult CUDAAPI reg_image(CUgraphicsResource *r,void *image,unsigned int flags) {
    assert(image && !flags); *r=(CUgraphicsResource)7;
    return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static CUresult CUDAAPI mapped_image(CUeglFrame *f,CUgraphicsResource r,unsigned int a,unsigned int b) {
    assert(r==(CUgraphicsResource)7 && !a && !b);
    *f=(CUeglFrame){.width=malformed ? 3u : 2u,.height=2,.planeCount=1,.numChannels=4,
        .cuFormat=CU_AD_FORMAT_UNSIGNED_INT8,.eglColorFormat=0x06,
        .frameType=CU_EGL_FRAME_TYPE_ARRAY,.frame.pArray={(CUarray)1}};
    return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static CUresult CUDAAPI unreg_image(CUgraphicsResource r) {
    assert(r==(CUgraphicsResource)7); return next() ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static void *symbol(void *lib,const char *name) {
    assert(lib==(void *)1);
    if (!strcmp(name,"cuDeviceGetPCIBusId")) return (void *)pci;
    if (!strcmp(name,"cuGraphicsEGLRegisterImage")) return (void *)reg_image;
    if (!strcmp(name,"cuGraphicsResourceGetMappedEglFrame")) return (void *)mapped_image;
    if (!strcmp(name,"cuGraphicsUnregisterResource")) return (void *)unreg_image;
    assert(!"unexpected driver lookup"); return NULL;
}
int main(void) {
    for (int failure=0;failure<=6;failure++) {
        NvencEngine e=fixture(); e.cuda_lib=(void *)1;
        stage=0; fail_at=failure; int complete=99;
        int result=nvenc_inspect_egl(&e,(void *)2,"0000:01:00.0",&complete);
        if (!failure) assert(!result && complete && !e.egl_resource && !e.gpu_quarantined && stage==6);
        else {
            assert(result==-EIO && !complete && e.gpu_quarantined && stage==failure);
            assert(nvenc_inspect_egl(&e,(void *)2,"0000:01:00.0",&complete)==-EBUSY);
            assert(stage==failure && !complete);
        }
    }
    NvencEngine e=fixture(); e.cuda_lib=(void *)1;
    stage=fail_at=0; malformed=1; int complete=0;
    assert(nvenc_inspect_egl(&e,(void *)2,"0000:01:00.0",&complete)==-ENOTSUP);
    assert(complete && !e.egl_resource && !e.gpu_quarantined && stage==6);
    puts("PASS: CUDA view inspection, clean layout rejection, six injected failures and no retry after quarantine");
}
