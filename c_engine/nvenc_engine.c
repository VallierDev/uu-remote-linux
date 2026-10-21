#include "nvenc_engine.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <ffnvcodec/dynlink_cuda.h>
#include <ffnvcodec/nvEncodeAPI.h>

// Struct revisions come from the vendored nvEncodeAPI.h (*_VER macros), so the session must be
// opened with that header's NVENCAPI_VERSION; older drivers are rejected in nvenc_create.
#define CU_MEMHOSTREGISTER_DEVICEMAP 0x02

typedef CUresult CUDAAPI (*tcuMemHostRegister)(void *p, size_t bytesize, unsigned int Flags);
typedef CUresult CUDAAPI (*tcuMemHostGetDevicePointer)(CUdeviceptr *pdptr, void *p, unsigned int Flags);
typedef CUresult CUDAAPI (*tcuMemHostUnregister)(void *p);
typedef NVENCSTATUS NVENCAPI (*tNvEncodeAPICreateInstance)(NV_ENCODE_API_FUNCTION_LIST *functionList);
typedef NVENCSTATUS NVENCAPI (*tNvEncodeAPIGetMaxSupportedVersion)(uint32_t *version);

struct NvencEngine {
    void *cuda_lib;
    void *nvenc_lib;

    tcuInit *cuInit;
    tcuDeviceGet *cuDeviceGet;
    tcuDevicePrimaryCtxRetain *cuDevicePrimaryCtxRetain;
    tcuDevicePrimaryCtxRelease *cuDevicePrimaryCtxRelease;
    tcuCtxCreate_v2 *cuCtxCreate;
    tcuCtxDestroy_v2 *cuCtxDestroy;
    tcuCtxPushCurrent_v2 *cuCtxPushCurrent;
    tcuCtxPopCurrent_v2 *cuCtxPopCurrent;
    tcuMemAlloc_v2 *cuMemAlloc;
    tcuMemFree_v2 *cuMemFree;
    tcuMemcpyHtoD_v2 *cuMemcpyHtoD;
    tcuMemHostRegister cuMemHostRegister;
    tcuMemHostGetDevicePointer cuMemHostGetDevicePointer;
    tcuMemHostUnregister cuMemHostUnregister;

    CUdevice dev;
    CUcontext cu_ctx;
    int is_standalone_ctx; // cuCtxCreate fallback (primary context unavailable, seen on GB10)
    CUdeviceptr cu_dptr;
    size_t frame_bytes;

    // Pinned XShm buffer registered for direct DMA reads
    void *registered_host_ptr;
    int is_zero_copy;

    NV_ENCODE_API_FUNCTION_LIST fn;
    void *encoder;

    NV_ENC_REGISTER_RESOURCE reg_res;
    NV_ENC_OUTPUT_PTR bitstream_buf;

    uint32_t width;
    uint32_t height;
    int is_hevc;

    uint8_t *output_staging;
    size_t output_staging_cap;
};

// Returns 1 when the context was pushed; pop only in that case.
static int ctx_push(NvencEngine *engine) {
    return engine->cuCtxPushCurrent && engine->cu_ctx && engine->cuCtxPushCurrent(engine->cu_ctx) == CUDA_SUCCESS;
}

static void ctx_pop(NvencEngine *engine, int pushed) {
    if (pushed && engine->cuCtxPopCurrent) {
        CUcontext dummy = NULL;
        engine->cuCtxPopCurrent(&dummy);
    }
}

