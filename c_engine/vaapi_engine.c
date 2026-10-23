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
    int64_t max_bps;
    int64_t buffer_bits;
    int is_hevc;
    uint32_t frame_count;
    // frames since the last IDR (periodic GOP), the alternating idr_pic_id, and an IDR owed after a lost frame
    uint32_t since_idr;
    uint32_t idr_pic_id;
    int idr_pending;
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
    engine->max_bps = config->max_bps > engine->target_bps ? config->max_bps : engine->target_bps;
    engine->buffer_bits = config->buffer_bits > 0 ? config->buffer_bits : engine->max_bps;
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

    // 3. Setup H.264 / HEVC EncSlice
    VAProfile profile = engine->is_hevc ? VAProfileHEVCMain : VAProfileH264Main;
    VAConfigAttrib enc_attrib[3];
    enc_attrib[0].type = VAConfigAttribRTFormat;
    enc_attrib[0].value = VA_RT_FORMAT_YUV420;
    enc_attrib[1].type = VAConfigAttribRateControl;
    // The engine writes SPS, PPS and slice headers itself (see BitWriter)
    enc_attrib[2].type = VAConfigAttribEncPackedHeaders;
    enc_attrib[2].value = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE;
    // VBR like the FFmpeg VAAPI pipeline (target + maxrate), then CBR, then CQP
    enc_attrib[1].value = VA_RC_VBR;
    engine->rc_mode = VA_RC_VBR;
    st = RT.vaCreateConfig(engine->va_dpy, profile, VAEntrypointEncSlice, enc_attrib, 3, &engine->enc_cfg);
    if (st != VA_STATUS_SUCCESS) {
        enc_attrib[1].value = VA_RC_CBR;
        engine->rc_mode = VA_RC_CBR;
        st = RT.vaCreateConfig(engine->va_dpy, profile, VAEntrypointEncSlice, enc_attrib, 3, &engine->enc_cfg);
    }
    if (st != VA_STATUS_SUCCESS) {
        // Fallback to CQP if CBR unsupported
        enc_attrib[1].value = VA_RC_CQP;
        st = RT.vaCreateConfig(engine->va_dpy, profile, VAEntrypointEncSlice, enc_attrib, 3, &engine->enc_cfg);
        if (st != VA_STATUS_SUCCESS) {
            fprintf(stderr, "[vaapi] vaCreateConfig EncSlice failed: %s\n", RT.vaErrorStr(st));
            vaapi_destroy(engine);
            return NULL;
        }
        engine->rc_mode = VA_RC_CQP;
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
// Packed H.264 / HEVC headers. Current radeonsi (Mesa 26) writes neither SPS/PPS nor the NAL/slice header from the
// parameter buffers alone (output began 00 00 00 01 00 ..., no SPS), so the host writes them like FFmpeg does.
// Every field here must match the sequence/picture/slice parameters submitted with it.
typedef struct { uint8_t buf[128]; uint32_t bits; } BitWriter;

static void bw_put(BitWriter* w, uint32_t value, int n) {
    for (int i = n - 1; i >= 0; i--, w->bits++) {
        if (w->bits / 8 >= sizeof(w->buf)) return;
        if ((value >> i) & 1) w->buf[w->bits / 8] |= 0x80 >> (w->bits % 8);
    }
}
static void bw_ue(BitWriter* w, uint32_t value) {
    int len = 0;
    for (uint32_t v = value + 1; v > 1; v >>= 1) len++;
    bw_put(w, 0, len);
    bw_put(w, value + 1, len + 1);
}
static void bw_se(BitWriter* w, int32_t value) { bw_ue(w, value > 0 ? 2 * (uint32_t)value - 1 : 2 * (uint32_t)(-value)); }
static void bw_trailing(BitWriter* w) { bw_put(w, 1, 1); if (w->bits % 8) bw_put(w, 0, 8 - w->bits % 8); }
// Start code + NAL header; the payload follows unescaped, emulation prevention is applied by bw_escape.
static void bw_nal(BitWriter* w, int ref_idc, int type) { bw_put(w, 1, 32); bw_put(w, (ref_idc << 5) | type, 8); }

// Insert emulation prevention bytes after the start code + NAL header (byte 5 on). Returns the byte length.
static uint32_t bw_escape(const BitWriter* w, uint8_t* out, uint32_t cap) {
    uint32_t len = (w->bits + 7) / 8, n = 0, zeros = 0;
    for (uint32_t i = 0; i < len && n < cap; i++) {
        if (i >= 5 && zeros >= 2 && w->buf[i] <= 3) { out[n++] = 3; zeros = 0; if (n >= cap) break; }
        zeros = w->buf[i] == 0 && i >= 5 ? zeros + 1 : 0;
        out[n++] = w->buf[i];
    }
    return n;
}

// Each NAL is escaped on its own, so the next NAL's start code stays intact.
static int vaapi_add_packed(VaapiEngine* engine, VAEncPackedHeaderType type, const BitWriter* nals, int count,
                            VABufferID* ids, int* n) {
    uint8_t data[320];
    uint32_t bytes = 0;
    for (int i = 0; i < count; i++) bytes += bw_escape(&nals[i], data + bytes, sizeof(data) - bytes);
    VAEncPackedHeaderParameterBuffer param = { .type = type, .bit_length = bytes * 8, .has_emulation_bytes = 1 };
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPackedHeaderParameterBufferType, sizeof(param), 1, &param, &ids[*n]) != VA_STATUS_SUCCESS)
        return -1;
    (*n)++;
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPackedHeaderDataBufferType, bytes, 1, data, &ids[*n]) != VA_STATUS_SUCCESS)
        return -1;
    (*n)++;
    return 0;
}

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

