// In-process libx264 H.264 engine: libx264 is dlopen'ed at runtime (no link-time dependency),
// the X11 ZPixmap frame (little-endian [B,G,R,X], the same word-order ARGB that NVENC receives)
// is converted to I420 on the CPU and encoded with the superfast/zerolatency preset the FFmpeg
// child process used to use. Because the encoder lives in the host, PLI/FIR can force an IDR
// immediately, so this path may keep the long 2 s GOP (GOP_SECONDS_INPROC) instead of the
// 0.5 s fallback the child-process path needs (see the GOP comment in src/video.rs).
// The compile-time x264.h must match the runtime library's X264_BUILD; a mismatch fails the
// versioned x264_encoder_open_<BUILD> lookup and the caller falls back to the process pipeline.

#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <x264.h>

#define X264C_STR2(x) #x
#define X264C_STR(x) X264C_STR2(x)

typedef struct X264cConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    uint32_t cq;
    int64_t target_bps;
    int64_t max_bps;
    int64_t buffer_bits;
} X264cConfig;

typedef struct X264cEngine {
    void *lib;
    x264_t *encoder;
    x264_t *(*encoder_open)(x264_param_t *);
    int (*encoder_encode)(x264_t *, x264_nal_t **, int *, x264_picture_t *, x264_picture_t *);
    void (*encoder_close)(x264_t *);
    void (*picture_init)(x264_picture_t *);
    x264_picture_t pic;
    uint8_t *plane[3];
    int stride[3];
    int width;
    int height;
    uint8_t *out;
    size_t out_cap;
} X264cEngine;

void x264c_destroy(X264cEngine *e);

static void *x264c_sym(void *lib, const char *name) {
    void *p = dlsym(lib, name);
    return p;
}

void *x264c_create(const X264cConfig *cfg) {
    if (!cfg || cfg->width == 0 || cfg->height == 0 || cfg->fps == 0 || cfg->gop == 0) return NULL;
    void *lib = dlopen("libx264.so", RTLD_NOW | RTLD_LOCAL);
    if (!lib) lib = dlopen("libx264.so." X264C_STR(X264_BUILD), RTLD_NOW | RTLD_LOCAL);
    if (!lib) return NULL;

    X264cEngine *e = calloc(1, sizeof *e);
    if (!e) { dlclose(lib); return NULL; }
    e->lib = lib;
    e->encoder_open = (x264_t *(*)(x264_param_t *))x264c_sym(lib, X264C_STR(x264_encoder_open));
    e->encoder_encode = (int (*)(x264_t *, x264_nal_t **, int *, x264_picture_t *, x264_picture_t *))x264c_sym(lib, "x264_encoder_encode");
    e->encoder_close = (void (*)(x264_t *))x264c_sym(lib, "x264_encoder_close");
    e->picture_init = (void (*)(x264_picture_t *))x264c_sym(lib, "x264_picture_init");
    int (*default_preset)(x264_param_t *, const char *, const char *) =
        (int (*)(x264_param_t *, const char *, const char *))x264c_sym(lib, "x264_param_default_preset");
    int (*apply_profile)(x264_param_t *, const char *) =
        (int (*)(x264_param_t *, const char *))x264c_sym(lib, "x264_param_apply_profile");
    if (!e->encoder_open || !e->encoder_encode || !e->encoder_close || !e->picture_init
        || !default_preset || !apply_profile) {
        free(e); dlclose(lib); return NULL;
    }

    x264_param_t p;
    if (default_preset(&p, "superfast", "zerolatency") != 0) { free(e); dlclose(lib); return NULL; }
    p.i_width = (int)cfg->width;
    p.i_height = (int)cfg->height;
    p.i_fps_num = (int)cfg->fps;
    p.i_fps_den = 1;
    p.i_timebase_num = 1;
    p.i_timebase_den = (int)cfg->fps;
    p.b_vfr_input = 0;
    // On-demand IDR exists, so the keyframe interval only bounds recovery after a lost stream.
    p.i_keyint_max = (int)cfg->gop;
    p.i_keyint_min = (int)cfg->gop;
    p.i_scenecut_threshold = 0;
    p.b_aud = 1;               // the reader splits access units on AUD, like the child process
    p.b_repeat_headers = 1;    // SPS/PPS ahead of every IDR
    p.i_threads = 4;           // the child process ran -threads 4
    if (cfg->max_bps > 0) {
        p.rc.i_rc_method = X264_RC_ABR;
        p.rc.i_bitrate = (int)(cfg->target_bps / 1000);
        p.rc.i_vbv_max_bitrate = (int)(cfg->max_bps / 1000);
        p.rc.i_vbv_buffer_size = (int)(cfg->buffer_bits / 1000);
    } else {
        p.rc.i_rc_method = X264_RC_CRF;
        p.rc.f_rf_constant = (float)cfg->cq;
    }
    if (apply_profile(&p, "baseline") != 0) { free(e); dlclose(lib); return NULL; }

    e->encoder = e->encoder_open(&p);
    if (!e->encoder) { free(e); dlclose(lib); return NULL; }

    const size_t w = cfg->width, h = cfg->height;
    const size_t cw = (w + 1) / 2, ch = (h + 1) / 2;
    e->width = (int)w;
    e->height = (int)h;
    e->stride[0] = (int)w;
    e->stride[1] = e->stride[2] = (int)cw;
    e->plane[0] = malloc(w * h);
    e->plane[1] = malloc(cw * ch);
    e->plane[2] = malloc(cw * ch);
    if (!e->plane[0] || !e->plane[1] || !e->plane[2]) { x264c_destroy(e); return NULL; }

    e->picture_init(&e->pic);
    e->pic.img.i_csp = X264_CSP_I420;
    e->pic.img.i_plane = 3;
    for (int i = 0; i < 3; i++) {
        e->pic.img.plane[i] = e->plane[i];
        e->pic.img.i_stride[i] = e->stride[i];
    }
    return e;
}

