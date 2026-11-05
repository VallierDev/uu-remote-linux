/*
 * Off-screen synthetic GBM DMA-BUF import probe; no screen capture.
 * Build: cc -std=c11 -Wall -Wextra -Werror -O2 -o /tmp/nvidia-gbm-egl-probe \
 *   contrib/nvidia-gbm-egl-probe.c $(pkg-config --cflags --libs gbm libdrm egl glesv2) -ldl
 * Run: /tmp/nvidia-gbm-egl-probe /dev/dri/by-path/pci-0000:26:00.0-render
 * CUDA-EGL and CUDA-GL registration are opportunistic and require a compatible driver.
 * For mapped-frame content verification, an official NVIDIA CUDA runtime wheel
 * provides headers without installing the toolkit (x86_64, CUDA runtime 12.9.79):
 * curl -fL https://files.pythonhosted.org/packages/bc/46/a92db19b8309581092a3add7e6fceb4c301a3fd233969856a8cbf042cd3c/nvidia_cuda_runtime_cu12-12.9.79-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl -o /tmp/uu-cuda.whl
 * echo '25bba2dfb01d48a9b59ca474a1ac43c6ebf7011f1b0b8cc44f54eb6ac48a96c3  /tmp/uu-cuda.whl' | sha256sum -c -
 * unzip -q -o /tmp/uu-cuda.whl -d /tmp/uu-cuda
 * Build as above with -I/tmp/uu-cuda/nvidia/cuda_runtime/include added to cc.
 * Source: https://pypi.org/project/nvidia-cuda-runtime-cu12/12.9.79/
 * Exit 0: image import, modifier query, applicable GL content test, CUDA-EGL
 * mapped-frame pixel verification, applicable CUDA-GL map and all cleanup succeeded. An
 * external-only GL binding is not a content test and remains inconclusive.
 * Exit 1: a test or cleanup failed; exit 2: incomplete/unavailable tests.
 * This does not test PipeWire buffers, cross-process synchronization or zero-copy.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <fcntl.h>
#include <unistd.h>
#include <dlfcn.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#if defined(__has_include)
# if __has_include(<cuda.h>) && __has_include(<cudaEGL.h>)
#  include <cuda.h>
#  include <cudaEGL.h>
#  define HAVE_CUDA_EGL_HEADERS 1
# endif
#endif

static int has_ext(const char *list, const char *name) {
    size_t n = strlen(name);
    const char *p = list;
    if (!p) return 0;
    while ((p = strstr(p, name))) {
        if ((p == list || p[-1] == ' ') && (p[n] == 0 || p[n] == ' ')) return 1;
        p += n;
    }
    return 0;
}

enum status { INCONCLUSIVE, PASS, FAIL, NOT_APPLICABLE };
static const char *status_name(enum status s) {
    static const char *const names[] = {"INCONCLUSIVE", "PASS", "FAIL", "NOT_APPLICABLE"};
    return names[s];
}
static int summary_exit(enum status import, enum status modifier, enum status gl,
                        enum status external, enum status cuda_egl, enum status cuda_gl, enum status cleanup) {
    enum status all[] = {import, modifier, gl, external, cuda_egl, cuda_gl, cleanup};
    int incomplete = 0;
    for (size_t i = 0; i < sizeof(all) / sizeof(all[0]); i++) {
        if (all[i] == FAIL) return 1;
        if (all[i] == INCONCLUSIVE) incomplete = 1;
    }
    return incomplete ? 2 : 0;
}

/* CUDA driver entry points, loaded at runtime to avoid requiring the toolkit. */
static enum status try_cuda(GLuint texture) {
    enum status status = INCONCLUSIVE;
    void *lib = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL), *ctx = NULL, *res = NULL;
    int rc, devs[16]; unsigned count = 0;
    int (*init)(unsigned), (*getdev)(unsigned *, int *, unsigned, unsigned);
    int (*create)(void **, unsigned, int), (*destroy)(void *);
    int (*reg)(void **, unsigned, unsigned, unsigned), (*unreg)(void *);
    int (*map)(unsigned, void **, void *), (*unmap)(unsigned, void **, void *);
    if (!lib) { puts("CUDA-GL: unavailable (libcuda.so.1)"); return status; }
