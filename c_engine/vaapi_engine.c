#include "vaapi_engine.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <dlfcn.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>
#include <va/va_enc_h264.h>

struct VaapiEngine {
    int drm_fd;
    VADisplay va_dpy;
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    int64_t target_bps;
    int is_hevc;
    uint32_t frame_count;

    // DMA-BUF backing
    int dma_fd;
    uint32_t stride;

    // Surfaces & Contexts
    VASurfaceID in_rgb_surf;
    VASurfaceID nv12_surf;
    VASurfaceID recon_surf;

    VAConfigID vpp_cfg;
    VAContextID vpp_ctx;
    VABufferID vpp_buf;

    VAConfigID enc_cfg;
    VAContextID enc_ctx;
    VABufferID coded_buf;

    uint8_t *output_staging;
    uint32_t output_staging_cap;
};

VaapiEngine* vaapi_create(const VaapiConfig* config) {
    if (!config || config->width == 0 || config->height == 0) return NULL;

    VaapiEngine* engine = (VaapiEngine*)calloc(1, sizeof(VaapiEngine));
    if (!engine) return NULL;

    engine->width = config->width;
    engine->height = config->height;
    engine->fps = config->fps > 0 ? config->fps : 60;
    engine->gop = config->gop > 0 ? config->gop : 30;
    engine->target_bps = config->target_bps > 0 ? config->target_bps : 6000000;
    engine->is_hevc = config->is_hevc;
    engine->dma_fd = -1;

    engine->drm_fd = open("/dev/dri/renderD128", O_RDWR);
    if (engine->drm_fd < 0) {
        fprintf(stderr, "[vaapi] failed to open /dev/dri/renderD128\n");
        free(engine);
        return NULL;
    }

    engine->va_dpy = vaGetDisplayDRM(engine->drm_fd);
    if (!engine->va_dpy) {
        close(engine->drm_fd);
        free(engine);
        return NULL;
    }

    int maj, min;
    VAStatus st = vaInitialize(engine->va_dpy, &maj, &min);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaInitialize failed: %s\n", vaErrorStr(st));
        close(engine->drm_fd);
        free(engine);
        return NULL;
    }

    // 1. Create NV12 Intermediate surface & Recon surface
    st = vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_YUV420, engine->width, engine->height, &engine->nv12_surf, 1, NULL, 0);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }
    st = vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_YUV420, engine->width, engine->height, &engine->recon_surf, 1, NULL, 0);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }

    // 2. Setup VPP (Video Processing Pipeline for Hardware RGB->NV12)
    st = vaCreateConfig(engine->va_dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &engine->vpp_cfg);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }
    st = vaCreateContext(engine->va_dpy, engine->vpp_cfg, engine->width, engine->height, 0, NULL, 0, &engine->vpp_ctx);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }

    // 3. Setup H264 EncSlice
    VAConfigAttrib enc_attrib[2];
    enc_attrib[0].type = VAConfigAttribRTFormat;
    enc_attrib[0].value = VA_RT_FORMAT_YUV420;
    enc_attrib[1].type = VAConfigAttribRateControl;
    enc_attrib[1].value = VA_RC_CBR;

    st = vaCreateConfig(engine->va_dpy, VAProfileH264Main, VAEntrypointEncSlice, enc_attrib, 2, &engine->enc_cfg);
    if (st != VA_STATUS_SUCCESS) {
        // Fallback to CQP if CBR unsupported
        enc_attrib[1].value = VA_RC_CQP;
        st = vaCreateConfig(engine->va_dpy, VAProfileH264Main, VAEntrypointEncSlice, enc_attrib, 2, &engine->enc_cfg);
        if (st != VA_STATUS_SUCCESS) {
            fprintf(stderr, "[vaapi] vaCreateConfig EncSlice failed: %s\n", vaErrorStr(st));
            vaapi_destroy(engine);
            return NULL;
        }
    }

    st = vaCreateContext(engine->va_dpy, engine->enc_cfg, engine->width, engine->height, VA_PROGRESSIVE, &engine->recon_surf, 1, &engine->enc_ctx);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaCreateContext EncSlice failed: %s\n", vaErrorStr(st));
        vaapi_destroy(engine);
        return NULL;
    }

    // Allocate 4MB bitstream buffer
    st = vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncCodedBufferType, 4 * 1024 * 1024, 1, NULL, &engine->coded_buf);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }

    return engine;
}

