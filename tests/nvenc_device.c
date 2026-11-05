/* Idle device preflight; fake CUDA lookup, no GPU or Portal access. */
#include <dlfcn.h>
static void *device_symbol(void *, const char *);
#define dlsym device_symbol
#define main gpu_regression_main
#include "nvenc_gpu.c"
#undef main
#undef dlsym
static int missing, query_error, queries;
static CUresult CUDAAPI pci_id(char *out, int size, CUdevice device) {
    (void)device; assert(size >= 13); queries++;
    strcpy(out,"0000:ab:00.0");
    return query_error ? CUDA_ERROR_UNKNOWN : CUDA_SUCCESS;
}
static void *device_symbol(void *library, const char *name) {
    assert(library==(void *)1);
    assert(!strcmp(name,"cuDeviceGetPCIBusId"));
    return missing ? NULL : (void *)pci_id;
}
int main(void) {
    NvencEngine e=fixture(); e.cuda_lib=(void *)1;
    assert(nvenc_check_device(NULL,"0000:ab:00.0")==-EINVAL);
    assert(nvenc_check_device(&e,NULL)==-EINVAL);
    assert(nvenc_check_device(&e,"invalid")==-EINVAL && !queries);
    assert(!nvenc_check_device(&e,"0000:AB:00.0"));
    assert(nvenc_check_device(&e,"0000:ac:00.0")==-EXDEV);
    missing=1; assert(nvenc_check_device(&e,"0000:ab:00.0")==-ENOSYS); missing=0;
    query_error=1; assert(nvenc_check_device(&e,"0000:ab:00.0")==-EIO); query_error=0;
    int before=queries;
    e.gpu_busy=1; assert(nvenc_check_device(&e,"0000:ab:00.0")==-EBUSY); e.gpu_busy=0;
    e.egl_resource=(CUgraphicsResource)1;
    assert(nvenc_check_device(&e,"0000:ab:00.0")==-EBUSY); e.egl_resource=NULL;
    assert(queries==before && !e.gpu_quarantined);
    assert(!nvenc_check_device(&e,"0000:ab:00.0"));
    e.gpu_quarantined=1;
    assert(nvenc_check_device(&e,"0000:ab:00.0")==-EBUSY && e.gpu_quarantined);
    puts("PASS: idle device identity/mismatch, missing symbol, query failure, busy rejection; no quarantine mutation");
    return 0;
}