#define SYM(v, n) do { *(void **)(&(v)) = dlsym(lib, (n)); if (!(v)) { printf("CUDA-GL: missing %s\n", (n)); goto out; } } while (0)
    SYM(init, "cuInit"); SYM(getdev, "cuGLGetDevices_v2");
    SYM(create, "cuCtxCreate_v2"); SYM(destroy, "cuCtxDestroy_v2");
    SYM(reg, "cuGraphicsGLRegisterImage"); SYM(unreg, "cuGraphicsUnregisterResource");
    SYM(map, "cuGraphicsMapResources"); SYM(unmap, "cuGraphicsUnmapResources");
#undef SYM
    status = FAIL;
    if ((rc = init(0))) { printf("CUDA-GL: cuInit failed %d\n", rc); goto out; }
    /* CU_GL_DEVICE_LIST_ALL = 1; query binds the choice to current GL context. */
    if ((rc = getdev(&count, devs, 16, 1)) || !count) {
        printf("CUDA-GL: current GL context has no CUDA device (rc=%d count=%u)\n", rc, count); goto out;
    }
    if ((rc = create(&ctx, 0, devs[0]))) { printf("CUDA-GL: context failed %d\n", rc); goto out; }
    if ((rc = reg(&res, texture, GL_TEXTURE_2D, 0))) { printf("CUDA-GL: registration failed %d\n", rc); goto cleanup; }
    if ((rc = map(1, &res, NULL))) printf("CUDA-GL: map failed %d\n", rc);
    else {
        puts("CUDA-GL: registered and mapped synthetic imported texture (CUDA content not verified)");
        if ((rc = unmap(1, &res, NULL))) printf("CUDA-GL: unmap failed %d\n", rc);
        else status = PASS;
    }
    if ((rc = unreg(res))) { printf("CUDA-GL: unregister failed %d\n", rc); status = FAIL; }
cleanup:
    if ((rc = destroy(ctx))) { printf("CUDA-GL: context destroy failed %d\n", rc); status = FAIL; }
out:
    if (dlclose(lib)) { fprintf(stderr, "CUDA-GL: dlclose failed\n"); status = FAIL; }
    return status;
}

/* Uses the CUDA device associated with the current GL context, but registers
 * the EGLImage directly, without depending on GL texture binding. */
static enum status try_cuda_egl(EGLImageKHR image, int content_ready) {
    enum status status = INCONCLUSIVE;
    void *lib = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL), *ctx = NULL, *res = NULL;
    int rc, devs[16]; unsigned count = 0;
    int (*init)(unsigned), (*getdev)(unsigned *, int *, unsigned, unsigned);
    int (*create)(void **, unsigned, int), (*destroy)(void *);
    int (*reg)(void **, EGLImageKHR, unsigned), (*unreg)(void *);
    (void)content_ready;
    if (!lib) { puts("CUDA-EGL: libcuda.so.1 unavailable"); return status; }
#define SYM(v, n) do { *(void **)(&(v)) = dlsym(lib, (n)); if (!(v)) { printf("CUDA-EGL: missing %s\n", (n)); goto out; } } while (0)
    SYM(init, "cuInit"); SYM(getdev, "cuGLGetDevices_v2");
    SYM(create, "cuCtxCreate_v2"); SYM(destroy, "cuCtxDestroy_v2");
    SYM(reg, "cuGraphicsEGLRegisterImage"); SYM(unreg, "cuGraphicsUnregisterResource");
