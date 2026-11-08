#include "portal-nvenc-experimental.h"
#include <string.h>

void portal_nvenc_experimental_quarantine(PortalNvencExperimental *p) {
    if (p) p->quarantined = 1;
}
int portal_nvenc_experimental_encode(PortalNvencExperimental *p, portal_nvenc_copy copy,
                                     void *opaque, const uint8_t **data, uint32_t *len) {
    if (data) *data = NULL;
    if (len) *len = 0;
    if (!p) return -1;
    p->source_complete = 0;
    if (p->busy) { portal_nvenc_experimental_quarantine(p); return -1; }
    if (p->quarantined) return -1;
    p->busy = 1;
    if (!data || !len || !copy || !p->fn || !p->sync || !p->output || !p->dst || !p->surface ||
        !p->encoder || !p->bitstream || !p->width || p->width > UINT32_MAX / 4 || !p->height ||
        !p->fn->nvEncMapInputResource || !p->fn->nvEncEncodePicture ||
        !p->fn->nvEncLockBitstream || !p->fn->nvEncUnlockBitstream ||
        !p->fn->nvEncUnmapInputResource) goto fail;
    if (copy(opaque, p->dst, p->width * 4) || p->quarantined) goto fail;
    if (p->sync() != CUDA_SUCCESS || p->quarantined) goto fail;
    p->source_complete = 1;
    NV_ENC_MAP_INPUT_RESOURCE map = {0};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER; map.registeredResource = p->surface;
    if (p->fn->nvEncMapInputResource(p->encoder, &map) != NV_ENC_SUCCESS || p->quarantined) goto fail;
    if (!map.mappedResource || map.mappedBufferFmt != NV_ENC_BUFFER_FORMAT_ARGB) goto fail;
    NV_ENC_PIC_PARAMS pic = {0};
    pic.version = NV_ENC_PIC_PARAMS_VER; pic.inputWidth = p->width; pic.inputHeight = p->height;
    pic.inputPitch = p->width * 4; pic.inputBuffer = map.mappedResource;
    pic.outputBitstream = p->bitstream; pic.bufferFmt = map.mappedBufferFmt;
    pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME; pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    if (p->fn->nvEncEncodePicture(p->encoder, &pic) != NV_ENC_SUCCESS || p->quarantined) goto fail;
    NV_ENC_LOCK_BITSTREAM lock = {0};
    lock.version = NV_ENC_LOCK_BITSTREAM_VER; lock.outputBitstream = p->bitstream;
    if (p->fn->nvEncLockBitstream(p->encoder, &lock) != NV_ENC_SUCCESS || p->quarantined) goto fail;
    if (!lock.bitstreamBufferPtr || !lock.bitstreamSizeInBytes || lock.bitstreamSizeInBytes > p->capacity) goto fail;
    memcpy(p->output, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
    if (p->fn->nvEncUnlockBitstream(p->encoder, p->bitstream) != NV_ENC_SUCCESS || p->quarantined) goto fail;
    if (p->fn->nvEncUnmapInputResource(p->encoder, map.mappedResource) != NV_ENC_SUCCESS || p->quarantined) goto fail;
    *data = p->output; *len = lock.bitstreamSizeInBytes;
    p->busy = 0;
    return 0;
fail:
    p->busy = 0;
    portal_nvenc_experimental_quarantine(p);
    return -1;
}
