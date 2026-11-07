/* Shared native encoder storage; no Portal or EGL imports are owned here. */
int nvenc_encode_gpu(NvencEngine *e, NvencGpuCopy copy, void *opaque,
    int force_idr, const uint8_t **data, uint32_t *len, int *source_complete) {
    if (data) *data = NULL;
    if (len) *len = 0;
    if (source_complete) *source_complete = 0;
    if (!e) return -1;
    if (e->gpu_busy || e->gpu_quarantined) { e->gpu_quarantined = 1; return -1; }
    e->gpu_busy = 1;
    if (!data || !len || !source_complete || !copy || e->is_zero_copy ||
        !e->cu_dptr || !e->width || e->width > UINT32_MAX / 4 || !e->height ||
        !e->cuCtxSynchronize || !e->cuCtxPopCurrent || !e->encoder ||
        !e->reg_res.registeredResource || !e->bitstream_buf ||
        !e->fn.nvEncMapInputResource || !e->fn.nvEncEncodePicture ||
        !e->fn.nvEncLockBitstream || !e->fn.nvEncUnlockBitstream ||
        !e->fn.nvEncUnmapInputResource || !ctx_push(e)) goto fail;
    if (copy(opaque, (uint64_t)e->cu_dptr, e->width * 4) || e->gpu_quarantined) goto fail;
    if (e->cuCtxSynchronize() != CUDA_SUCCESS || e->gpu_quarantined) goto fail;
    *source_complete = 1;
    NV_ENC_MAP_INPUT_RESOURCE map = { .version = NV_ENC_MAP_INPUT_RESOURCE_VER,
        .registeredResource = e->reg_res.registeredResource };
    if (e->fn.nvEncMapInputResource(e->encoder, &map) != NV_ENC_SUCCESS ||
        !map.mappedResource || map.mappedBufferFmt != NV_ENC_BUFFER_FORMAT_ARGB) goto fail;
    NV_ENC_PIC_PARAMS pic = { .version = NV_ENC_PIC_PARAMS_VER,
        .inputWidth = e->width, .inputHeight = e->height, .inputPitch = e->width * 4,
        .inputBuffer = map.mappedResource, .outputBitstream = e->bitstream_buf,
        .bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB, .pictureStruct = NV_ENC_PIC_STRUCT_FRAME,
        .encodePicFlags = force_idr ? NV_ENC_PIC_FLAG_FORCEIDR : 0 };
    if (e->fn.nvEncEncodePicture(e->encoder, &pic) != NV_ENC_SUCCESS) goto fail;
    NV_ENC_LOCK_BITSTREAM lock = { .version = NV_ENC_LOCK_BITSTREAM_VER,
        .outputBitstream = e->bitstream_buf, .doNotWait = 0 };
    if (e->fn.nvEncLockBitstream(e->encoder, &lock) != NV_ENC_SUCCESS ||
        !lock.bitstreamBufferPtr || !lock.bitstreamSizeInBytes) goto fail;
    if (lock.bitstreamSizeInBytes > e->output_staging_cap) {
        uint8_t *grown = realloc(e->output_staging, lock.bitstreamSizeInBytes);
        if (!grown) goto fail;
        e->output_staging = grown; e->output_staging_cap = lock.bitstreamSizeInBytes;
    }
    memcpy(e->output_staging, lock.bitstreamBufferPtr, lock.bitstreamSizeInBytes);
    if (e->fn.nvEncUnlockBitstream(e->encoder, e->bitstream_buf) != NV_ENC_SUCCESS ||
        e->fn.nvEncUnmapInputResource(e->encoder, map.mappedResource) != NV_ENC_SUCCESS) goto fail;
    CUcontext popped = NULL;
    if (e->cuCtxPopCurrent(&popped) != CUDA_SUCCESS || popped != e->cu_ctx) goto fail;
    *data = e->output_staging; *len = lock.bitstreamSizeInBytes;
    e->gpu_busy = 0;
    return 0;
fail:
    /* Do not unmap, pop, destroy, or retry after uncertain driver progress.
     * The caller must retire this worker thread and retain its imported source. */
    e->gpu_quarantined = 1; e->gpu_busy = 0;
    return -1;
}
