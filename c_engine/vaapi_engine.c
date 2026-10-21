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

// Runtime-loaded libva, like the NVENC engine's libcuda/libnvidia-encode: an inproc-av build
// must not add a hard DT_NEEDED on libva for users who never touch VAAPI.
// Signatures come from the installed headers via __typeof__ so they cannot drift.
static struct {
    void *libva, *libva_drm;
    __typeof__(vaGetDisplayDRM) *vaGetDisplayDRM;
    __typeof__(vaInitialize) *vaInitialize;
    __typeof__(vaTerminate) *vaTerminate;
    __typeof__(vaErrorStr) *vaErrorStr;
    __typeof__(vaCreateSurfaces) *vaCreateSurfaces;
    __typeof__(vaDestroySurfaces) *vaDestroySurfaces;
    __typeof__(vaCreateConfig) *vaCreateConfig;
    __typeof__(vaDestroyConfig) *vaDestroyConfig;
    __typeof__(vaCreateContext) *vaCreateContext;
    __typeof__(vaDestroyContext) *vaDestroyContext;
    __typeof__(vaCreateBuffer) *vaCreateBuffer;
    __typeof__(vaDestroyBuffer) *vaDestroyBuffer;
    __typeof__(vaMapBuffer) *vaMapBuffer;
    __typeof__(vaUnmapBuffer) *vaUnmapBuffer;
    __typeof__(vaCreateImage) *vaCreateImage;
    __typeof__(vaDestroyImage) *vaDestroyImage;
    __typeof__(vaPutImage) *vaPutImage;
    __typeof__(vaBeginPicture) *vaBeginPicture;
    __typeof__(vaRenderPicture) *vaRenderPicture;
    __typeof__(vaEndPicture) *vaEndPicture;
    __typeof__(vaSyncSurface) *vaSyncSurface;
} RT;

static int vaapi_load_runtime(void) {
    if (RT.libva) return 0;
    RT.libva = dlopen("libva.so.2", RTLD_NOW | RTLD_GLOBAL);
    RT.libva_drm = dlopen("libva-drm.so.2", RTLD_NOW);
    if (!RT.libva || !RT.libva_drm) {
        fprintf(stderr, "[vaapi] dlopen libva/libva-drm failed\n");
        return -1;
    }
    // libva-drm depends on libva, so one lookup covers both libraries
    #define VA_LOAD(sym) do { \
        *(void**)&RT.sym = dlsym(RT.libva_drm, #sym) ? dlsym(RT.libva_drm, #sym) : dlsym(RT.libva, #sym); \
        if (!RT.sym) { dlclose(RT.libva_drm); dlclose(RT.libva); RT.libva = RT.libva_drm = NULL; return -1; } \
    } while (0)
    VA_LOAD(vaGetDisplayDRM);
    VA_LOAD(vaInitialize);
    VA_LOAD(vaTerminate);
    VA_LOAD(vaErrorStr);
    VA_LOAD(vaCreateSurfaces);
    VA_LOAD(vaDestroySurfaces);
    VA_LOAD(vaCreateConfig);
    VA_LOAD(vaDestroyConfig);
    VA_LOAD(vaCreateContext);
    VA_LOAD(vaDestroyContext);
    VA_LOAD(vaCreateBuffer);
    VA_LOAD(vaDestroyBuffer);
    VA_LOAD(vaMapBuffer);
    VA_LOAD(vaUnmapBuffer);
    VA_LOAD(vaCreateImage);
    VA_LOAD(vaDestroyImage);
    VA_LOAD(vaPutImage);
    VA_LOAD(vaBeginPicture);
    VA_LOAD(vaRenderPicture);
    VA_LOAD(vaEndPicture);
    VA_LOAD(vaSyncSurface);
    #undef VA_LOAD
    return 0;
}

struct VaapiEngine {
    int drm_fd;
    VADisplay va_dpy;
    uint32_t cap_width;   // capture (RGB input) size
    uint32_t cap_height;
    uint32_t width;       // encode (output) size
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    int64_t target_bps;
    int is_hevc;
    uint32_t frame_count;
    // frame_num / POC bookkeeping since the last IDR; H264 state the driver does not derive itself
    uint32_t frame_num;
    uint32_t rc_mode;

    // DMA-BUF backing
    int dma_fd;
    uint32_t stride;

    // Surfaces & Contexts
    VASurfaceID in_rgb_surf;
    VASurfaceID nv12_surf;
    VASurfaceID recon_surfs[2];

