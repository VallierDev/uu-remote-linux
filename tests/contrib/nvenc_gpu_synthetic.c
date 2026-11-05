/* Self-created device pixels only; no Portal, desktop or DMA-BUF imports. */
#include "../../c_engine/nvenc_engine.h"
#include <ffnvcodec/dynlink_cuda.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
typedef CUresult CUDAAPI (*Fill)(CUdeviceptr, unsigned char, size_t);
struct Pattern { Fill fill; unsigned char value; };
static int copy(void *opaque, uint64_t dst, uint32_t pitch) {
    struct Pattern *p = opaque;
    return p->fill((CUdeviceptr)dst, p->value, (size_t)pitch * 256) != CUDA_SUCCESS;
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    void *cuda = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!cuda) return 1;
    struct Pattern pattern = { .fill = (Fill)dlsym(cuda, "cuMemsetD8_v2") };
    if (!pattern.fill) return 1;
    FILE *out = fopen(argv[1], "wb");
    if (!out) return 1;
    NvencConfig config = { .width=256, .height=256, .fps=30, .gop=30, .cq=23,
        .target_bps=2000000, .max_bps=2000000, .buffer_bits=2000000 };
    NvencEngine *e = nvenc_create(&config);
    if (!e) return 1;
    for (int i = 0; i < 32; i++) {
        pattern.value = i % 2 ? 255 : 0;
        const uint8_t *data = NULL; uint32_t length = 0; int complete = 0;
        if (nvenc_encode_gpu(e, copy, &pattern, i == 0, &data, &length, &complete) ||
            !complete || !data || !length || fwrite(data, 1, length, out) != length) return 1;
    }
    nvenc_destroy(e);
    if (fclose(out)) return 1;
    puts("PASS: native NVENC GPU callback encoded 32 alternating synthetic frames");
    return 0;
}
