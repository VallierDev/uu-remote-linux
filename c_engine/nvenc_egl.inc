int nvenc_check_device(NvencEngine *e, const char *expected_pci) {
    if (!e || !expected_pci || strlen(expected_pci) != 12 || !e->cuda_lib) return -EINVAL;
    if (e->gpu_busy || e->gpu_quarantined || e->egl_resource) return -EBUSY;
    CUresult (CUDAAPI *pci)(char *, int, CUdevice) = NULL;
    *(void **)(&pci) = dlsym(e->cuda_lib, "cuDeviceGetPCIBusId");
    if (!pci) return -ENOSYS;
    char actual[32] = {0};
    if (pci(actual, sizeof(actual), e->dev) != CUDA_SUCCESS) return -EIO;
    actual[sizeof(actual)-1] = 0;
    return strcasecmp(actual, expected_pci) ? -EXDEV : 0;
}

/* Metadata only; does not dereference device pointers or submit pixel work. */
static int nvenc_egl_layout(const CUeglFrame *f, uint32_t width, uint32_t height) {
    if (f->width!=width || f->height!=height || f->planeCount!=1 || f->numChannels!=4 ||
        f->cuFormat!=CU_AD_FORMAT_UNSIGNED_INT8 || f->eglColorFormat!=0x06) return 0;
    return (f->frameType==CU_EGL_FRAME_TYPE_ARRAY && f->frame.pArray[0]) ||
        (f->frameType==CU_EGL_FRAME_TYPE_PITCH && f->frame.pPitch[0] && (uint64_t)f->pitch>=(uint64_t)width*4);
}
int nvenc_inspect_egl(NvencEngine *e, void *image, const char *pci_id, int *views_complete) {
    if (!views_complete) return -EINVAL;
    *views_complete=0;
    if (!e || !image) return -EINVAL;
    int checked=nvenc_check_device(e,pci_id);
    if (checked) return checked;
    CUresult (CUDAAPI *reg)(CUgraphicsResource *, void *, unsigned int)=NULL;
    CUresult (CUDAAPI *mapped)(CUeglFrame *, CUgraphicsResource, unsigned int, unsigned int)=NULL;
    CUresult (CUDAAPI *unreg)(CUgraphicsResource)=NULL;
    *(void **)(&reg)=dlsym(e->cuda_lib,"cuGraphicsEGLRegisterImage");
    *(void **)(&mapped)=dlsym(e->cuda_lib,"cuGraphicsResourceGetMappedEglFrame");
    *(void **)(&unreg)=dlsym(e->cuda_lib,"cuGraphicsUnregisterResource");
    if (!reg || !mapped || !unreg) { *views_complete=1; return -ENOSYS; }
    if (!ctx_push(e)) goto unknown;
    if (reg(&e->egl_resource,image,0)!=CUDA_SUCCESS || !e->egl_resource) goto unknown;
    CUeglFrame frame={0};
    if (mapped(&frame,e->egl_resource,0,0)!=CUDA_SUCCESS) goto unknown;
    int valid=nvenc_egl_layout(&frame,e->width,e->height);
    if (unreg(e->egl_resource)!=CUDA_SUCCESS) goto unknown;
    e->egl_resource=NULL;
    if (e->cuCtxSynchronize()!=CUDA_SUCCESS) goto unknown;
    CUcontext popped=NULL;
    if (e->cuCtxPopCurrent(&popped)!=CUDA_SUCCESS || popped!=e->cu_ctx) goto unknown;
    *views_complete=1;
    return valid ? 0 : -ENOTSUP;
unknown:
    e->gpu_quarantined=1;
    return -EIO;
}

struct NvencEglCopy {
    NvencEngine *engine;
    void *image;
    CUresult (CUDAAPI *reg)(CUgraphicsResource *, void *, unsigned int);
    CUresult (CUDAAPI *mapped)(CUeglFrame *, CUgraphicsResource, unsigned int, unsigned int);
    CUresult (CUDAAPI *copy)(const CUDA_MEMCPY2D *);
};
static int nvenc_copy_egl(void *opaque, uint64_t dst, uint32_t pitch) {
    struct NvencEglCopy *s = opaque; NvencEngine *e = s->engine;
    if (s->reg(&e->egl_resource, s->image, 0) != CUDA_SUCCESS || !e->egl_resource) return -1;
    CUeglFrame f = {0};
    if (s->mapped(&f, e->egl_resource, 0, 0) != CUDA_SUCCESS ||
        !nvenc_egl_layout(&f,e->width,e->height)) return -1;
    CUDA_MEMCPY2D cp = { .dstMemoryType = CU_MEMORYTYPE_DEVICE,
        .dstDevice = (CUdeviceptr)dst, .dstPitch = pitch,
        .WidthInBytes = (size_t)e->width * 4, .Height = e->height };
    if (f.frameType == CU_EGL_FRAME_TYPE_ARRAY && f.frame.pArray[0]) {
        cp.srcMemoryType = CU_MEMORYTYPE_ARRAY; cp.srcArray = f.frame.pArray[0];
    } else if (f.frameType == CU_EGL_FRAME_TYPE_PITCH && f.frame.pPitch[0] && f.pitch >= cp.WidthInBytes) {
        cp.srcMemoryType = CU_MEMORYTYPE_DEVICE;
        cp.srcDevice = (CUdeviceptr)(uintptr_t)f.frame.pPitch[0]; cp.srcPitch = f.pitch;
    } else return -1;
    return s->copy(&cp) != CUDA_SUCCESS;
}
int nvenc_encode_egl(NvencEngine *e, void *image, const char *expected_pci,
    int idr, const uint8_t **data, uint32_t *len, int *source_complete) {
    if (data) *data = NULL;
    if (len) *len = 0;
    if (source_complete) *source_complete = 0;
    if (!e) return -1;
    if (!image || !expected_pci || strlen(expected_pci) != 12 || !data || !len || !source_complete ||
        e->gpu_busy || e->gpu_quarantined || e->egl_resource || !e->cuda_lib) goto fail;
    struct NvencEglCopy s = { .engine = e, .image = image };
    CUresult (CUDAAPI *unreg)(CUgraphicsResource);
#define EGL_LOAD(field, symbol) do { *(void **)(&(field)) = dlsym(e->cuda_lib, symbol); if (!(field)) goto fail; } while (0)
    EGL_LOAD(s.reg, "cuGraphicsEGLRegisterImage");
    EGL_LOAD(s.mapped, "cuGraphicsResourceGetMappedEglFrame");
    EGL_LOAD(s.copy, "cuMemcpy2D_v2");
    EGL_LOAD(unreg, "cuGraphicsUnregisterResource");
#undef EGL_LOAD
    if (nvenc_check_device(e, expected_pci)) goto fail;
    const uint8_t *packet = NULL; uint32_t size = 0;
    if (nvenc_encode_gpu(e, nvenc_copy_egl, &s, idr, &packet, &size, source_complete)) goto fail;
    // Encoding completed; release the imported CUDA view before publishing bytes.
    if (!ctx_push(e) || unreg(e->egl_resource) != CUDA_SUCCESS) goto fail;
    e->egl_resource = NULL;
    if (e->cuCtxSynchronize() != CUDA_SUCCESS) goto fail;
    CUcontext popped = NULL;
    if (e->cuCtxPopCurrent(&popped) != CUDA_SUCCESS || popped != e->cu_ctx) goto fail;
    *data = packet; *len = size; return 0;
fail:
    e->gpu_quarantined = 1;
    return -1;
}
