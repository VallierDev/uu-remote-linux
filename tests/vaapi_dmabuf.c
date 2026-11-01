/* Hardware regression: cc tests/vaapi_dmabuf.c -o /tmp/vaapi-dma -lgbm -ldl
 * Requires a VAAPI render node. stdout is synthetic H.264: red DMA-BUF, then black host upload.
 * No desktop capture or account credentials are used. */
#include <assert.h>
#include <gbm.h>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include "../c_engine/vaapi_engine.c"

static void assert_color(struct gbm_bo *bo, int write_red) {
    uint32_t stride=gbm_bo_get_stride(bo);
    int fd=gbm_bo_get_fd(bo);
    assert(fd>=0);
    size_t size=(size_t)stride*720;
    uint8_t *pixels=mmap(NULL,size,PROT_READ | PROT_WRITE,MAP_SHARED,fd,0);
    assert(pixels!=MAP_FAILED);
    struct dma_buf_sync sync={.flags=DMA_BUF_SYNC_START | DMA_BUF_SYNC_RW};
    assert(ioctl(fd,DMA_BUF_IOCTL_SYNC,&sync)==0);
    for (unsigned y=0; y<720; y++) {
        for (unsigned x=0; x<1280; x++) {
            uint8_t *pixel=pixels+y*stride+x*4;
            if (write_red) { pixel[0]=0; pixel[1]=0; pixel[2]=255; pixel[3]=255; }
            else assert(pixel[0]==0 && pixel[1]==0 && pixel[2]==255 && pixel[3]==255);
        }
    }
    sync.flags=DMA_BUF_SYNC_END | DMA_BUF_SYNC_RW;
    assert(ioctl(fd,DMA_BUF_IOCTL_SYNC,&sync)==0);
    munmap(pixels,size); close(fd);
}
static void encode(VaapiEngine *engine) {
    const uint8_t *data=NULL;
    uint32_t size=0;
    assert(vaapi_encode_frame(engine, 1, &data, &size)==0 && data && size);
    assert(fwrite(data, 1, size, stdout)==size);
}
int main(void) {
    const char *device=getenv("UU_VAAPI_DEVICE");
    if (!device) device="/dev/dri/renderD128";
    int fd=open(device, O_RDWR | O_CLOEXEC);
    assert(fd>=0);
    struct gbm_device *gbm=gbm_create_device(fd);
    assert(gbm);
    const uint64_t modifier=0;
    struct gbm_bo *bo=gbm_bo_create_with_modifiers2(gbm,1280,720,GBM_FORMAT_ARGB8888,&modifier,1,GBM_BO_USE_RENDERING);
    assert(bo && gbm_bo_get_modifier(bo)==0);
    assert_color(bo,1);
    int dma=gbm_bo_get_fd(bo);
    assert(dma>=0);
    UuDmaFrame frame={.fd=dma,.fourcc=GBM_FORMAT_ARGB8888,.width=1280,.height=720,
        .stride=gbm_bo_get_stride(bo),.size=gbm_bo_get_stride(bo)*720,.modifier=0};
    VaapiConfig config={.width=1280,.height=720,.cap_width=1280,.cap_height=720,.fps=30,.gop=30,
        .target_bps=1000000,.max_bps=2000000,.buffer_bits=500000,.device=device};
    VaapiEngine *engine=vaapi_create(&config);
    assert(engine && vaapi_import_frame(engine,&frame)==0);
    encode(engine);
    uint8_t *black=calloc(1280*720,4);
    assert(black && vaapi_upload_host_buffer(engine,black,1280*4)==0);
    encode(engine);
    /* Host fallback must allocate its own surface, never overwrite the leased compositor buffer. */
    assert_color(bo,0);
    free(black);
    vaapi_destroy(engine);
    close(dma); gbm_bo_destroy(bo); gbm_device_destroy(gbm); close(fd);
    return 0;
}