#undef SYM
    status = FAIL;
    rc = init(0); printf("CUDA-EGL cuInit: %d\n", rc); if (rc) goto out;
    rc = getdev(&count, devs, 16, 1);
    printf("CUDA-EGL cuGLGetDevices: %d count=%u\n", rc, count);
    if (rc || !count) goto out;
    rc = create(&ctx, 0, devs[0]); printf("CUDA-EGL cuCtxCreate: %d\n", rc);
    if (rc) goto out;
    rc = reg(&res, image, 0); printf("CUDA-EGL cuGraphicsEGLRegisterImage: %d\n", rc);
    if (!rc) {
#ifdef HAVE_CUDA_EGL_HEADERS
        CUresult (*mapped)(CUeglFrame *, CUgraphicsResource, unsigned, unsigned);
        CUresult (*copy2d)(const CUDA_MEMCPY2D *);
        CUeglFrame frame;
        *(void **)(&copy2d) = dlsym(lib, "cuMemcpy2D_v2");
        *(void **)(&mapped) = dlsym(lib, "cuGraphicsResourceGetMappedEglFrame");
        if (!mapped) { puts("CUDA-EGL: missing cuGraphicsResourceGetMappedEglFrame"); status = INCONCLUSIVE; }
        else {
            memset(&frame, 0, sizeof(frame));
            rc = (int)mapped(&frame, (CUgraphicsResource)res, 0, 0);
            printf("CUDA-EGL cuGraphicsResourceGetMappedEglFrame: %d\n", rc);
            if (!rc) {
                unsigned char pixels[64 * 64 * 4];
                CUDA_MEMCPY2D copy = {0};
                printf("CUDA-EGL frame: %ux%u planes=%u frameType=%d color=%d format=%d channels=%u pitch=%u\n", frame.width, frame.height, frame.planeCount, (int)frame.frameType, (int)frame.eglColorFormat, (int)frame.cuFormat, frame.numChannels, frame.pitch);
                status = INCONCLUSIVE;
                if (!content_ready || !copy2d) puts("CUDA-EGL: producer pattern or copy entry point unavailable");
                else if (frame.width != 64 || frame.height != 64 || frame.planeCount != 1 || frame.numChannels != 4 || frame.cuFormat != CU_AD_FORMAT_UNSIGNED_INT8 || frame.eglColorFormat != CU_EGL_COLOR_FORMAT_ARGB) puts("CUDA-EGL: unsupported frame layout; content not verified");
                else {
                    copy.dstMemoryType = CU_MEMORYTYPE_HOST; copy.dstHost = pixels; copy.dstPitch = 64 * 4;
                    copy.WidthInBytes = 64 * 4; copy.Height = 64;
                    if (frame.frameType == CU_EGL_FRAME_TYPE_ARRAY && frame.frame.pArray[0]) {
                        copy.srcMemoryType = CU_MEMORYTYPE_ARRAY; copy.srcArray = frame.frame.pArray[0];
                    } else if (frame.frameType == CU_EGL_FRAME_TYPE_PITCH && frame.frame.pPitch[0] && frame.pitch >= 64 * 4) {
                        copy.srcMemoryType = CU_MEMORYTYPE_DEVICE; copy.srcDevice = (CUdeviceptr)(uintptr_t)frame.frame.pPitch[0]; copy.srcPitch = frame.pitch;
                    }
                    if (!copy.srcMemoryType) puts("CUDA-EGL: unsupported frame storage");
                    else if ((rc = (int)copy2d(&copy))) { printf("CUDA-EGL: copy to host failed %d\n", rc); status = FAIL; }
                    else {
                        status = PASS;
                        for (unsigned y = 0; y < 64 && status == PASS; y++) for (unsigned x = 0; x < 64; x++) {
                            const unsigned char *p = pixels + (y * 64 + x) * 4;
                            if (p[0] != x || p[1] != y || p[2] != (x ^ y) || p[3] != 255) { printf("CUDA-EGL: color mismatch at %u,%u: %u %u %u %u\n", x, y, p[0], p[1], p[2], p[3]); status = FAIL; break; }
                        }
                        if (status == PASS) puts("CUDA-EGL: all synthetic BGRA pixels read and verified");
                    }
                }
            }
        }
#else
        puts("CUDA-EGL mapped frame: not attempted (CUDA EGL development headers unavailable at build time)");
        status = INCONCLUSIVE;
#endif
        rc = unreg(res); printf("CUDA-EGL cuGraphicsUnregisterResource: %d\n", rc); if (rc) status = FAIL;
    }
    rc = destroy(ctx); printf("CUDA-EGL cuCtxDestroy: %d\n", rc); if (rc) status = FAIL;