// Buffers for one encoded picture; every created ID is destroyed after the picture, success or not.
typedef struct {
    VABufferID seq, misc[3], pic, slice, packed[6];
    int misc_n, packed_n;
} EncBuffers;

static int vaapi_h264_params(VaapiEngine* engine, int is_idr, VASurfaceID curr_recon, VASurfaceID prev_recon, EncBuffers* b) {
    uint32_t mb_w = (engine->width + 15) / 16;
    uint32_t mb_h = (engine->height + 15) / 16;
    uint32_t frame_num = is_idr ? 0 : engine->frame_num;
    uint32_t crop_right = (mb_w * 16 - engine->width) / 2, crop_bottom = (mb_h * 16 - engine->height) / 2;
    if (is_idr) {
        VAEncSequenceParameterBufferH264 seq_p;
        memset(&seq_p, 0, sizeof(seq_p));
        seq_p.seq_parameter_set_id = 0;
        seq_p.level_idc = 51;
        seq_p.intra_period = engine->gop;
        seq_p.intra_idr_period = engine->gop;
        seq_p.ip_period = 1;
        seq_p.bits_per_second = (uint32_t)engine->max_bps;
        seq_p.max_num_ref_frames = 1;
        seq_p.picture_width_in_mbs = mb_w;
        seq_p.picture_height_in_mbs = mb_h;
        seq_p.seq_fields.bits.direct_8x8_inference_flag = 1;
        seq_p.seq_fields.bits.frame_mbs_only_flag = 1;
        seq_p.seq_fields.bits.pic_order_cnt_type = 0;
        seq_p.frame_cropping_flag = crop_right || crop_bottom;
        seq_p.frame_crop_right_offset = crop_right;
        seq_p.frame_crop_bottom_offset = crop_bottom;
        // An IDR without its SPS is undecodable: fail rather than submit pic + slice alone
        if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSequenceParameterBufferType, sizeof(seq_p), 1, &seq_p, &b->seq) != VA_STATUS_SUCCESS)
            return -1;

        // SPS: Main, level 5.1, frame_num and POC lsb 4 bits (both wrap & 15 above), one reference
        BitWriter sps = {0};
        bw_nal(&sps, 3, 7);
        bw_put(&sps, 77, 8); bw_put(&sps, 0x40, 8); bw_put(&sps, 51, 8); // constraint_set1: Main
        bw_ue(&sps, 0); bw_ue(&sps, 0); bw_ue(&sps, 0); bw_ue(&sps, 0); // sps id, log2_max_frame_num-4, poc type 0, log2_max_poc_lsb-4
        bw_ue(&sps, 1); bw_put(&sps, 0, 1);                               // max_num_ref_frames, gaps
        bw_ue(&sps, mb_w - 1); bw_ue(&sps, mb_h - 1);
        bw_put(&sps, 1, 1); bw_put(&sps, 1, 1);                           // frame_mbs_only, direct_8x8_inference
        bw_put(&sps, seq_p.frame_cropping_flag, 1);
        if (seq_p.frame_cropping_flag) { bw_ue(&sps, 0); bw_ue(&sps, crop_right); bw_ue(&sps, 0); bw_ue(&sps, crop_bottom); }
        bw_put(&sps, 0, 1);                                               // no VUI
        bw_trailing(&sps);
        // PPS: CABAC, one L0 reference, QP 26, default deblocking
        BitWriter pps = {0};
        bw_nal(&pps, 3, 8);
        bw_ue(&pps, 0); bw_ue(&pps, 0); bw_put(&pps, 1, 1); bw_put(&pps, 0, 1);
        bw_ue(&pps, 0); bw_ue(&pps, 0); bw_ue(&pps, 0);                    // slice groups, num_ref_idx l0/l1 - 1
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 2);                           // weighted pred / bipred
        bw_se(&pps, 0); bw_se(&pps, 0); bw_se(&pps, 0);                    // pic_init_qp-26, qs, chroma offset
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 1); bw_put(&pps, 0, 1);        // deblocking control, constrained intra, redundant
        bw_trailing(&pps);
        // SPS and PPS go out as one Sequence header, like FFmpeg
        BitWriter headers[2] = { sps, pps };
        if (vaapi_add_packed(engine, VAEncPackedHeaderSequence, headers, 2, b->packed, &b->packed_n) != 0) return -1;
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
    pic_p.pic_fields.bits.entropy_coding_mode_flag = 1;
    pic_p.pic_fields.bits.deblocking_filter_control_present_flag = 0;
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPictureParameterBufferType, sizeof(pic_p), 1, &pic_p, &b->pic) != VA_STATUS_SUCCESS)
        return -1;

    VAEncSliceParameterBufferH264 slice_p;
    memset(&slice_p, 0, sizeof(slice_p));
    slice_p.macroblock_address = 0;
    slice_p.num_macroblocks = mb_w * mb_h;
    slice_p.macroblock_info = VA_INVALID_ID;
    slice_p.slice_type = is_idr ? 2 : 0; // 2 = I-Slice, 0 = P-Slice
    slice_p.direct_spatial_mv_pred_flag = 1;
    // Consecutive IDRs must differ in idr_pic_id (7.4.3); flip it on every IDR
    if (is_idr) engine->idr_pic_id ^= 1;
    slice_p.idr_pic_id = engine->idr_pic_id;
    slice_p.pic_order_cnt_lsb = pic_p.CurrPic.TopFieldOrderCnt;
    for (int i = 0; i < 32; i++) slice_p.RefPicList0[i].picture_id = VA_INVALID_ID;
    if (!is_idr) {
        slice_p.RefPicList0[0].picture_id = prev_recon;
        slice_p.RefPicList0[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
        slice_p.RefPicList0[0].frame_idx = (frame_num - 1) & 15;
    }
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSliceParameterBufferType, sizeof(slice_p), 1, &slice_p, &b->slice) != VA_STATUS_SUCCESS)
        return -1;

    // Slice header including cabac_alignment_one_bit, byte aligned and escaped as FFmpeg submits it
    BitWriter sh = {0};
    bw_nal(&sh, 3, is_idr ? 5 : 1);
    bw_ue(&sh, 0); bw_ue(&sh, is_idr ? 7 : 5); bw_ue(&sh, 0);           // first_mb, slice_type I/P (all slices), pps id
    bw_put(&sh, frame_num & 15, 4);
    if (is_idr) bw_ue(&sh, slice_p.idr_pic_id);
    bw_put(&sh, slice_p.pic_order_cnt_lsb & 15, 4);
    if (!is_idr) { bw_put(&sh, 0, 1); bw_put(&sh, 0, 1); }              // num_ref_idx override, ref list modification
    if (is_idr) { bw_put(&sh, 0, 1); bw_put(&sh, 0, 1); }               // no_output_of_prior_pics, long_term_reference
    else bw_put(&sh, 0, 1);                                             // adaptive_ref_pic_marking
    if (!is_idr) bw_ue(&sh, 0);                                         // cabac_init_idc
    bw_se(&sh, 0);                                                      // slice_qp_delta (no deblocking fields)
    while (sh.bits % 8) bw_put(&sh, 1, 1);
    return vaapi_add_packed(engine, VAEncPackedHeaderSlice, &sh, 1, b->packed, &b->packed_n);
}