    VAConfigID vpp_cfg;
    VAContextID vpp_ctx;
    VABufferID vpp_buf;
    VARectangle src_rect, dst_rect;

    VAConfigID enc_cfg;
    VAContextID enc_ctx;
    VABufferID coded_buf;

    uint8_t *output_staging;
    uint32_t output_staging_cap;
    // Host upload path: a VAImage the CPU writes, then vaPutImage into in_rgb_surf
    VAImage upload_img;
    int upload_ready;
};

// The VPP pipeline buffer points at the engine-owned rects, so VPP scales cap -> output
static int vaapi_make_vpp_buffer(VaapiEngine* engine) {
    VAProcPipelineParameterBuffer vpp_param;
    memset(&vpp_param, 0, sizeof(vpp_param));
    vpp_param.surface = engine->in_rgb_surf;
    vpp_param.surface_region = &engine->src_rect;
    vpp_param.output_region = &engine->dst_rect;
    return RT.vaCreateBuffer(engine->va_dpy, engine->vpp_ctx, VAProcPipelineParameterBufferType,
                             sizeof(vpp_param), 1, &vpp_param, &engine->vpp_buf) == VA_STATUS_SUCCESS ? 0 : -1;
}

VaapiEngine* vaapi_create(const VaapiConfig* config) {
    if (!config || config->width == 0 || config->height == 0 || config->cap_width == 0 || config->cap_height == 0) return NULL;
    if (config->is_hevc) return NULL; // H.264 only; the caller falls back to the FFmpeg pipeline

    if (vaapi_load_runtime() != 0) return NULL;

    VaapiEngine* engine = (VaapiEngine*)calloc(1, sizeof(VaapiEngine));
    if (!engine) return NULL;

    engine->cap_width = config->cap_width;
    engine->cap_height = config->cap_height;
    engine->width = config->width;
    engine->height = config->height;
    engine->fps = config->fps > 0 ? config->fps : 60;
    engine->gop = config->gop > 0 ? config->gop : 30;
    engine->target_bps = config->target_bps > 0 ? config->target_bps : 6000000;
    engine->is_hevc = config->is_hevc;
    engine->dma_fd = -1;
    // calloc leaves surface/context IDs at 0, which is a valid VA ID; mark them unset.
    engine->in_rgb_surf = VA_INVALID_ID;
    engine->nv12_surf = VA_INVALID_ID;
    engine->recon_surfs[0] = VA_INVALID_ID;
    engine->recon_surfs[1] = VA_INVALID_ID;
    engine->vpp_cfg = VA_INVALID_ID;
    engine->vpp_ctx = VA_INVALID_ID;
    engine->vpp_buf = VA_INVALID_ID;
    engine->enc_cfg = VA_INVALID_ID;
    engine->enc_ctx = VA_INVALID_ID;
    engine->coded_buf = VA_INVALID_ID;
    engine->src_rect.width = config->cap_width;
    engine->src_rect.height = config->cap_height;
    engine->dst_rect.width = config->width;
    engine->dst_rect.height = config->height;

    engine->drm_fd = open(config->device ? config->device : "/dev/dri/renderD128", O_RDWR);
    if (engine->drm_fd < 0) {
        fprintf(stderr, "[vaapi] failed to open %s\n", config->device ? config->device : "/dev/dri/renderD128");
        free(engine);
        return NULL;
    }

    engine->va_dpy = RT.vaGetDisplayDRM(engine->drm_fd);
    if (!engine->va_dpy) {
        close(engine->drm_fd);
        free(engine);
        return NULL;
    }

    int maj, min;
    VAStatus st = RT.vaInitialize(engine->va_dpy, &maj, &min);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaInitialize failed: %s\n", RT.vaErrorStr(st));
        close(engine->drm_fd);
        free(engine);
        return NULL;
    }

    // 1. NV12 intermediate + two Recon surfaces at the encode size (ping-pong: a P frame must
    // not reference the surface its own reconstruction is being written into)
    st = RT.vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_YUV420, engine->width, engine->height, &engine->nv12_surf, 1, NULL, 0);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }
    st = RT.vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_YUV420, engine->width, engine->height, engine->recon_surfs, 2, NULL, 0);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }

    // 2. Setup VPP (Video Processing Pipeline for Hardware RGB->NV12 with scaling)
    uint32_t vpp_w = engine->cap_width > engine->width ? engine->cap_width : engine->width;
    uint32_t vpp_h = engine->cap_height > engine->height ? engine->cap_height : engine->height;
    st = RT.vaCreateConfig(engine->va_dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &engine->vpp_cfg);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }
    st = RT.vaCreateContext(engine->va_dpy, engine->vpp_cfg, vpp_w, vpp_h, 0, NULL, 0, &engine->vpp_ctx);
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

    st = RT.vaCreateConfig(engine->va_dpy, VAProfileH264Main, VAEntrypointEncSlice, enc_attrib, 2, &engine->enc_cfg);
    if (st != VA_STATUS_SUCCESS) {
        // Fallback to CQP if CBR unsupported
        enc_attrib[1].value = VA_RC_CQP;
        st = RT.vaCreateConfig(engine->va_dpy, VAProfileH264Main, VAEntrypointEncSlice, enc_attrib, 2, &engine->enc_cfg);
        if (st != VA_STATUS_SUCCESS) {
            fprintf(stderr, "[vaapi] vaCreateConfig EncSlice failed: %s\n", RT.vaErrorStr(st));
            vaapi_destroy(engine);
            return NULL;
        }
        engine->rc_mode = VA_RC_CQP;
    } else {
        engine->rc_mode = VA_RC_CBR;
    }

    st = RT.vaCreateContext(engine->va_dpy, engine->enc_cfg, engine->width, engine->height, VA_PROGRESSIVE, engine->recon_surfs, 2, &engine->enc_ctx);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaCreateContext EncSlice failed: %s\n", RT.vaErrorStr(st));
        vaapi_destroy(engine);
        return NULL;
    }

    // Coded bitstream buffer: generous, a 20M-preset intra frame can burst past 4MB
    st = RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncCodedBufferType, 16 * 1024 * 1024, 1, NULL, &engine->coded_buf);
    if (st != VA_STATUS_SUCCESS) {
        vaapi_destroy(engine);
        return NULL;
    }

    return engine;
}