out:
    puts("CUDA-EGL: real Portal buffer and cross-process synchronization not verified");
    if (dlclose(lib)) { fprintf(stderr, "CUDA-EGL: dlclose failed\n"); status = FAIL; }
    return status;
}

/* Returns -1 when unknown, otherwise the linear modifier's external-only flag. */
static int query_modifiers(EGLDisplay dpy) {
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query =
        (PFNEGLQUERYDMABUFMODIFIERSEXTPROC)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    EGLint n = 0, got = 0;
    EGLuint64KHR *mods = NULL;
    EGLBoolean *external = NULL;
    int found = 0, result = -1;
    if (!query) { puts("Modifier query: entry point unavailable"); return -1; }
    if (!query(dpy, DRM_FORMAT_ARGB8888, 0, NULL, NULL, &n)) {
        fprintf(stderr, "Modifier count query failed: 0x%x\n", eglGetError()); return -1;
    }
    printf("ARGB8888 modifier count: %d\n", n);
    if (n <= 0 || n > 65536) { puts("Linear modifier: not reported (or invalid count)"); return -1; }
    mods = calloc((size_t)n, sizeof(*mods));
    external = calloc((size_t)n, sizeof(*external));
    if (!mods || !external) { fprintf(stderr, "Modifier query allocation failed\n"); goto done; }
    if (!query(dpy, DRM_FORMAT_ARGB8888, n, mods, external, &got)) {
        fprintf(stderr, "Modifier list query failed: 0x%x\n", eglGetError()); goto done;
    }
    if (got < 0 || got > n) { fprintf(stderr, "Invalid modifier count: %d\n", got); goto done; }
    for (EGLint i = 0; i < got; i++) if (mods[i] == DRM_FORMAT_MOD_LINEAR) {
        printf("ARGB8888 linear modifier: external_only=%s (%u)\n",
               external[i] == EGL_TRUE ? "true" : external[i] == EGL_FALSE ? "false" : "invalid", external[i]);
        if (found || (external[i] != EGL_TRUE && external[i] != EGL_FALSE)) { result = -1; goto done; }
        result = external[i] == EGL_TRUE;
        found = 1;
    }
    if (!found) puts("ARGB8888 linear modifier: not reported");
done:
    free(mods); free(external);
    return result;
}