NvencEngine* nvenc_create(const NvencConfig* config) {
    if (!config || config->width == 0 || config->height == 0) {
        return NULL;
    }

    NvencEngine *engine = (NvencEngine*)calloc(1, sizeof(NvencEngine));
    if (!engine) return NULL;

    engine->width = config->width;
    engine->height = config->height;
    engine->is_hevc = config->is_hevc;

    // 1. Load CUDA driver
    engine->cuda_lib = dlopen("libcuda.so.1", RTLD_NOW);
    if (!engine->cuda_lib) {
        fprintf(stderr, "[nvenc] dlopen libcuda.so.1 failed\n");
        free(engine);
        return NULL;
    }

    engine->cuInit = (tcuInit*)dlsym(engine->cuda_lib, "cuInit");
    engine->cuDeviceGet = (tcuDeviceGet*)dlsym(engine->cuda_lib, "cuDeviceGet");
    engine->cuDevicePrimaryCtxRetain = (tcuDevicePrimaryCtxRetain*)dlsym(engine->cuda_lib, "cuDevicePrimaryCtxRetain");
    engine->cuDevicePrimaryCtxRelease = (tcuDevicePrimaryCtxRelease*)dlsym(engine->cuda_lib, "cuDevicePrimaryCtxRelease_v2");
    if (!engine->cuDevicePrimaryCtxRelease)
        engine->cuDevicePrimaryCtxRelease = (tcuDevicePrimaryCtxRelease*)dlsym(engine->cuda_lib, "cuDevicePrimaryCtxRelease");
    engine->cuCtxCreate = (tcuCtxCreate_v2*)dlsym(engine->cuda_lib, "cuCtxCreate_v2");
    engine->cuCtxDestroy = (tcuCtxDestroy_v2*)dlsym(engine->cuda_lib, "cuCtxDestroy_v2");
    engine->cuCtxPushCurrent = (tcuCtxPushCurrent_v2*)dlsym(engine->cuda_lib, "cuCtxPushCurrent_v2");
    engine->cuCtxPopCurrent = (tcuCtxPopCurrent_v2*)dlsym(engine->cuda_lib, "cuCtxPopCurrent_v2");
    engine->cuMemAlloc = (tcuMemAlloc_v2*)dlsym(engine->cuda_lib, "cuMemAlloc_v2");
    engine->cuMemFree = (tcuMemFree_v2*)dlsym(engine->cuda_lib, "cuMemFree_v2");
    engine->cuMemcpyHtoD = (tcuMemcpyHtoD_v2*)dlsym(engine->cuda_lib, "cuMemcpyHtoD_v2");
    engine->cuMemHostRegister = (tcuMemHostRegister)dlsym(engine->cuda_lib, "cuMemHostRegister_v2");
    engine->cuMemHostGetDevicePointer = (tcuMemHostGetDevicePointer)dlsym(engine->cuda_lib, "cuMemHostGetDevicePointer_v2");
    engine->cuMemHostUnregister = (tcuMemHostUnregister)dlsym(engine->cuda_lib, "cuMemHostUnregister");

    if (!engine->cuInit || !engine->cuDeviceGet || !engine->cuDevicePrimaryCtxRetain ||
        !engine->cuDevicePrimaryCtxRelease || !engine->cuCtxPushCurrent || !engine->cuCtxPopCurrent ||
        !engine->cuMemAlloc || !engine->cuMemFree || !engine->cuMemcpyHtoD) {
        fprintf(stderr, "[nvenc] Missing CUDA driver symbols\n");
        nvenc_destroy(engine);
        return NULL;
    }

    CUresult cures = engine->cuInit(0);
    if (cures != CUDA_SUCCESS) {
        fprintf(stderr, "[nvenc] cuInit failed: %d\n", cures);
        nvenc_destroy(engine);
        return NULL;
    }

    cures = engine->cuDeviceGet(&engine->dev, 0);
    if (cures != CUDA_SUCCESS) {
        fprintf(stderr, "[nvenc] cuDeviceGet failed: %d\n", cures);
        nvenc_destroy(engine);
        return NULL;
    }

    cures = engine->cuDevicePrimaryCtxRetain(&engine->cu_ctx, engine->dev);
    if ((cures != CUDA_SUCCESS || !engine->cu_ctx) && engine->cuCtxCreate && engine->cuCtxDestroy) {
        engine->cu_ctx = NULL;
        // cuCtxCreate makes the new context current; the push below makes it current again
        cures = engine->cuCtxCreate(&engine->cu_ctx, 0, engine->dev);
        if (cures == CUDA_SUCCESS && engine->cu_ctx) {
            CUcontext dummy = NULL;
            engine->cuCtxPopCurrent(&dummy);
            engine->is_standalone_ctx = 1;
        }
    }
    if (cures != CUDA_SUCCESS || !engine->cu_ctx) {
        fprintf(stderr, "[nvenc] CUDA context creation failed: %d\n", cures);
        engine->cu_ctx = NULL;
        nvenc_destroy(engine);
        return NULL;
    }
    int pushed = ctx_push(engine);
    if (!pushed) {
        fprintf(stderr, "[nvenc] cuCtxPushCurrent failed\n");
        nvenc_destroy(engine);
        return NULL;
    }

    // 2. Load NVENC
    engine->nvenc_lib = dlopen("libnvidia-encode.so.1", RTLD_NOW);
    if (!engine->nvenc_lib) {
        fprintf(stderr, "[nvenc] dlopen libnvidia-encode.so.1 failed\n");
        goto fail;
    }

    // Like FFmpeg: the driver must support the header's API version, otherwise fall back.
    tNvEncodeAPIGetMaxSupportedVersion get_max_ver = (tNvEncodeAPIGetMaxSupportedVersion)dlsym(
        engine->nvenc_lib, "NvEncodeAPIGetMaxSupportedVersion");
    uint32_t max_supported_ver = 0;
    if (!get_max_ver || get_max_ver(&max_supported_ver) != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] NvEncodeAPIGetMaxSupportedVersion failed\n");
        goto fail;
    }
    if (max_supported_ver < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        fprintf(stderr, "[nvenc] Driver NVENC API %u.%u is older than required %d.%d; update the NVIDIA driver\n",
                max_supported_ver >> 4, max_supported_ver & 0xf, NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
        goto fail;
    }

    tNvEncodeAPICreateInstance create_inst = (tNvEncodeAPICreateInstance)dlsym(engine->nvenc_lib, "NvEncodeAPICreateInstance");
    if (!create_inst) {
        fprintf(stderr, "[nvenc] dlsym NvEncodeAPICreateInstance failed\n");
        goto fail;
    }

    memset(&engine->fn, 0, sizeof(engine->fn));
    engine->fn.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    NVENCSTATUS nvres = create_inst(&engine->fn);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] NvEncodeAPICreateInstance failed: %d\n", nvres);
        goto fail;
    }

    // 3. Open Encode Session
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open_params;
    memset(&open_params, 0, sizeof(open_params));
    open_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    open_params.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    open_params.device = engine->cu_ctx;
    open_params.apiVersion = NVENCAPI_VERSION;

    nvres = engine->fn.nvEncOpenEncodeSessionEx(&open_params, &engine->encoder);
    if (nvres != NV_ENC_SUCCESS || !engine->encoder) {
        fprintf(stderr, "[nvenc] nvEncOpenEncodeSessionEx failed: %d\n", nvres);
        engine->encoder = NULL;
        goto fail;
    }

    // 4. Query Preset Config
    GUID codec_guid = engine->is_hevc ? NV_ENC_CODEC_HEVC_GUID : NV_ENC_CODEC_H264_GUID;
    NV_ENC_PRESET_CONFIG preset_cfg;
    memset(&preset_cfg, 0, sizeof(preset_cfg));
    preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
    preset_cfg.presetCfg.version = NV_ENC_CONFIG_VER;

    nvres = engine->fn.nvEncGetEncodePresetConfigEx(
        engine->encoder,
        codec_guid,
        NV_ENC_PRESET_P4_GUID,
        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        &preset_cfg
    );
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncGetEncodePresetConfigEx failed: %d\n", nvres);
        goto fail;
    }

    NV_ENC_CONFIG enc_cfg;
    memcpy(&enc_cfg, &preset_cfg.presetCfg, sizeof(NV_ENC_CONFIG));

    enc_cfg.gopLength = config->gop > 0 ? config->gop : NVENC_INFINITE_GOPLENGTH;
    enc_cfg.frameIntervalP = 1; // Strict low-latency: zero B-frames
    enc_cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
    enc_cfg.rcParams.averageBitRate = (uint32_t)(config->target_bps > 0 ? config->target_bps : 10000000);
    enc_cfg.rcParams.maxBitRate = (uint32_t)(config->max_bps > 0 ? config->max_bps : 15000000);
    enc_cfg.rcParams.vbvBufferSize = (uint32_t)(config->buffer_bits > 0 ? config->buffer_bits : (config->max_bps > 0 ? config->max_bps / 10 : 1500000));
    enc_cfg.rcParams.targetQuality = (uint8_t)(config->cq > 0 ? config->cq : 28);
    NV_ENC_CAPS_PARAM caps_param;
    memset(&caps_param, 0, sizeof(caps_param));
    caps_param.version = NV_ENC_CAPS_PARAM_VER;
    int caps_val = 0;
    if (config->chroma_444) {
        // GPUs without 4:4:4 encode fail here; the caller falls back to FFmpeg/libx264, which keep 4:4:4.
        caps_param.capsToQuery = NV_ENC_CAPS_SUPPORT_YUV444_ENCODE;
        if (engine->fn.nvEncGetEncodeCaps(engine->encoder, codec_guid, &caps_param, &caps_val) != NV_ENC_SUCCESS || caps_val <= 0) {
            fprintf(stderr, "[nvenc] 4:4:4 encoding not supported for this codec\n");
            goto fail;
        }
        // NVENC rejects AQ / temporal AQ in 4:4:4 mode.
        enc_cfg.rcParams.enableAQ = 0;
        enc_cfg.rcParams.enableTemporalAQ = 0;
    } else {
        enc_cfg.rcParams.enableAQ = 1;
        // Temporal AQ only where the hardware/codec reports it
        caps_param.capsToQuery = NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ;
        caps_val = 0;
        enc_cfg.rcParams.enableTemporalAQ =
            engine->fn.nvEncGetEncodeCaps(engine->encoder, codec_guid, &caps_param, &caps_val) == NV_ENC_SUCCESS && caps_val > 0;
    }

    if (engine->is_hevc) {
        enc_cfg.profileGUID = config->chroma_444 ? NV_ENC_HEVC_PROFILE_FREXT_GUID : NV_ENC_HEVC_PROFILE_MAIN_GUID;
        enc_cfg.encodeCodecConfig.hevcConfig.chromaFormatIDC = config->chroma_444 ? 3 : 1;
        enc_cfg.encodeCodecConfig.hevcConfig.idrPeriod = enc_cfg.gopLength;
        enc_cfg.encodeCodecConfig.hevcConfig.repeatSPSPPS = 1;
        enc_cfg.encodeCodecConfig.hevcConfig.outputAUD = 1;
    } else {
        enc_cfg.profileGUID = config->chroma_444 ? NV_ENC_H264_PROFILE_HIGH_444_GUID : NV_ENC_H264_PROFILE_BASELINE_GUID;
        enc_cfg.encodeCodecConfig.h264Config.chromaFormatIDC = config->chroma_444 ? 3 : 1;
        enc_cfg.encodeCodecConfig.h264Config.idrPeriod = enc_cfg.gopLength;
        enc_cfg.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
        enc_cfg.encodeCodecConfig.h264Config.outputAUD = 1;
    }

    // 5. Initialize Encoder
    NV_ENC_INITIALIZE_PARAMS init_params;
    memset(&init_params, 0, sizeof(init_params));
    init_params.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init_params.encodeGUID = codec_guid;
    init_params.presetGUID = NV_ENC_PRESET_P4_GUID;
    init_params.encodeWidth = engine->width;
    init_params.encodeHeight = engine->height;
    init_params.darWidth = engine->width;
    init_params.darHeight = engine->height;
    init_params.frameRateNum = config->fps > 0 ? config->fps : 60;
    init_params.frameRateDen = 1;
    init_params.enablePTD = 1;
    init_params.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    init_params.encodeConfig = &enc_cfg;

    nvres = engine->fn.nvEncInitializeEncoder(engine->encoder, &init_params);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncInitializeEncoder failed: %d\n", nvres);
        goto fail;
    }

    // 6. Device input buffer (ARGB32), used until a host buffer is registered
    engine->frame_bytes = (size_t)engine->width * engine->height * 4;
    cures = engine->cuMemAlloc(&engine->cu_dptr, engine->frame_bytes);
    if (cures != CUDA_SUCCESS) {
        fprintf(stderr, "[nvenc] cuMemAlloc failed: %d\n", cures);
        engine->cu_dptr = 0;
        goto fail;
    }

    memset(&engine->reg_res, 0, sizeof(engine->reg_res));
    engine->reg_res.version = NV_ENC_REGISTER_RESOURCE_VER;
    engine->reg_res.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    engine->reg_res.width = engine->width;
    engine->reg_res.height = engine->height;
    engine->reg_res.pitch = engine->width * 4;
    engine->reg_res.resourceToRegister = (void*)engine->cu_dptr;
    engine->reg_res.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;

    nvres = engine->fn.nvEncRegisterResource(engine->encoder, &engine->reg_res);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncRegisterResource failed: %d\n", nvres);
        engine->reg_res.registeredResource = NULL;
        goto fail;
    }

    // 7. Create bitstream output buffer
    NV_ENC_CREATE_BITSTREAM_BUFFER bs_buf;
    memset(&bs_buf, 0, sizeof(bs_buf));
    bs_buf.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    nvres = engine->fn.nvEncCreateBitstreamBuffer(engine->encoder, &bs_buf);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncCreateBitstreamBuffer failed: %d\n", nvres);
        goto fail;
    }
    engine->bitstream_buf = bs_buf.bitstreamBuffer;

    engine->output_staging_cap = 2 * 1024 * 1024;
    engine->output_staging = (uint8_t*)malloc(engine->output_staging_cap);
    if (!engine->output_staging) {
        fprintf(stderr, "[nvenc] output buffer allocation failed\n");
        goto fail;
    }

    ctx_pop(engine, pushed);
    return engine;