int vaapi_import_dma_buffer(VaapiEngine* engine, int dma_fd, uint32_t stride) {
    if (!engine || dma_fd < 0) return -1;

    if (engine->in_rgb_surf != VA_INVALID_ID) {
        vaDestroySurfaces(engine->va_dpy, &engine->in_rgb_surf, 1);
        engine->in_rgb_surf = VA_INVALID_ID;
    }
    if (engine->vpp_buf != VA_INVALID_ID) {
        vaDestroyBuffer(engine->va_dpy, engine->vpp_buf);
        engine->vpp_buf = VA_INVALID_ID;
    }

    engine->dma_fd = dma_fd;
    engine->stride = stride;

    VADRMPRIMESurfaceDescriptor prime_desc;
    memset(&prime_desc, 0, sizeof(prime_desc));
    prime_desc.fourcc = VA_FOURCC_BGRX;
    prime_desc.width = engine->width;
    prime_desc.height = engine->height;
    prime_desc.num_objects = 1;
    prime_desc.objects[0].fd = dma_fd;
    prime_desc.objects[0].size = stride * engine->height;
    prime_desc.objects[0].drm_format_modifier = 0;
    prime_desc.num_layers = 1;
    prime_desc.layers[0].drm_format = 0x34325258; // DRM_FORMAT_XRGB8888
    prime_desc.layers[0].num_planes = 1;
    prime_desc.layers[0].pitch[0] = stride;

    VASurfaceAttrib attribs[2];
    attribs[0].type = VASurfaceAttribMemoryType;
    attribs[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attribs[0].value.type = VAGenericValueTypeInteger;
    attribs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attribs[1].type = VASurfaceAttribExternalBufferDescriptor;
    attribs[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attribs[1].value.type = VAGenericValueTypePointer;
    attribs[1].value.value.p = &prime_desc;

    VAStatus st = vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_RGB32, engine->width, engine->height, &engine->in_rgb_surf, 1, attribs, 2);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaCreateSurfaces DRM_PRIME_2 failed: %s\n", vaErrorStr(st));
        return -1;
    }

    VAProcPipelineParameterBuffer vpp_param;
    memset(&vpp_param, 0, sizeof(vpp_param));
    vpp_param.surface = engine->in_rgb_surf;
    st = vaCreateBuffer(engine->va_dpy, engine->vpp_ctx, VAProcPipelineParameterBufferType, sizeof(vpp_param), 1, &vpp_param, &engine->vpp_buf);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaCreateBuffer VPP failed: %s\n", vaErrorStr(st));
        return -2;
    }

    return 0;
}

int vaapi_upload_host_buffer(VaapiEngine* engine, const uint8_t* host_ptr, uint32_t pitch) {
    if (!engine || !host_ptr) return -1;

    if (engine->in_rgb_surf == VA_INVALID_ID) {
        VAStatus st = vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_RGB32, engine->width, engine->height, &engine->in_rgb_surf, 1, NULL, 0);
        if (st != VA_STATUS_SUCCESS) return -2;

        VAProcPipelineParameterBuffer vpp_param;
        memset(&vpp_param, 0, sizeof(vpp_param));
        vpp_param.surface = engine->in_rgb_surf;
        st = vaCreateBuffer(engine->va_dpy, engine->vpp_ctx, VAProcPipelineParameterBufferType, sizeof(vpp_param), 1, &vpp_param, &engine->vpp_buf);
        if (st != VA_STATUS_SUCCESS) return -3;
    }

    VAImage img;
    VAStatus st = vaDeriveImage(engine->va_dpy, engine->in_rgb_surf, &img);
    if (st == VA_STATUS_SUCCESS) {
        void* p = NULL;
        vaMapBuffer(engine->va_dpy, img.buf, &p);
        if (p) {
            uint32_t copy_lines = engine->height;
            uint32_t copy_bytes = engine->width * 4;
            for (uint32_t y = 0; y < copy_lines; y++) {
                memcpy((uint8_t*)p + y * img.pitches[0], host_ptr + y * pitch, copy_bytes);
            }
            vaUnmapBuffer(engine->va_dpy, img.buf);
        }
        vaDestroyImage(engine->va_dpy, img.image_id);
    }
    return 0;
}