int main(int argc, char **argv) {
    int fd = -1, dma = -1, initialized = 0, external_only = -1;
    enum status imported = FAIL, mod_status = INCONCLUSIVE, gl = INCONCLUSIVE;
    enum status gl_external = NOT_APPLICABLE, gl_2d = INCONCLUSIVE;
    enum status cuda_egl = INCONCLUSIVE, cuda_gl = INCONCLUSIVE, cleanup = PASS;
    struct gbm_device *gbm = NULL; struct gbm_bo *bo = NULL;
    EGLDisplay dpy = EGL_NO_DISPLAY; EGLContext ctx = EGL_NO_CONTEXT;
    EGLSurface surf = EGL_NO_SURFACE; EGLImageKHR image = EGL_NO_IMAGE_KHR;
    GLuint tex = 0, external_tex = 0, fbo = 0; EGLConfig config; EGLint count, major, minor;
    int external_supported = 0, content_ready = 0, surfaceless = 0;
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image = NULL;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    const char *ext; uint64_t modifier; unsigned char pixel[4] = {0};
    if (argc != 2) { fprintf(stderr, "usage: %s NVIDIA-render-node\n", argv[0]); return 2; }
    printf("Synthetic off-screen probe, node: %s\n", argv[1]);
    fd = open(argv[1], O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open render node"); goto out; }
    gbm = gbm_create_device(fd);
    if (!gbm) { perror("gbm_create_device"); goto out; }
    bo = gbm_bo_create(gbm, 64, 64, GBM_FORMAT_ARGB8888, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    if (!bo) {
        const uint64_t linear = DRM_FORMAT_MOD_LINEAR;
        perror("gbm_bo_create linear ARGB8888; trying explicit linear modifier");
        bo = gbm_bo_create_with_modifiers(gbm, 64, 64, GBM_FORMAT_ARGB8888, &linear, 1);
        if (!bo) { perror("gbm_bo_create_with_modifiers linear"); goto out; }
    }
    modifier = gbm_bo_get_modifier(bo);
    printf("GBM: %ux%u stride=%u modifier=0x%016llx planes=%d\n", gbm_bo_get_width(bo), gbm_bo_get_height(bo), gbm_bo_get_stride(bo), (unsigned long long)modifier, gbm_bo_get_plane_count(bo));
    if (gbm_bo_get_plane_count(bo) != 1 || modifier != DRM_FORMAT_MOD_LINEAR) {
        fprintf(stderr, "Not an explicitly linear single-plane buffer; cannot test safely\n"); goto out;
    }
    {
        uint32_t stride = 0; void *map_data = NULL;
        unsigned char *p = gbm_bo_map(bo, 0, 0, 64, 64, GBM_BO_TRANSFER_WRITE, &stride, &map_data);
        if (!p) perror("GBM synthetic producer map unavailable");
        else {
            if (stride >= 64 * 4) {
                for (unsigned y = 0; y < 64; y++) for (unsigned x = 0; x < 64; x++) {
                    unsigned char *pixel = p + y * stride + x * 4;
                    pixel[0] = x; pixel[1] = y; pixel[2] = x ^ y; pixel[3] = 255;
                }
                content_ready = 1;
            } else fprintf(stderr, "GBM map returned invalid stride\n");
            gbm_bo_unmap(bo, map_data);
        }
    }
    dma = gbm_bo_get_fd(bo);
    if (dma < 0) { perror("gbm_bo_get_fd"); goto out; }
    ext = eglQueryString(EGL_NO_DISPLAY, EGL_EXTENSIONS);
    if (!has_ext(ext, "EGL_EXT_platform_base") || !has_ext(ext, "EGL_KHR_platform_gbm")) {
        fprintf(stderr, "Missing client EGL_EXT_platform_base or EGL_KHR_platform_gbm\n"); goto out;
    }
    platform = (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!platform) { fprintf(stderr, "Missing eglGetPlatformDisplayEXT\n"); goto out; }
    dpy = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &major, &minor)) { fprintf(stderr, "EGL GBM initialize failed: 0x%x\n", eglGetError()); goto out; }
    initialized = 1;
    printf("EGL %d.%d vendor: %s\n", major, minor, eglQueryString(dpy, EGL_VENDOR));
    ext = eglQueryString(dpy, EGL_EXTENSIONS);
    if (!has_ext(ext, "EGL_KHR_image_base") || !has_ext(ext, "EGL_EXT_image_dma_buf_import") || !has_ext(ext, "EGL_EXT_image_dma_buf_import_modifiers")) {
        fprintf(stderr, "Missing EGL image, DMA-BUF import or modifier extension\n"); goto out;
    }
    surfaceless = has_ext(ext, "EGL_KHR_surfaceless_context");
    external_only = query_modifiers(dpy);
    if (external_only >= 0) mod_status = PASS;
    create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    bind_image = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (!create_image || !destroy_image || !bind_image) { fprintf(stderr, "Missing image entry point\n"); goto out; }
    const EGLint attrs[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    const EGLint pb[] = { EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE };
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!eglBindAPI(EGL_OPENGL_ES_API) || !eglChooseConfig(dpy, attrs, &config, 1, &count) || count != 1) { fprintf(stderr, "EGL config failed: 0x%x\n", eglGetError()); goto out; }
    if (!surfaceless) {
        surf = eglCreatePbufferSurface(dpy, config, pb);
        if (surf == EGL_NO_SURFACE) { fprintf(stderr, "EGL pbuffer failed: 0x%x\n", eglGetError()); goto out; }
    }
    ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, surf, surf, ctx)) { fprintf(stderr, "EGL context failed: 0x%x\n", eglGetError()); goto out; }
    printf("EGL context: %s\n", surfaceless ? "surfaceless" : "pbuffer");
    printf("GL renderer: %s\n", glGetString(GL_RENDERER));
    ext = (const char *)glGetString(GL_EXTENSIONS);
    if (!has_ext(ext, "GL_OES_EGL_image")) { fprintf(stderr, "Missing GL_OES_EGL_image\n"); goto out; }
    external_supported = has_ext(ext, "GL_OES_EGL_image_external");
    printf("GL_OES_EGL_image_external: %s\n", external_supported ? "supported" : "not supported");
    const EGLint ia[] = { EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_ARGB8888,
        EGL_DMA_BUF_PLANE0_FD_EXT, dma, EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)gbm_bo_get_offset(bo, 0),
        EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)gbm_bo_get_stride(bo),
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0, EGL_NONE };
    image = create_image(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, ia);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "DMA-BUF EGLImage import failed: 0x%x\n", eglGetError()); goto out; }
    puts("EGLImage: synthetic DMA-BUF import succeeded");
    imported = PASS;
    cuda_egl = try_cuda_egl(image, content_ready);
    if (external_supported) {
        GLenum err;
        gl_external = FAIL;
        glGenTextures(1, &external_tex);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, external_tex);
        err = glGetError();
        if (err != GL_NO_ERROR) fprintf(stderr, "GL external texture setup failed: 0x%x\n", err);
        else {
            bind_image(GL_TEXTURE_EXTERNAL_OES, image);
            err = glGetError();
            if (err != GL_NO_ERROR) fprintf(stderr, "GL external image binding failed: 0x%x\n", err);
            else { gl_external = PASS; puts("GL external image binding succeeded (sampling/content and CUDA-GL external texture interop not tested)"); }
        }
    }
    if (external_only == 1) {
        gl_2d = NOT_APPLICABLE;
        cuda_gl = NOT_APPLICABLE;
        puts("GL 2D / CUDA-GL: NOT_APPLICABLE for external-only modifier");
        goto out;
    }
    gl_2d = FAIL;
    gl = FAIL;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    { GLenum err = glGetError(); if (err != GL_NO_ERROR) { fprintf(stderr, "GL texture setup failed: 0x%x\n", err); goto out; } }
    bind_image(GL_TEXTURE_2D, image);
    { GLenum err = glGetError(); if (err != GL_NO_ERROR) { fprintf(stderr, "GL image binding failed: 0x%x\n", err); goto out; } }
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    if (glGetError() != GL_NO_ERROR || glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { fprintf(stderr, "Imported image framebuffer incomplete\n"); goto out; }
    glClearColor(1, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT); glFinish();
    glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, pixel);
    if (glGetError() != GL_NO_ERROR || pixel[0] != 255 || pixel[1] != 0 || pixel[2] != 0 || pixel[3] != 255) { fprintf(stderr, "GL readback failed: %u %u %u %u\n", pixel[0], pixel[1], pixel[2], pixel[3]); goto out; }
    puts("GL: imported texture framebuffer clear/readback passed");
    gl_2d = PASS; gl = PASS;
    cuda_gl = try_cuda(tex);