fail:
    // Pop the creation push first; nvenc_destroy pushes and pops on its own.
    ctx_pop(engine, pushed);
    nvenc_destroy(engine);
    return NULL;
}

int nvenc_register_host_buffer(NvencEngine* engine, void* host_ptr, size_t size) {
    if (!engine || !host_ptr || size < engine->frame_bytes) {
        return -1;
    }
    if (!engine->cuMemHostRegister || !engine->cuMemHostGetDevicePointer || !engine->cuMemHostUnregister) {
        return -2;
    }

    int pushed = ctx_push(engine);
    if (!pushed) return -2;

    // If already registered, unregister first
    nvenc_unregister_host_buffer(engine);

    CUresult cures = engine->cuMemHostRegister(host_ptr, size, CU_MEMHOSTREGISTER_DEVICEMAP);
    if (cures != CUDA_SUCCESS) {
        fprintf(stderr, "[nvenc] cuMemHostRegister failed: %d\n", cures);
        ctx_pop(engine, pushed);
        return -3;
    }

    CUdeviceptr dev_ptr = 0;
    cures = engine->cuMemHostGetDevicePointer(&dev_ptr, host_ptr, 0);
    if (cures != CUDA_SUCCESS || dev_ptr == 0) {
        fprintf(stderr, "[nvenc] cuMemHostGetDevicePointer failed: %d\n", cures);
        engine->cuMemHostUnregister(host_ptr);
        ctx_pop(engine, pushed);
        return -4;
    }

    // Register the pinned buffer before releasing the device buffer, so a failure keeps the copy path working
    NV_ENC_REGISTER_RESOURCE new_reg;
    memset(&new_reg, 0, sizeof(new_reg));
    new_reg.version = NV_ENC_REGISTER_RESOURCE_VER;
    new_reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR;
    new_reg.width = engine->width;
    new_reg.height = engine->height;
    new_reg.pitch = engine->width * 4;
    new_reg.resourceToRegister = (void*)dev_ptr;
    new_reg.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;

    NVENCSTATUS nvres = engine->fn.nvEncRegisterResource(engine->encoder, &new_reg);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncRegisterResource (pinned host) failed: %d\n", nvres);
        engine->cuMemHostUnregister(host_ptr);
        ctx_pop(engine, pushed);
        return -5;
    }

    if (engine->reg_res.registeredResource) {
        engine->fn.nvEncUnregisterResource(engine->encoder, engine->reg_res.registeredResource);
    }
    if (engine->cu_dptr) {
        engine->cuMemFree(engine->cu_dptr);
        engine->cu_dptr = 0;
    }
    engine->reg_res = new_reg;
    engine->registered_host_ptr = host_ptr;
    engine->is_zero_copy = 1;

    ctx_pop(engine, pushed);
    return 0;
}