int vaapi_import_dma_buffer(VaapiEngine* engine, int dma_fd, uint32_t stride) {
    if (!engine || dma_fd < 0) return -1;

    if (engine->in_rgb_surf != VA_INVALID_ID) {
        RT.vaDestroySurfaces(engine->va_dpy, &engine->in_rgb_surf, 1);
        engine->in_rgb_surf = VA_INVALID_ID;
    }
    if (engine->vpp_buf != VA_INVALID_ID) {
        RT.vaDestroyBuffer(engine->va_dpy, engine->vpp_buf);
        engine->vpp_buf = VA_INVALID_ID;
    }

    engine->dma_fd = dma_fd;
    engine->stride = stride;

    VADRMPRIMESurfaceDescriptor prime_desc;
    memset(&prime_desc, 0, sizeof(prime_desc));
    prime_desc.fourcc = VA_FOURCC_BGRX;
    prime_desc.width = engine->cap_width;
    prime_desc.height = engine->cap_height;
    prime_desc.num_objects = 1;
    prime_desc.objects[0].fd = dma_fd;
    prime_desc.objects[0].size = stride * engine->cap_height;
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

    VAStatus st = RT.vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_RGB32, engine->cap_width, engine->cap_height, &engine->in_rgb_surf, 1, attribs, 2);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "[vaapi] vaCreateSurfaces DRM_PRIME_2 failed: %s\n", RT.vaErrorStr(st));
        return -1;
    }

    if (vaapi_make_vpp_buffer(engine) != 0) {
        fprintf(stderr, "[vaapi] vaCreateBuffer VPP failed\n");
        return -2;
    }

    return 0;
}