out:
    puts("LIMIT: synthetic buffer only; PipeWire import, cross-process/producer synchronization and zero-copy are not proven.");
    if (fbo) glDeleteFramebuffers(1, &fbo);
    if (tex) glDeleteTextures(1, &tex);
    if (external_tex) glDeleteTextures(1, &external_tex);
    if ((fbo || tex || external_tex) && glGetError() != GL_NO_ERROR) { fprintf(stderr, "GL cleanup failed\n"); cleanup = FAIL; }
    if (image != EGL_NO_IMAGE_KHR && destroy_image && !destroy_image(dpy, image)) { fprintf(stderr, "eglDestroyImageKHR failed: 0x%x\n", eglGetError()); cleanup = FAIL; }
    if (initialized) {
        printf("Cleanup before unbind: context current=%d draw current=%d read current=%d\n",
               eglGetCurrentContext() == ctx, eglGetCurrentSurface(EGL_DRAW) == surf,
               eglGetCurrentSurface(EGL_READ) == surf);
        if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT)) { fprintf(stderr, "eglMakeCurrent cleanup failed: 0x%x\n", eglGetError()); cleanup = FAIL; }
        printf("Cleanup after unbind: context=%p draw=%p read=%p (no-context=%d no-surfaces=%d)\n",
               (void *)eglGetCurrentContext(), (void *)eglGetCurrentSurface(EGL_DRAW),
               (void *)eglGetCurrentSurface(EGL_READ), eglGetCurrentContext() == EGL_NO_CONTEXT,
               eglGetCurrentSurface(EGL_DRAW) == EGL_NO_SURFACE && eglGetCurrentSurface(EGL_READ) == EGL_NO_SURFACE);
        if (surf != EGL_NO_SURFACE) {
            EGLBoolean ok; EGLint error, width = -1;
            (void)eglGetError();
            ok = eglQuerySurface(dpy, surf, EGL_WIDTH, &width);
            error = eglGetError();
            printf("eglQuerySurface after unbind: return=%u error=0x%x width=%d\n", ok, error, width);
            if (ok != EGL_TRUE || error != EGL_SUCCESS) cleanup = FAIL;
            (void)eglGetError(); /* Isolate the result from prior EGL calls. */
            ok = eglDestroySurface(dpy, surf);
            error = eglGetError();
            printf("eglDestroySurface(display, surface): return=%u error=0x%x\n", ok, error);
            if (ok != EGL_TRUE || error != EGL_SUCCESS) { fprintf(stderr, "eglDestroySurface anomalous result\n"); cleanup = FAIL; }
        }
        if (ctx != EGL_NO_CONTEXT && !eglDestroyContext(dpy, ctx)) { fprintf(stderr, "eglDestroyContext failed: 0x%x\n", eglGetError()); cleanup = FAIL; }
        if (!eglTerminate(dpy)) { fprintf(stderr, "eglTerminate failed: 0x%x\n", eglGetError()); cleanup = FAIL; }
    }
    if (dma >= 0 && close(dma)) { perror("close DMA-BUF"); cleanup = FAIL; }
    if (bo) gbm_bo_destroy(bo);
    if (gbm) gbm_device_destroy(gbm);
    if (fd >= 0 && close(fd)) { perror("close render node"); cleanup = FAIL; }
    printf("SUMMARY import=%s modifier=%s GL-external-bind=%s GL-2D=%s GL-content=%s CUDA-EGL=%s CUDA-GL=%s cleanup=%s\n",
           status_name(imported), status_name(mod_status), status_name(gl_external), status_name(gl_2d),
           status_name(gl), status_name(cuda_egl), status_name(cuda_gl), status_name(cleanup));
    int result = summary_exit(imported, mod_status, gl, gl_external, cuda_egl, cuda_gl, cleanup);
    printf("OVERALL %s (exit %d; 0 requires applicable content/map tests and cleanup to pass)\n",
           result == 0 ? "PASS" : result == 1 ? "FAIL" : "INCONCLUSIVE", result);
    return result;
}