// BT.601 limited range, the integer coefficients swscale uses for bgr0 -> yuv420p.
static inline uint8_t x264c_clip8(int v) { return v < 0 ? 0 : (v > 255 ? 255 : (uint8_t)v); }

static void x264c_convert_bgrx_to_i420(X264cEngine *e, const uint8_t *src, uint32_t pitch) {
    const int w = e->width, h = e->height;
    const int cw = e->stride[1];
    for (int y = 0; y < h; y++) {
        const uint8_t *row = src + (size_t)y * pitch;
        uint8_t *yp = e->plane[0] + (size_t)y * w;
        for (int x = 0; x < w; x++) {
            const int b = row[4 * x], g = row[4 * x + 1], r = row[4 * x + 2];
            yp[x] = x264c_clip8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }
    for (int y = 0; y < h - 1; y += 2) {
        const uint8_t *row0 = src + (size_t)y * pitch;
        const uint8_t *row1 = src + (size_t)(y + 1) * pitch;
        uint8_t *up = e->plane[1] + (size_t)(y / 2) * cw;
        uint8_t *vp = e->plane[2] + (size_t)(y / 2) * cw;
        for (int x = 0; x < w - 1; x += 2) {
            int b = row0[4 * x] + row0[4 * x + 4] + row1[4 * x] + row1[4 * x + 4];
            int g = row0[4 * x + 1] + row0[4 * x + 5] + row1[4 * x + 1] + row1[4 * x + 5];
            int r = row0[4 * x + 2] + row0[4 * x + 6] + row1[4 * x + 2] + row1[4 * x + 6];
            up[x / 2] = x264c_clip8(((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128);
            vp[x / 2] = x264c_clip8(((112 * r - 94 * g - 18 * b + 512) >> 10) + 128);
        }
    }
}

int x264c_encode_frame(X264cEngine *e, const uint8_t *src, uint32_t pitch, int force_idr,
                       const uint8_t **out_data, uint32_t *out_len) {
    if (!e || !src || !out_data || !out_len) return -1;
    x264c_convert_bgrx_to_i420(e, src, pitch);
    e->pic.i_pts++;
    e->pic.i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;

    x264_nal_t *nals;
    int nal_count;
    x264_picture_t pic_out;
    if (e->encoder_encode(e->encoder, &nals, &nal_count, &e->pic, &pic_out) < 0) return -2;

    size_t total = 0;
    for (int i = 0; i < nal_count; i++) total += nals[i].i_payload;
    if (total > e->out_cap) {
        uint8_t *grown = realloc(e->out, total);
        if (!grown) return -3;
        e->out = grown;
        e->out_cap = total;
    }
    size_t at = 0;
    for (int i = 0; i < nal_count; i++) {
        memcpy(e->out + at, nals[i].p_payload, nals[i].i_payload);
        at += nals[i].i_payload;
    }
    *out_data = e->out;
    *out_len = (uint32_t)total;
    return 0;
}

void x264c_destroy(X264cEngine *e) {
    if (!e) return;
    if (e->encoder) e->encoder_close(e->encoder);
    free(e->plane[0]);
    free(e->plane[1]);
    free(e->plane[2]);
    free(e->out);
    if (e->lib) dlclose(e->lib);
    free(e);
}