// Caller has the context current.
void nvenc_unregister_host_buffer(NvencEngine* engine) {
    if (!engine || !engine->is_zero_copy) return;

    if (engine->reg_res.registeredResource && engine->encoder) {
        engine->fn.nvEncUnregisterResource(engine->encoder, engine->reg_res.registeredResource);
        memset(&engine->reg_res, 0, sizeof(engine->reg_res));
    }
    if (engine->registered_host_ptr && engine->cuMemHostUnregister) {
        engine->cuMemHostUnregister(engine->registered_host_ptr);
        engine->registered_host_ptr = NULL;
    }
    engine->is_zero_copy = 0;
}

int nvenc_encode_frame(
    NvencEngine* engine,
    const uint8_t* input_argb,
    uint32_t pitch,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
) {
    if (!engine || !out_data || !out_len || !engine->reg_res.registeredResource) {
        return -1;
    }
    *out_data = NULL;
    *out_len = 0;

    int pushed = ctx_push(engine);
    if (!pushed) return -1;

    // Without a registered host buffer, copy the frame into the device buffer
    if (!engine->is_zero_copy) {
        if (!input_argb) {
            ctx_pop(engine, pushed);
            return -1;
        }
        CUresult cures = CUDA_SUCCESS;
        if (pitch == engine->width * 4) {
            cures = engine->cuMemcpyHtoD(engine->cu_dptr, input_argb, engine->frame_bytes);
        } else {
            for (uint32_t y = 0; y < engine->height && cures == CUDA_SUCCESS; y++) {
                cures = engine->cuMemcpyHtoD(
                    engine->cu_dptr + (size_t)y * engine->width * 4,
                    input_argb + (size_t)y * pitch,
                    engine->width * 4
                );
            }
        }
        if (cures != CUDA_SUCCESS) {
            fprintf(stderr, "[nvenc] cuMemcpyHtoD failed: %d\n", cures);
            ctx_pop(engine, pushed);
            return -2;
        }
    }

    NV_ENC_MAP_INPUT_RESOURCE map_res;
    memset(&map_res, 0, sizeof(map_res));
    map_res.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map_res.registeredResource = engine->reg_res.registeredResource;
    NVENCSTATUS nvres = engine->fn.nvEncMapInputResource(engine->encoder, &map_res);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncMapInputResource failed: %d\n", nvres);
        ctx_pop(engine, pushed);
        return -3;
    }

    NV_ENC_PIC_PARAMS pic_params;
    memset(&pic_params, 0, sizeof(pic_params));
    pic_params.version = NV_ENC_PIC_PARAMS_VER;
    pic_params.inputWidth = engine->width;
    pic_params.inputHeight = engine->height;
    pic_params.inputPitch = engine->width * 4;
    pic_params.inputBuffer = map_res.mappedResource;
    pic_params.outputBitstream = engine->bitstream_buf;
    pic_params.bufferFmt = NV_ENC_BUFFER_FORMAT_ARGB;
    pic_params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    if (force_idr) {
        pic_params.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    }

    nvres = engine->fn.nvEncEncodePicture(engine->encoder, &pic_params);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncEncodePicture failed: %d\n", nvres);
        engine->fn.nvEncUnmapInputResource(engine->encoder, map_res.mappedResource);
        ctx_pop(engine, pushed);
        return -4;
    }

    NV_ENC_LOCK_BITSTREAM lock_bs;
    memset(&lock_bs, 0, sizeof(lock_bs));
    lock_bs.version = NV_ENC_LOCK_BITSTREAM_VER;
    lock_bs.outputBitstream = engine->bitstream_buf;
    lock_bs.doNotWait = 0;

    nvres = engine->fn.nvEncLockBitstream(engine->encoder, &lock_bs);
    if (nvres != NV_ENC_SUCCESS) {
        fprintf(stderr, "[nvenc] nvEncLockBitstream failed: %d\n", nvres);
        engine->fn.nvEncUnmapInputResource(engine->encoder, map_res.mappedResource);
        ctx_pop(engine, pushed);
        return -5;
    }

    int rc = 0;
    uint32_t size = lock_bs.bitstreamSizeInBytes;
    if (size > engine->output_staging_cap) {
        size_t cap = (size_t)size + 64 * 1024;
        uint8_t *grown = (uint8_t*)realloc(engine->output_staging, cap);
        if (grown) {
            engine->output_staging = grown;
            engine->output_staging_cap = cap;
        } else {
            fprintf(stderr, "[nvenc] output buffer allocation failed: %u bytes\n", size);
            rc = -6;
        }
    }
    if (rc == 0 && size > 0 && lock_bs.bitstreamBufferPtr) {
        memcpy(engine->output_staging, lock_bs.bitstreamBufferPtr, size);
        *out_data = engine->output_staging;
        *out_len = size;
    }

    engine->fn.nvEncUnlockBitstream(engine->encoder, engine->bitstream_buf);
    // The SDK allows unmapping only after the bitstream lock returned for this picture
    engine->fn.nvEncUnmapInputResource(engine->encoder, map_res.mappedResource);
    ctx_pop(engine, pushed);
    return rc;
}