// HEVC NAL header: forbidden 0, nal_unit_type, nuh_layer_id 0, nuh_temporal_id_plus1 1.
static void bw_hevc_nal(BitWriter* w, int type) { bw_put(w, 1, 32); bw_put(w, (type << 9) | 1, 16); }

// profile_tier_level(1, 0): Main, Main 10 compatible like FFmpeg, progressive frames only.
static void bw_hevc_ptl(BitWriter* w, uint32_t level) {
    bw_put(w, 0, 2); bw_put(w, 0, 1); bw_put(w, 1, 5);   // profile space, tier, general_profile_idc Main
    bw_put(w, 0x60000000, 32);                         // compatibility flags 1 and 2
    bw_put(w, 1, 1); bw_put(w, 0, 1); bw_put(w, 1, 1); bw_put(w, 1, 1); // progressive, interlaced, non_packed, frame_only
    bw_put(w, 0, 32); bw_put(w, 0, 12);                // reserved_zero_43bits + inbld_flag
    bw_put(w, level, 8);
}

// HEVC Main with FFmpeg's hevc_vaapi parameters on radeonsi (LIBVA_TRACE + trace_headers of its output):
// 64x64 CTB, 8x8 min CB, 4x4..32x32 TB, AMP and SAO on, one short-term reference, POC lsb 12 bits.
// FFmpeg also sends VA max_transform_hierarchy_depth 0 / diff_cu_qp_delta_depth 3 while its SPS/PPS
// say 4 / 0; both copies are kept as FFmpeg has them, since that is what decodes on this driver.
static int vaapi_hevc_params(VaapiEngine* engine, int is_idr, VASurfaceID curr_recon, VASurfaceID prev_recon, EncBuffers* b) {
    // pic size must be a multiple of the 8x8 min CB; the conformance window (in chroma units) crops the rest
    uint32_t w = (engine->width + 7) & ~7u, h = (engine->height + 7) & ~7u;
    uint32_t crop_right = (w - engine->width) / 2, crop_bottom = (h - engine->height) / 2;
    uint32_t poc = is_idr ? 0 : engine->since_idr;
    // Level from luma picture size and sample rate (Table A.8): 4.1, 5.0, 5.1, 5.2
    uint64_t size = (uint64_t)w * h, rate = size * engine->fps;
    uint32_t level = rate > 534773760 ? 156 : rate > 267386880 ? 153 : (size > 2228224 || rate > 133693440) ? 150 : 123;
    if (is_idr) {
        VAEncSequenceParameterBufferHEVC seq_p;
        memset(&seq_p, 0, sizeof(seq_p));
        seq_p.general_profile_idc = 1;
        seq_p.general_level_idc = level;
        seq_p.intra_period = engine->gop;
        seq_p.intra_idr_period = engine->gop;
        seq_p.ip_period = 1;
        seq_p.bits_per_second = (uint32_t)engine->max_bps;
        seq_p.pic_width_in_luma_samples = w;
        seq_p.pic_height_in_luma_samples = h;
        seq_p.seq_fields.bits.chroma_format_idc = 1;
        seq_p.seq_fields.bits.amp_enabled_flag = 1;
        seq_p.seq_fields.bits.sample_adaptive_offset_enabled_flag = 1;
        seq_p.log2_diff_max_min_luma_coding_block_size = 3;
        seq_p.log2_diff_max_min_transform_block_size = 3;
        if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSequenceParameterBufferType, sizeof(seq_p), 1, &seq_p, &b->seq) != VA_STATUS_SUCCESS)
            return -1;

        BitWriter vps = {0};
        bw_hevc_nal(&vps, 32);
        bw_put(&vps, 0, 4); bw_put(&vps, 1, 1); bw_put(&vps, 1, 1);      // vps id, base layer internal / available
        bw_put(&vps, 0, 6); bw_put(&vps, 0, 3); bw_put(&vps, 1, 1);      // max layers-1, max sub layers-1, temporal id nesting
        bw_put(&vps, 0xffff, 16);
        bw_hevc_ptl(&vps, level);
        bw_put(&vps, 0, 1); bw_ue(&vps, 1); bw_ue(&vps, 0); bw_ue(&vps, 0); // ordering info: dec buffering-1, reorder, latency
        bw_put(&vps, 0, 6); bw_ue(&vps, 0);                              // max layer id, num layer sets-1
        bw_put(&vps, 0, 1); bw_put(&vps, 0, 1);                          // no timing info, no extension
        bw_trailing(&vps);

        BitWriter sps = {0};
        bw_hevc_nal(&sps, 33);
        bw_put(&sps, 0, 4); bw_put(&sps, 0, 3); bw_put(&sps, 1, 1);      // vps id, max sub layers-1, temporal id nesting
        bw_hevc_ptl(&sps, level);
        bw_ue(&sps, 0); bw_ue(&sps, 1); bw_ue(&sps, w); bw_ue(&sps, h);  // sps id, 4:2:0, size
        bw_put(&sps, crop_right || crop_bottom, 1);
        if (crop_right || crop_bottom) { bw_ue(&sps, 0); bw_ue(&sps, crop_right); bw_ue(&sps, 0); bw_ue(&sps, crop_bottom); }
        bw_ue(&sps, 0); bw_ue(&sps, 0); bw_ue(&sps, 8);                  // 8-bit luma/chroma, log2_max_poc_lsb-4
        bw_put(&sps, 0, 1); bw_ue(&sps, 1); bw_ue(&sps, 0); bw_ue(&sps, 0);
        bw_ue(&sps, 0); bw_ue(&sps, 3); bw_ue(&sps, 0); bw_ue(&sps, 3);  // CB 8..64, TB 4..32
        bw_ue(&sps, 4); bw_ue(&sps, 4);                                  // max transform hierarchy depth inter/intra
        bw_put(&sps, 0, 1); bw_put(&sps, 1, 1); bw_put(&sps, 1, 1); bw_put(&sps, 0, 1); // scaling list, AMP, SAO, PCM
        bw_ue(&sps, 0); bw_put(&sps, 0, 1);                              // RPS in slice headers, no long-term refs
        bw_put(&sps, 0, 1); bw_put(&sps, 0, 1);                          // temporal MVP, strong intra smoothing
        bw_put(&sps, 0, 1); bw_put(&sps, 0, 1);                          // no VUI, no extension
        bw_trailing(&sps);

        BitWriter pps = {0};
        bw_hevc_nal(&pps, 34);
        bw_ue(&pps, 0); bw_ue(&pps, 0);
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 1); bw_put(&pps, 0, 3);      // dependent slices, output flag, extra bits
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 1);                          // sign data hiding, cabac init present
        bw_ue(&pps, 0); bw_ue(&pps, 0); bw_se(&pps, 0);                  // num_ref_idx l0/l1 default-1, init_qp-26
        bw_put(&pps, 0, 1); bw_put(&pps, 1, 1); bw_put(&pps, 1, 1);      // constrained intra, transform skip, cu_qp_delta
        bw_ue(&pps, 0); bw_se(&pps, 0); bw_se(&pps, 0);                  // diff_cu_qp_delta_depth, cb/cr offsets
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 1); bw_put(&pps, 0, 1);      // slice chroma offsets, weighted pred/bipred
        bw_put(&pps, 0, 1); bw_put(&pps, 0, 1); bw_put(&pps, 0, 1);      // transquant bypass, tiles, entropy sync
        bw_put(&pps, 1, 1); bw_put(&pps, 0, 1); bw_put(&pps, 0, 1);      // loop filter across slices, deblocking ctl, scaling
        bw_put(&pps, 0, 1); bw_ue(&pps, 0); bw_put(&pps, 0, 1); bw_put(&pps, 0, 1); // list mod, merge level-2, ext
        bw_trailing(&pps);

        BitWriter headers[3] = { vps, sps, pps };
        if (vaapi_add_packed(engine, VAEncPackedHeaderSequence, headers, 3, b->packed, &b->packed_n) != 0) return -1;
    }

    VAEncPictureParameterBufferHEVC pic_p;
    memset(&pic_p, 0, sizeof(pic_p));
    pic_p.decoded_curr_pic.picture_id = curr_recon;
    pic_p.decoded_curr_pic.pic_order_cnt = poc;
    for (int i = 0; i < 15; i++) {
        pic_p.reference_frames[i].picture_id = VA_INVALID_ID;
        pic_p.reference_frames[i].flags = VA_PICTURE_HEVC_INVALID;
    }
    if (!is_idr) {
        pic_p.reference_frames[0].picture_id = prev_recon;
        pic_p.reference_frames[0].pic_order_cnt = poc - 1;
        pic_p.reference_frames[0].flags = VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
    }
    pic_p.coded_buf = engine->coded_buf;
    pic_p.collocated_ref_pic_index = 0xff;
    pic_p.pic_init_qp = 30;
    pic_p.diff_cu_qp_delta_depth = 3;
    pic_p.nal_unit_type = is_idr ? 19 : 1;                            // IDR_W_RADL / TRAIL_R
    pic_p.pic_fields.bits.idr_pic_flag = is_idr;
    pic_p.pic_fields.bits.coding_type = is_idr ? 1 : 2;
    pic_p.pic_fields.bits.reference_pic_flag = 1;
    pic_p.pic_fields.bits.transform_skip_enabled_flag = 1;
    pic_p.pic_fields.bits.cu_qp_delta_enabled_flag = 1;
    pic_p.pic_fields.bits.pps_loop_filter_across_slices_enabled_flag = 1;
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncPictureParameterBufferType, sizeof(pic_p), 1, &pic_p, &b->pic) != VA_STATUS_SUCCESS)
        return -1;

    VAEncSliceParameterBufferHEVC slice_p;
    memset(&slice_p, 0, sizeof(slice_p));
    slice_p.num_ctu_in_slice = ((w + 63) / 64) * ((h + 63) / 64);
    slice_p.slice_type = is_idr ? 2 : 1;                              // I / P
    for (int i = 0; i < 15; i++) {
        slice_p.ref_pic_list0[i].picture_id = slice_p.ref_pic_list1[i].picture_id = VA_INVALID_ID;
        slice_p.ref_pic_list0[i].flags = slice_p.ref_pic_list1[i].flags = VA_PICTURE_HEVC_INVALID;
    }
    if (!is_idr) slice_p.ref_pic_list0[0] = pic_p.reference_frames[0];
    slice_p.max_num_merge_cand = 5;
    slice_p.slice_fields.bits.last_slice_of_pic_flag = 1;
    slice_p.slice_fields.bits.slice_sao_luma_flag = 1;
    slice_p.slice_fields.bits.slice_sao_chroma_flag = 1;
    slice_p.slice_fields.bits.collocated_from_l0_flag = !is_idr;
    if (RT.vaCreateBuffer(engine->va_dpy, engine->enc_ctx, VAEncSliceParameterBufferType, sizeof(slice_p), 1, &slice_p, &b->slice) != VA_STATUS_SUCCESS)
        return -1;

    BitWriter sh = {0};
    bw_hevc_nal(&sh, pic_p.nal_unit_type);
    bw_put(&sh, 1, 1);                                                  // first slice segment
    if (is_idr) bw_put(&sh, 0, 1);                                      // no_output_of_prior_pics
    bw_ue(&sh, 0); bw_ue(&sh, slice_p.slice_type);                      // pps id, slice type
    if (!is_idr) {
        bw_put(&sh, poc & 0xfff, 12);
        bw_put(&sh, 0, 1);                                              // RPS coded here: one previous picture
        bw_ue(&sh, 1); bw_ue(&sh, 0); bw_ue(&sh, 0); bw_put(&sh, 1, 1); // negative 1, positive 0, delta-1 0, used
    }
    bw_put(&sh, 1, 1); bw_put(&sh, 1, 1);                               // SAO luma, chroma
    if (!is_idr) { bw_put(&sh, 0, 1); bw_ue(&sh, 0); }                  // num_ref_idx override, five_minus_max_merge
    bw_se(&sh, 0);                                                      // slice_qp_delta (the driver writes the real one)
    bw_put(&sh, 0, 1);                                                  // loop filter across slices
    bw_put(&sh, 1, 1);                                                  // byte_alignment()
    while (sh.bits % 8) bw_put(&sh, 0, 1);
    return vaapi_add_packed(engine, VAEncPackedHeaderSlice, &sh, 1, b->packed, &b->packed_n);
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

    int is_idr = force_idr || engine->idr_pending || engine->frame_count == 0 || engine->since_idr >= engine->gop;
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

    // 2. Rate control (every IDR) and the codec's sequence / picture / slice parameters + packed headers
    EncBuffers b = { .seq = VA_INVALID_ID, .pic = VA_INVALID_ID, .slice = VA_INVALID_ID };
    int ret = -11;
    if (is_idr) {
        VAEncMiscParameterRateControl rc;
        memset(&rc, 0, sizeof(rc));
        // VBR: bits_per_second is the ceiling (maxrate) and target_percentage the average (#33)
        rc.bits_per_second = (uint32_t)engine->max_bps;
        rc.target_percentage = engine->rc_mode == VA_RC_VBR ? (uint32_t)(engine->target_bps * 100 / engine->max_bps) : 100;
        rc.window_size = 1000;
        rc.initial_qp = 28;
        rc.min_qp = 16;
        // The driver's VBR raises QP only up to max_qp; 44 blocked the capped presets from reaching
        // the QP 51 hardware floor on hard content (2560x1600 testsrc2: 4.16 -> 2.60 Mbps, #33)
        rc.max_qp = 51;
        rc.rc_flags.bits.disable_frame_skip = 1; // a remote desktop must not drop frames silently
        b.misc[b.misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeRateControl, &rc, sizeof(rc));

        VAEncMiscParameterHRD hrd;
        memset(&hrd, 0, sizeof(hrd));
        hrd.buffer_size = (uint32_t)engine->buffer_bits; // the viewer preset's bufsize
        hrd.initial_buffer_fullness = hrd.buffer_size / 2;
        b.misc[b.misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeHRD, &hrd, sizeof(hrd));

        // numerator in the low 16 bits, denominator in the high 16 bits
        VAEncMiscParameterFrameRate fr = { .framerate = (1u << 16) | engine->fps };
        b.misc[b.misc_n++] = vaapi_create_misc_buffer(engine, VAEncMiscParameterTypeFrameRate, &fr, sizeof(fr));
    }
    if ((engine->is_hevc ? vaapi_hevc_params : vaapi_h264_params)(engine, is_idr, curr_recon, prev_recon, &b) != 0) goto done;

    // 3. Render and Encode on AMD VCN hardware
    VABufferID render_b[12];
    int render_n = 0;
    // FFmpeg's order: sequence, rate control misc, picture, packed headers (parameter sets, slice header), slice.
    // With misc first, radeonsi (Mesa 26) ignored the VBR target: the 1M preset ran at 1.6 Mbps, 0.96 after.
    if (b.seq != VA_INVALID_ID) render_b[render_n++] = b.seq;
    for (int i = 0; i < b.misc_n; i++) {
        if (b.misc[i] != VA_INVALID_ID) render_b[render_n++] = b.misc[i];
    }
    render_b[render_n++] = b.pic;
    for (int i = 0; i < b.packed_n; i++) render_b[render_n++] = b.packed[i];
    render_b[render_n++] = b.slice;

    st = RT.vaBeginPicture(engine->va_dpy, engine->enc_ctx, engine->nv12_surf);
    if (st != VA_STATUS_SUCCESS) goto done;
    st = RT.vaRenderPicture(engine->va_dpy, engine->enc_ctx, render_b, render_n);
    if (st != VA_STATUS_SUCCESS) goto done;
    st = RT.vaEndPicture(engine->va_dpy, engine->enc_ctx);
    if (st != VA_STATUS_SUCCESS) goto done;
    st = RT.vaSyncSurface(engine->va_dpy, curr_recon);
    if (st != VA_STATUS_SUCCESS) goto done;

    // 4. Map Bitstream Output; an access unit may arrive as several linked segments
    VACodedBufferSegment *segment = NULL;
    st = RT.vaMapBuffer(engine->va_dpy, engine->coded_buf, (void**)&segment);
    if (st != VA_STATUS_SUCCESS || !segment) goto done;
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

    engine->frame_num = is_idr ? 1 : ((engine->frame_num + 1) & 15);
    engine->since_idr = is_idr ? 1 : engine->since_idr + 1;
    engine->frame_count++;
    ret = 0;

done:
    // No bitstream (failure, empty or slice overflow): the next frame_num / POC would skip, so restart with an IDR
    engine->idr_pending = ret != 0 || *out_len == 0;
    for (int i = 0; i < b.misc_n; i++) {
        if (b.misc[i] != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, b.misc[i]);
    }
    if (b.seq != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, b.seq);
    if (b.pic != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, b.pic);
    if (b.slice != VA_INVALID_ID) RT.vaDestroyBuffer(engine->va_dpy, b.slice);
    for (int i = 0; i < b.packed_n; i++) RT.vaDestroyBuffer(engine->va_dpy, b.packed[i]);
    return ret;
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