int vaapi_encode_frame(
    VaapiEngine* engine,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
) {
    if (!engine || engine->in_rgb_surf == VA_INVALID_ID) {
        if (out_data) *out_data = NULL;
        if (out_len) *out_len = 0;
        return -1;
    }

    int is_idr = force_idr || (engine->frame_count % engine->gop == 0);
    uint32_t mb_w = (engine->width + 15) / 16;
    uint32_t mb_h = (engine->height + 15) / 16;

    // 1. Hardware VPP Color Conversion (RGB -> NV12 entirely inside VRAM)
    vaBeginPicture(engine->va_dpy, engine->vpp_ctx, engine->nv12_surf);
    vaRenderPicture(engine->va_dpy, engine->vpp_ctx, &engine->vpp_buf, 1);
    vaEndPicture(engine->va_dpy, engine->vpp_ctx);

    // 2. Prepare Sequence / Picture / Slice Parameters
    VABufferID seq_b = VA_INVALID_ID;
    if (is_idr) {
        VAEncSequenceParameterBufferH264 seq_p;
        memset(&seq_p, 0, sizeof(seq_p));
        seq_p.level_idc = 51;
        seq_p.intra_period = engine->gop;
        seq_p.intra_idr_period = engine->gop;
        seq_p.ip_period = 1;
        seq_p.bits_per_second = engine->target_bps;
        seq_p.max_num_ref_frames = 1;
        seq_p.picture_width_in_mbs = mb_w;
        seq_p.picture_height_in_mbs = mb_h;
        vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSequenceParameterBufferType, sizeof(seq_p), 1, &seq_p, &seq_b);
    }

    VAEncPictureParameterBufferH264 pic_p;
    memset(&pic_p, 0, sizeof(pic_p));
    pic_p.CurrPic.picture_id = engine->recon_surf;
    for (int i = 0; i < 16; i++) pic_p.ReferenceFrames[i].picture_id = VA_INVALID_ID;
    pic_p.coded_buf = engine->coded_buf;
    pic_p.pic_fields.bits.idr_pic_flag = is_idr ? 1 : 0;
    pic_p.pic_fields.bits.reference_pic_flag = 1;
    VABufferID pic_b = VA_INVALID_ID;
    vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPictureParameterBufferType, sizeof(pic_p), 1, &pic_p, &pic_b);

    VAEncSliceParameterBufferH264 slice_p;
    memset(&slice_p, 0, sizeof(slice_p));
    slice_p.macroblock_address = 0;
    slice_p.num_macroblocks = mb_w * mb_h;
    slice_p.macroblock_info = VA_INVALID_ID;
    slice_p.slice_type = is_idr ? 2 : 0; // 2 = I-Slice, 0 = P-Slice
    slice_p.idr_pic_id = is_idr ? (engine->frame_count / engine->gop) : 0;
    VABufferID slice_b = VA_INVALID_ID;
    vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSliceParameterBufferType, sizeof(slice_p), 1, &slice_p, &slice_b);

    // 3. Render and Encode on AMD VCN hardware
    vaBeginPicture(engine->va_dpy, engine->enc_ctx, engine->nv12_surf);
    if (is_idr && seq_b != VA_INVALID_ID) {
        vaRenderPicture(engine->va_dpy, engine->enc_ctx, &seq_b, 1);
    }
    vaRenderPicture(engine->va_dpy, engine->enc_ctx, &pic_b, 1);
    vaRenderPicture(engine->va_dpy, engine->enc_ctx, &slice_b, 1);
    vaEndPicture(engine->va_dpy, engine->enc_ctx);
    vaSyncSurface(engine->va_dpy, engine->nv12_surf);

    // 4. Map Bitstream Output
    VACodedBufferSegment *segment = NULL;
    vaMapBuffer(engine->va_dpy, engine->coded_buf, (void**)&segment);
    if (segment && segment->size > 0) {
        if (segment->size > engine->output_staging_cap) {
            engine->output_staging_cap = segment->size + 64 * 1024;
            engine->output_staging = (uint8_t*)realloc(engine->output_staging, engine->output_staging_cap);
        }
        memcpy(engine->output_staging, segment->buf, segment->size);
        *out_data = engine->output_staging;
        *out_len = segment->size;
    } else {
        *out_data = NULL;
        *out_len = 0;
    }
    vaUnmapBuffer(engine->va_dpy, engine->coded_buf);

    if (seq_b != VA_INVALID_ID) vaDestroyBuffer(engine->va_dpy, seq_b);
    if (pic_b != VA_INVALID_ID) vaDestroyBuffer(engine->va_dpy, pic_b);
    if (slice_b != VA_INVALID_ID) vaDestroyBuffer(engine->va_dpy, slice_b);

    engine->frame_count++;
    return 0;
}

void vaapi_destroy(VaapiEngine* engine) {
    if (!engine) return;

    if (engine->coded_buf != VA_INVALID_ID) vaDestroyBuffer(engine->va_dpy, engine->coded_buf);
    if (engine->enc_ctx != VA_INVALID_ID) vaDestroyContext(engine->va_dpy, engine->enc_ctx);
    if (engine->enc_cfg != VA_INVALID_ID) vaDestroyConfig(engine->va_dpy, engine->enc_cfg);

    if (engine->vpp_buf != VA_INVALID_ID) vaDestroyBuffer(engine->va_dpy, engine->vpp_buf);
    if (engine->vpp_ctx != VA_INVALID_ID) vaDestroyContext(engine->va_dpy, engine->vpp_ctx);
    if (engine->vpp_cfg != VA_INVALID_ID) vaDestroyConfig(engine->va_dpy, engine->vpp_cfg);

    if (engine->in_rgb_surf != VA_INVALID_ID) vaDestroySurfaces(engine->va_dpy, &engine->in_rgb_surf, 1);
    if (engine->nv12_surf != VA_INVALID_ID) vaDestroySurfaces(engine->va_dpy, &engine->nv12_surf, 1);
    if (engine->recon_surf != VA_INVALID_ID) vaDestroySurfaces(engine->va_dpy, &engine->recon_surf, 1);

    if (engine->va_dpy) vaTerminate(engine->va_dpy);
    if (engine->drm_fd >= 0) close(engine->drm_fd);
    if (engine->output_staging) free(engine->output_staging);

    free(engine);
}