int vaapi_upload_host_buffer(VaapiEngine* engine, const uint8_t* host_ptr, uint32_t pitch) {
    if (!engine || !host_ptr) return -1;

    if (engine->in_rgb_surf == VA_INVALID_ID) {
        VAStatus st = RT.vaCreateSurfaces(engine->va_dpy, VA_RT_FORMAT_RGB32, engine->cap_width, engine->cap_height, &engine->in_rgb_surf, 1, NULL, 0);
        if (st != VA_STATUS_SUCCESS) return -2;
        if (vaapi_make_vpp_buffer(engine) != 0) return -3;
    }

    if (!engine->upload_ready) {
        VAImageFormat fmt;
        memset(&fmt, 0, sizeof(fmt));
        fmt.fourcc = VA_FOURCC_BGRX;
        fmt.byte_order = VA_LSB_FIRST;
        fmt.bits_per_pixel = 32;
        fmt.depth = 24;
        fmt.red_mask = 0x00ff0000;
        fmt.green_mask = 0x0000ff00;
        fmt.blue_mask = 0x000000ff;
        // vaDeriveImage on the RGB32 surface is refused by some drivers
        VAStatus st = RT.vaCreateImage(engine->va_dpy, &fmt, engine->cap_width, engine->cap_height, &engine->upload_img);
        if (st != VA_STATUS_SUCCESS) return -4;
        engine->upload_ready = 1;
    }

    void* p = NULL;
    VAStatus st = RT.vaMapBuffer(engine->va_dpy, engine->upload_img.buf, &p);
    if (st != VA_STATUS_SUCCESS || !p) return -5;
    uint32_t copy_bytes = engine->cap_width * 4;
    for (uint32_t y = 0; y < engine->cap_height; y++) {
        memcpy((uint8_t*)p + y * engine->upload_img.pitches[0], host_ptr + y * pitch, copy_bytes);
    }
    RT.vaUnmapBuffer(engine->va_dpy, engine->upload_img.buf);

    st = RT.vaPutImage(engine->va_dpy, engine->in_rgb_surf, engine->upload_img.image_id,
                       0, 0, engine->cap_width, engine->cap_height, 0, 0, engine->cap_width, engine->cap_height);
    if (st != VA_STATUS_SUCCESS) return -6;
    return 0;
}

// Sequence-level misc parameter. AMD's Mesa driver feeds the VCN ring message from these;
// a CBR config that never gets its rate-control/HRD parameters submits garbage and the
// encode job wedges the ring until a GPU reset.
static VABufferID vaapi_create_misc_buffer(VaapiEngine* engine, VAEncMiscParameterType type, const void* param, uint32_t param_size) {
    uint8_t buf[sizeof(VAEncMiscParameterBuffer) + 64];
    VAEncMiscParameterBuffer* misc = (VAEncMiscParameterBuffer*)buf;
    misc->type = type;
    memcpy(misc->data, param, param_size);
    VABufferID id = VA_INVALID_ID;
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncMiscParameterBufferType,
                          sizeof(VAEncMiscParameterBuffer) + param_size, 1, buf, &id) != VA_STATUS_SUCCESS) {
        return VA_INVALID_ID;
    }
    return id;
}

