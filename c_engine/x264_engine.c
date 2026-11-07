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
#if defined(__x86_64__)
#include <immintrin.h>
#endif
#include <x264.h>

#define X264C_STR2(x) #x
#define X264C_STR(x) X264C_STR2(x)

typedef struct X264cConfig {
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    uint32_t gop;
    uint32_t cq;
    uint32_t abr;   // 1: viewer bitrate preset -> ABR + VBV; 0: 自动 -> capped CRF
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

// x264c_create failure reasons, reported through the caller's `error` pointer.
enum {
    X264C_OK = 0,
    X264C_ERR_DLOPEN = 1,   // neither libx264.so.<X264_BUILD> nor libx264.so could be loaded
    X264C_ERR_SYMBOL = 2,   // loaded, but a versioned symbol is missing: X264_BUILD mismatch
    X264C_ERR_OPEN = 3,     // parameter setup or x264_encoder_open failed
};

static void *x264c_sym(void *lib, const char *name) {
    void *p = dlsym(lib, name);
    return p;
}

void *x264c_create(const X264cConfig *cfg, int *error) {
    if (error) *error = X264C_OK;
    if (!cfg || cfg->width == 0 || cfg->height == 0 || cfg->fps == 0 || cfg->gop == 0) return NULL;
    // The compile-time X264_BUILD names the runtime major; the unversioned soname is the fallback
    // for a dev symlink.
    void *lib = dlopen("libx264.so." X264C_STR(X264_BUILD), RTLD_NOW | RTLD_LOCAL);
    if (!lib) lib = dlopen("libx264.so", RTLD_NOW | RTLD_LOCAL);
    if (!lib) {
        if (error) *error = X264C_ERR_DLOPEN;
        return NULL;
    }

    X264cEngine *e = calloc(1, sizeof *e);
    if (!e) {
        if (error) *error = X264C_ERR_OPEN;
        dlclose(lib);
        return NULL;
    }
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
        if (error) *error = X264C_ERR_SYMBOL;
        free(e); dlclose(lib); return NULL;
    }

    x264_param_t p;
    if (default_preset(&p, "superfast", "zerolatency") != 0) { if (error) *error = X264C_ERR_OPEN; free(e); dlclose(lib); return NULL; }
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
    // Same rate control as the FFmpeg child: a viewer preset is ABR at 3/4 of the cap, 自动 is
    // CRF, and both set the same VBV bound.
    if (cfg->abr) {
        p.rc.i_rc_method = X264_RC_ABR;
        p.rc.i_bitrate = (int)(cfg->target_bps / 1000);
    } else {
        p.rc.i_rc_method = X264_RC_CRF;
        p.rc.f_rf_constant = (float)cfg->cq;
    }
    if (cfg->max_bps > 0) {
        p.rc.i_vbv_max_bitrate = (int)(cfg->max_bps / 1000);
        p.rc.i_vbv_buffer_size = (int)(cfg->buffer_bits / 1000);
    }
    if (apply_profile(&p, "baseline") != 0) { if (error) *error = X264C_ERR_OPEN; free(e); dlclose(lib); return NULL; }

    e->encoder = e->encoder_open(&p);
    if (!e->encoder) { if (error) *error = X264C_ERR_OPEN; free(e); dlclose(lib); return NULL; }

    const size_t w = cfg->width, h = cfg->height;
    const size_t cw = (w + 1) / 2, ch = (h + 1) / 2;
    e->width = (int)w;
    e->height = (int)h;
    e->stride[0] = (int)w;
    e->stride[1] = e->stride[2] = (int)cw;
    e->plane[0] = malloc(w * h);
    e->plane[1] = malloc(cw * ch);
    e->plane[2] = malloc(cw * ch);
    if (!e->plane[0] || !e->plane[1] || !e->plane[2]) { x264c_destroy(e); if (error) *error = X264C_ERR_OPEN; return NULL; }

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

// Runtime AVX2 dispatch on x86-64: one cached CPU-feature check, scalar stays as the fallback.
#if defined(__x86_64__)
#define X264C_AVX2 __attribute__((target("avx2")))

static int x264c_avx2(void) {
    static int supported = -1;
    if (supported < 0) supported = __builtin_cpu_supports("avx2") ? 1 : 0;
    return supported;
}
#else
static int x264c_avx2(void) { return 0; }
#endif

static inline void x264c_y_row_scalar(const uint8_t *row, uint8_t *yp, int w) {
    for (int x = 0; x < w; x++) {
        const int b = row[4 * x], g = row[4 * x + 1], r = row[4 * x + 2];
        yp[x] = x264c_clip8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
    }
}

#if defined(__x86_64__)
// Eight pixels per iteration in 32-bit lanes; the sum plus rounding fits u16 and the range
// (16..235) needs no clamp, so a saturating pack writes the eight luma bytes directly.
X264C_AVX2 static void x264c_y_row_avx2(const uint8_t *row, uint8_t *yp, int w) {
    const __m256i mask = _mm256_set1_epi32(0xFF);
    const __m256i bias = _mm256_set1_epi32(128 + 16 * 256);
    const __m256i cr = _mm256_set1_epi32(66), cg = _mm256_set1_epi32(129), cb = _mm256_set1_epi32(25);
    int x = 0;
    for (; x + 8 <= w; x += 8) {
        const __m256i p = _mm256_loadu_si256((const __m256i *)(row + 4 * x));
        const __m256i b = _mm256_and_si256(p, mask);
        const __m256i g = _mm256_and_si256(_mm256_srli_epi32(p, 8), mask);
        const __m256i r = _mm256_and_si256(_mm256_srli_epi32(p, 16), mask);
        __m256i y32 = _mm256_add_epi32(_mm256_mullo_epi32(r, cr), _mm256_mullo_epi32(g, cg));
        y32 = _mm256_add_epi32(y32, _mm256_mullo_epi32(b, cb));
        y32 = _mm256_srli_epi32(_mm256_add_epi32(y32, bias), 8);
        __m128i p16 = _mm_packus_epi32(_mm256_castsi256_si128(y32), _mm256_extracti128_si256(y32, 1));
        _mm_storel_epi64((__m128i *)(yp + x), _mm_packus_epi16(p16, p16));
    }
    if (x < w) x264c_y_row_scalar(row + 4 * x, yp + x, w - x);
}

// 2x2 sums for U and V; maddubs multiplies the packed B,G,R,X bytes by the signed
// BT.601 coefficients (all fit int8), madd with ones collapses each pixel to one u32.
X264C_AVX2 static inline __m256i x264c_uv_2rows(const uint8_t *r0, const uint8_t *r1, const __m256i coeff, const __m256i ones) {
    const __m256i a = _mm256_maddubs_epi16(_mm256_loadu_si256((const __m256i *)r0), coeff);
    const __m256i b = _mm256_maddubs_epi16(_mm256_loadu_si256((const __m256i *)r1), coeff);
    return _mm256_add_epi32(_mm256_madd_epi16(a, ones), _mm256_madd_epi16(b, ones));
}

// Eight chroma samples (16 columns) per iteration: row sums, pairwise horizontal add,
// then (sum + 512) >> 10 + 128. The result range is 16..239, so no clamp is needed.
X264C_AVX2 static void x264c_uv_row_avx2(const uint8_t *r0, const uint8_t *r1, uint8_t *up, uint8_t *vp, int w) {
    const __m256i ones = _mm256_set1_epi16(1);
    // Coefficients follow the packed byte order B,G,R,X (BT.601 as in the scalar loop).
    const __m256i ucoeff = _mm256_setr_epi8(112, -74, -38, 0, 112, -74, -38, 0, 112, -74, -38, 0, 112, -74, -38, 0,
                                            112, -74, -38, 0, 112, -74, -38, 0, 112, -74, -38, 0, 112, -74, -38, 0);
    const __m256i vcoeff = _mm256_setr_epi8(-18, -94, 112, 0, -18, -94, 112, 0, -18, -94, 112, 0, -18, -94, 112, 0,
                                            -18, -94, 112, 0, -18, -94, 112, 0, -18, -94, 112, 0, -18, -94, 112, 0);
    const __m256i idx = _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7);
    const __m256i round = _mm256_set1_epi32(512), half = _mm256_set1_epi32(128);
    const int cw = w / 2;
    int cx = 0;
    for (; cx + 8 <= cw; cx += 8) {
        const int x = 2 * cx;
        __m256i us = _mm256_hadd_epi32(x264c_uv_2rows(r0 + 4 * x, r1 + 4 * x, ucoeff, ones),
                                       x264c_uv_2rows(r0 + 4 * x + 32, r1 + 4 * x + 32, ucoeff, ones));
        __m256i vs = _mm256_hadd_epi32(x264c_uv_2rows(r0 + 4 * x, r1 + 4 * x, vcoeff, ones),
                                       x264c_uv_2rows(r0 + 4 * x + 32, r1 + 4 * x + 32, vcoeff, ones));
        us = _mm256_add_epi32(_mm256_srai_epi32(_mm256_add_epi32(_mm256_permutevar8x32_epi32(us, idx), round), 10), half);
        vs = _mm256_add_epi32(_mm256_srai_epi32(_mm256_add_epi32(_mm256_permutevar8x32_epi32(vs, idx), round), 10), half);
        __m128i u16 = _mm_packus_epi32(_mm256_castsi256_si128(us), _mm256_extracti128_si256(us, 1));
        __m128i v16 = _mm_packus_epi32(_mm256_castsi256_si128(vs), _mm256_extracti128_si256(vs, 1));
        _mm_storel_epi64((__m128i *)(up + cx), _mm_packus_epi16(u16, u16));
        _mm_storel_epi64((__m128i *)(vp + cx), _mm_packus_epi16(v16, v16));
    }
    for (; cx < cw; cx++) {
        const int x = 2 * cx;
        int b = r0[4 * x] + r0[4 * x + 4] + r1[4 * x] + r1[4 * x + 4];
        int g = r0[4 * x + 1] + r0[4 * x + 5] + r1[4 * x + 1] + r1[4 * x + 5];
        int r = r0[4 * x + 2] + r0[4 * x + 6] + r1[4 * x + 2] + r1[4 * x + 6];
        up[cx] = x264c_clip8(((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128);
        vp[cx] = x264c_clip8(((112 * r - 94 * g - 18 * b + 512) >> 10) + 128);
    }
}

#endif // __x86_64__

static void x264c_uv_row_scalar(const uint8_t *r0, const uint8_t *r1, uint8_t *up, uint8_t *vp, int w) {
    const int cw = w / 2;
    for (int cx = 0; cx < cw; cx++) {
        const int x = 2 * cx;
        int b = r0[4 * x] + r0[4 * x + 4] + r1[4 * x] + r1[4 * x + 4];
        int g = r0[4 * x + 1] + r0[4 * x + 5] + r1[4 * x + 1] + r1[4 * x + 5];
        int r = r0[4 * x + 2] + r0[4 * x + 6] + r1[4 * x + 2] + r1[4 * x + 6];
        up[cx] = x264c_clip8(((-38 * r - 74 * g + 112 * b + 512) >> 10) + 128);
        vp[cx] = x264c_clip8(((112 * r - 94 * g - 18 * b + 512) >> 10) + 128);
    }
}

// Whole-frame BGRX -> I420 with runtime AVX2 dispatch. `y`/`u`/`v` are separate planes
// (strides w, w/2, w/2); width and height must be even.
static void x264c_convert_i420(const uint8_t *src, uint32_t pitch, uint8_t *y, uint8_t *u, uint8_t *v,
                               int w, int h, int avx2) {
    const int cw = w / 2;
    for (int row = 0; row < h; row++) {
        const uint8_t *src_row = src + (size_t)row * pitch;
#if defined(__x86_64__)
        if (avx2) { x264c_y_row_avx2(src_row, y + (size_t)row * w, w); continue; }
#else
        (void)avx2;
#endif
        x264c_y_row_scalar(src_row, y + (size_t)row * w, w);
    }
    for (int row = 0; row + 1 < h; row += 2) {
        const uint8_t *r0 = src + (size_t)row * pitch, *r1 = src + (size_t)(row + 1) * pitch;
        uint8_t *up = u + (size_t)(row / 2) * cw, *vp = v + (size_t)(row / 2) * cw;
#if defined(__x86_64__)
        if (avx2) { x264c_uv_row_avx2(r0, r1, up, vp, w); continue; }
#endif
        x264c_uv_row_scalar(r0, r1, up, vp, w);
    }
}

void x264c_convert_bgrx_i420(const uint8_t *src, uint32_t pitch, uint8_t *y, uint8_t *u, uint8_t *v,
                             int w, int h) {
    if (!src || !y || !u || !v || w <= 0 || h <= 0) return;
    x264c_convert_i420(src, pitch, y, u, v, w, h, x264c_avx2());
}

// Test-only hook: converts with or without AVX2 and reports which path ran (1 = AVX2).
int x264c_test_convert(const uint8_t *src, uint32_t pitch, uint8_t *y, uint8_t *u, uint8_t *v,
                       int w, int h, int force_scalar) {
    if (!src || !y || !u || !v || w <= 0 || h <= 0) return 0;
    const int use = force_scalar ? 0 : x264c_avx2();
    x264c_convert_i420(src, pitch, y, u, v, w, h, use);
    return use;
}

// One destination row pair: luma rows plus the interlaced U/V row. Same helpers as the
// whole-frame path, so a fused scale+convert is byte-identical to scale then convert.
void x264c_convert_row_pair(const uint8_t *top, const uint8_t *bottom, uint8_t *y_top, uint8_t *y_bottom,
                            uint8_t *u, uint8_t *v, int w) {
    if (!top || !bottom || !y_top || !y_bottom || !u || !v || w <= 0) return;
#if defined(__x86_64__)
    if (x264c_avx2()) {
        x264c_y_row_avx2(top, y_top, w);
        x264c_y_row_avx2(bottom, y_bottom, w);
        x264c_uv_row_avx2(top, bottom, u, v, w);
        return;
    }
#endif
    x264c_y_row_scalar(top, y_top, w);
    x264c_y_row_scalar(bottom, y_bottom, w);
    x264c_uv_row_scalar(top, bottom, u, v, w);
}

static int x264c_pic_encode(X264cEngine *e, int force_idr, const uint8_t **out_data, uint32_t *out_len) {
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

// x264 copies the input picture during encoder_encode, so the caller may reuse the planes
// (and their memory) as soon as this returns.
int x264c_encode_i420(X264cEngine *e, const uint8_t *y, const uint8_t *u, const uint8_t *v, int force_idr,
                      const uint8_t **out_data, uint32_t *out_len) {
    if (!e || !y || !u || !v || !out_data || !out_len) return -1;
    e->pic.img.plane[0] = (uint8_t *)y;
    e->pic.img.plane[1] = (uint8_t *)u;
    e->pic.img.plane[2] = (uint8_t *)v;
    e->pic.img.i_stride[0] = e->width;
    e->pic.img.i_stride[1] = e->pic.img.i_stride[2] = e->width / 2;
    return x264c_pic_encode(e, force_idr, out_data, out_len);
}

int x264c_encode_frame(X264cEngine *e, const uint8_t *src, uint32_t pitch, int force_idr,
                       const uint8_t **out_data, uint32_t *out_len) {
    if (!e || !src || !out_data || !out_len) return -1;
    x264c_convert_i420(src, pitch, e->plane[0], e->plane[1], e->plane[2], e->width, e->height, x264c_avx2());
    e->pic.img.plane[0] = e->plane[0];
    e->pic.img.plane[1] = e->plane[1];
    e->pic.img.plane[2] = e->plane[2];
    e->pic.img.i_stride[0] = e->width;
    e->pic.img.i_stride[1] = e->pic.img.i_stride[2] = e->stride[1];
    return x264c_pic_encode(e, force_idr, out_data, out_len);
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