void nvenc_destroy(NvencEngine* engine) {
    if (!engine) return;

    int pushed = ctx_push(engine);
    // Without a current context the NVENC/CUDA objects cannot be released safely; leak them.
    if (pushed) {
        if (engine->bitstream_buf && engine->encoder) {
            engine->fn.nvEncDestroyBitstreamBuffer(engine->encoder, engine->bitstream_buf);
            engine->bitstream_buf = NULL;
        }

        nvenc_unregister_host_buffer(engine);

        if (engine->reg_res.registeredResource && engine->encoder) {
            engine->fn.nvEncUnregisterResource(engine->encoder, engine->reg_res.registeredResource);
            memset(&engine->reg_res, 0, sizeof(engine->reg_res));
        }
        if (engine->cu_dptr) {
            engine->cuMemFree(engine->cu_dptr);
            engine->cu_dptr = 0;
        }
        if (engine->encoder) {
            engine->fn.nvEncDestroyEncoder(engine->encoder);
            engine->encoder = NULL;
        }
        ctx_pop(engine, pushed);
    }

    if (engine->cu_ctx) {
        if (engine->is_standalone_ctx) engine->cuCtxDestroy(engine->cu_ctx);
        else engine->cuDevicePrimaryCtxRelease(engine->dev);
        engine->cu_ctx = NULL;
    }

    free(engine->output_staging);
    // libcuda/libnvidia-encode stay loaded: their driver threads outlive the session, and
    // dlclose crashed them on the next encoder start (SEGV_MAPERR in libnvcuvid).
    free(engine);
}