int vaapi_encode_frame(
    VaapiEngine* engine,
    int force_idr,
    const uint8_t** out_data,
    uint32_t* out_len
) {
    if (out_data) *out_data = NULL;
    if (out_len) *out_len = 0;
    if (!engine || engine->in_rgb_surf == VA_INVALID_ID) return -1;

    int is_idr = force_idr || (engine->frame_count % engine->gop == 0);
    uint32_t mb_w = (engine->width + 15) / 16;
    uint32_t mb_h = (engine->height + 15) / 16;
    uint32_t frame_num = is_idr ? 0 : engine->frame_num;
    // Recon ping-pong: a P frame must reference the previous recon surface, not its own
    VASurfaceID curr_recon = engine->recon_surfs[engine->frame_count & 1];
    VASurfaceID prev_recon = engine->recon_surfs[(engine->frame_count + 1) & 1];

    // 1. Hardware VPP Color Conversion + scaling (RGB -> NV12 entirely inside VRAM)
    VAStatus st = RT.vaBeginPicture(engine->va_dpy, engine->vpp_ctx, engine->nv12_surf);
    if (st != VA_STATUS_SUCCESS) return -10;
    st = RT.vaRenderPicture(engine->va_dpy, engine->vpp_ctx, &engine->vpp_buf, 1);
    if (st != VA_STATUS_SUCCESS) return -10;
    st = RT.vaEndPicture(engine->va_dpy, engine->vpp_ctx);
    if (st != VA_STATUS_SUCCESS) return -10;

    // 2. Prepare Sequence / Rate Control / Picture / Slice Parameters
    VABufferID seq_b = VA_INVALID_ID;
    VABufferID misc_b[3] = { VA_INVALID_ID, VA_INVALID_ID, VA_INVALID_ID };
    int misc_n = 0;
    if (is_idr) {
        VAEncSequenceParameterBufferH264 seq_p;
        memset(&seq_p, 0, sizeof(seq_p));
        seq_p.seq_parameter_set_id = 0;
        seq_p.level_idc = 51;
        seq_p.intra_period = engine->gop;
        seq_p.intra_idr_period = engine->gop;
        seq_p.ip_period = 1;
        seq_p.bits_per_second = engine->target_bps;
        seq_p.max_num_ref_frames = 1;
        seq_p.picture_width_in_mbs = mb_w;
        seq_p.picture_height_in_mbs = mb_h;
        seq_p.seq_fields.bits.direct_8x8_inference_flag = 1;
        seq_p.seq_fields.bits.frame_mbs_only_flag = 1;
        seq_p.seq_fields.bits.pic_order_cnt_type = 0;
        RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSequenceParameterBufferType, sizeof(seq_p), 1, &seq_p, &seq_b);

        VAEncMiscParameterRateControl rc;
        memset(&rc, 0, sizeof(rc));
        rc.bits_per_second = (uint32_t)engine->target_bps;
        rc.target_percentage = 100;
        rc.window_size = 1000;
        rc.initial_qp = 28;
        rc.min_qp = 16;
        rc.max_qp = 44;
        rc.rc_flags.bits.disable_frame_skip = 1; // a remote desktop must not drop frames silently
        misc_b[misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeRateControl, &rc, sizeof(rc));

        VAEncMiscParameterHRD hrd;
        memset(&hrd, 0, sizeof(hrd));
        hrd.initial_buffer_fullness = (uint32_t)engine->target_bps / 2;
        hrd.buffer_size = (uint32_t)engine->target_bps; // 1 s of HRD buffer
        misc_b[misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeHRD, &hrd, sizeof(hrd));

        // numerator in the low 16 bits, denominator in the high 16 bits
        VAEncMiscParameterFrameRate fr = { .framerate = (1u << 16) | engine->fps };
        misc_b[misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeFrameRate, &fr, sizeof(fr));
    }

    VAEncPictureParameterBufferH264 pic_p;
    memset(&pic_p, 0, sizeof(pic_p));
    pic_p.CurrPic.picture_id = curr_recon;
    pic_p.CurrPic.flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
    pic_p.CurrPic.frame_idx = frame_num;
    pic_p.CurrPic.TopFieldOrderCnt = pic_p.CurrPic.BottomFieldOrderCnt = (2 * frame_num) & 15;
    for (int i = 0; i < 16; i++) pic_p.ReferenceFrames[i].picture_id = VA_INVALID_ID;
    if (!is_idr) {
        pic_p.ReferenceFrames[0].picture_id = prev_recon;
        pic_p.ReferenceFrames[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
        pic_p.ReferenceFrames[0].frame_idx = (frame_num - 1) & 15;
        pic_p.ReferenceFrames[0].TopFieldOrderCnt = pic_p.ReferenceFrames[0].BottomFieldOrderCnt = (2 * (frame_num - 1)) & 15;
    }
    pic_p.coded_buf = engine->coded_buf;
    pic_p.frame_num = frame_num;
    pic_p.pic_init_qp = 26;
    pic_p.seq_parameter_set_id = 0;
    pic_p.pic_parameter_set_id = 0;
    pic_p.pic_fields.bits.idr_pic_flag = is_idr ? 1 : 0;
    pic_p.pic_fields.bits.reference_pic_flag = 1;
    VABufferID pic_b = VA_INVALID_ID;
    RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPictureParameterBufferType, sizeof(pic_p), 1, &pic_p, &pic_b);

    VAEncSliceParameterBufferH264 slice_p;
    memset(&slice_p, 0, sizeof(slice_p));
    slice_p.macroblock_address = 0;
    slice_p.num_macroblocks = mb_w * mb_h;
    slice_p.macroblock_info = VA_INVALID_ID;
    slice_p.slice_type = is_idr ? 2 : 0; // 2 = I-Slice, 0 = P-Slice
    slice_p.direct_spatial_mv_pred_flag = 1;
    slice_p.idr_pic_id = (engine->frame_count / engine->gop) & 1;
    for (int i = 0; i < 32; i++) slice_p.RefPicList0[i].picture_id = VA_INVALID_ID;
    if (!is_idr) {
        slice_p.RefPicList0[0].picture_id = prev_recon;
        slice_p.RefPicList0[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
        slice_p.RefPicList0[0].frame_idx = (frame_num - 1) & 15;
    }
    VABufferID slice_b = VA_INVALID_ID;
    RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSliceParameterBufferType, sizeof(slice_p), 1, &slice_p, &slice_b);

    // 3. Render and Encode on AMD VCN hardware
    VABufferID render_b[6];
    int render_n = 0;
    for (int i = 0; i < misc_n; i++) {
        if (misc_b[i] != VA_INVALID_ID) render_b[render_n++] = misc_b[i];
    }
    if (seq_b != VA_INVALID_ID) render_b[render_n++] = seq_b;
    if (pic_b != VA_INVALID_ID) render_b[render_n++] = pic_b;
    if (slice_b != VA_INVALID_ID) render_b[render_n++] = slice_b;
    if (render_n < 3) goto fail;

    st = RT.vaBeginPicture(engine->va_dpy, engine->enc_ctx, engine->nv12_surf);
    if (st != VA_STATUS_SUCCESS) goto fail;
    st = RT.vaRenderPicture(engine->va_dpy, engine->enc_ctx, render_b, render_n);
    if (st != VA_STATUS_SUCCESS) goto fail;
    st = RT.vaEndPicture(engine->va_dpy, engine->enc_ctx);
    if (st != VA_STATUS_SUCCESS) goto fail;
    st = RT.vaSyncSurface(engine->va_dpy, curr_recon);
    if (st != VA_STATUS_SUCCESS) goto fail;

    // 4. Map Bitstream Output; an access unit may arrive as several linked segments
    VACodedBufferSegment *segment = NULL;
    st = RT.vaMapBuffer(engine->va_dpy, engine->coded_buf, (void**)&segment);
    if (st != VA_STATUS_SUCCESS || !segment) goto fail;
    uint32_t total = 0;
    for (VACodedBufferSegment *seg = segment; seg; seg = seg->next) total += seg->size;
    if (total > 0 && !(segment->status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK)) {
        if (total > engine->output_staging_cap) {
            engine->output_staging_cap = total + 64 * 1024;
            engine->output_staging = (uint8_t*)realloc(engine->output_staging, engine->output_staging_cap);
        }
        uint32_t off = 0;
        for (VACodedBufferSegment *seg = segment; seg; seg = seg->next) {
            memcpy(engine->output_staging + off, seg->buf, seg->size);
            off += seg->size;
        }
        *out_data = engine->output_staging;
        *out_len = total;
    }
    RT.vaUnmapBuffer(engine->va_dpy, engine->coded_buf);

    for (int i = 0; i < misc_n; i++) {
        if (misc_b[i] != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, misc_b[i]);
    }
    if (seq_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, seq_b);
    if (pic_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, pic_b);
    if (slice_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, slice_b);

    engine->frame_num = is_idr ? 1 : ((engine->frame_num + 1) & 15);
    engine->frame_count++;
    return 0;

