#ifndef UU_DMABUF_FRAME_H
#define UU_DMABUF_FRAME_H
#include <stdint.h>
/* Borrowed single-plane RGB buffer. The producer retains ownership until the lease ends. */
typedef struct {
    int32_t fd;
    uint32_t fourcc, width, height, stride, offset, size;
    uint64_t modifier;
} UuDmaFrame;
#endif