fail:
    for (int i = 0; i < misc_n; i++) {
        if (misc_b[i] != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, misc_b[i]);
    }
    if (seq_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, seq_b);
    if (pic_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, pic_b);
    if (slice_b != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, slice_b);
    return -11;
}

void vaapi_destroy(VaapiEngine* engine) {
    if (!engine) return;

    if (engine->coded_buf != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, engine->coded_buf);
    if (engine->enc_ctx != VA_INVALID_ID) RT.vaDestroyContext(engine->va_dpy, engine->enc_ctx);
    if (engine->enc_cfg != VA_INVALID_ID) RT.vaDestroyConfig(engine->va_dpy, engine->enc_cfg);

    if (engine->vpp_buf != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, engine->vpp_buf);
    if (engine->vpp_ctx != VA_INVALID_ID) RT.vaDestroyContext(engine->va_dpy, engine->vpp_ctx);
    if (engine->vpp_cfg != VA_INVALID_ID) RT.vaDestroyConfig(engine->va_dpy, engine->vpp_cfg);

    if (engine->upload_ready) RT.vaDestroyImage(engine->va_dpy, engine->upload_img.image_id);
    if (engine->in_rgb_surf != VA_INVALID_ID) RT.vaDestroySurfaces(engine->va_dpy, &engine->in_rgb_surf, 1);
    if (engine->nv12_surf != VA_INVALID_ID) RT.vaDestroySurfaces(engine->va_dpy, &engine->nv12_surf, 1);
    if (engine->recon_surfs[0] != VA_INVALID_ID || engine->recon_surfs[1] != VA_INVALID_ID) {
        RT.vaDestroySurfaces(engine->va_dpy, engine->recon_surfs, 2);
    }

    if (engine->va_dpy) RT.vaTerminate(engine->va_dpy);
    if (engine->drm_fd >= 0) close(engine->drm_fd);
    if (engine->output_staging) free(engine->output_staging);

    free(engine);
}
