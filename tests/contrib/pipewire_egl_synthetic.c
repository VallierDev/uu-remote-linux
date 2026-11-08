/* Private synthetic GBM -> PipeWire -> EGL offscreen pixel test. No capture.
 * glFinish is the synthetic producer's completion prerequisite, not a Portal fence. */
#define _GNU_SOURCE
#include <pipewire/pipewire.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include "../../c_engine/portal_nvenc_egl_attrs.h"
#include <spa/param/video/format-utils.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include <limits.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#define WIDTH 256
#define HEIGHT 256
#define TARGET_MOD UINT64_C(0x0300000000606010)
struct endpoint { struct pw_thread_loop *loop; struct pw_stream *stream; int producer; struct spa_video_info_raw format; };
struct allocation { struct gbm_bo *bo; int fd; dev_t dev; ino_t ino; uint32_t stride, size, offset; struct allocation *next; };
static struct endpoint src, dst;
static struct gbm_device *gbm;
static int drmfd = -1;
static atomic_int failed, received, formats;
static struct pw_buffer *held;
static atomic_int sent, queue_succeeded;
static _Atomic(struct allocation *) rendered;
static EGLDisplay display;
static EGLConfig config;
static PFNEGLCREATEIMAGEKHRPROC create_image;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;

/* Keep our objects alive until process exit, including on errors. This does
 * not prevent PipeWire revocation or establish a real producer's lease. */
static int pixels(const struct spa_video_info_raw *format, const struct spa_data *data, int producer) {
    struct pnspa_snapshot s = { .format = format->format, .width = format->size.width,
        .height = format->size.height, .modifier = format->modifier, .pixel_fd = (int)data->fd,
        .mapoffset = data->mapoffset, .offset = data->chunk->offset, .stride = data->chunk->stride };
    EGLint attrs[PNEGL_ATTR_COUNT];
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!pnegl_attrs(&s, attrs) || !eglBindAPI(EGL_OPENGL_ES_API)) return -1;
    EGLContext ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) return -1;
    EGLImageKHR image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "EGL DMA-BUF import error 0x%x\n", eglGetError()); return -1; }
    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    bind_image(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum error = glGetError(), status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (error != GL_NO_ERROR || status != GL_FRAMEBUFFER_COMPLETE) {
        fprintf(stderr, "EGL image framebuffer error=0x%x status=0x%x\n", error, status); return -1;
    }
    if (producer) {
        glEnable(GL_SCISSOR_TEST);
        const GLfloat colors[4][3] = {{1,0,0},{0,1,0},{0,0,1},{1,1,1}};
        for (int q = 0; q < 4; q++) {
            glScissor((q % 2) * WIDTH / 2, (q / 2) * HEIGHT / 2, WIDTH / 2, HEIGHT / 2);
            glClearColor(colors[q][0], colors[q][1], colors[q][2], 1);
            glClear(GL_COLOR_BUFFER_BIT);
        }
        glFinish(); /* Must complete before this producer queues the lease. */
        if (glGetError() != GL_NO_ERROR) return -1;
    } else {
        const unsigned char expected[4][3] = {{255,0,0},{0,255,0},{0,0,255},{255,255,255}};
        for (int q = 0; q < 4; q++) {
            unsigned char p[4];
            glReadPixels((q % 2) * WIDTH / 2 + WIDTH / 4, (q / 2) * HEIGHT / 2 + HEIGHT / 4,
                         1, 1, GL_RGBA, GL_UNSIGNED_BYTE, p);
            if (glGetError() != GL_NO_ERROR || memcmp(p, expected[q], 3)) return -1;
        }
        glFinish();
        if (glGetError() != GL_NO_ERROR) return -1;
    }
    return 0;
}
static pthread_mutex_t allocations_lock = PTHREAD_MUTEX_INITIALIZER;
static struct allocation *allocations;
/* Immutable once measured, before either PipeWire stream is created. */
static struct allocation *layout;

/* Pin before opening for I/O. Privileged changes to /dev or sysfs are outside
 * the threat model; the caller must use a trusted local render device. */
static int open_render_node(const char *path) {
    const char prefix[] = "/dev/dri/renderD";
    if (strncmp(path, prefix, sizeof(prefix) - 1)) goto invalid;
    const char *digits = path + sizeof(prefix) - 1;
    if (!*digits || strspn(digits, "0123456789") != strlen(digits)) goto invalid;
    int pin = open(path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
    if (pin < 0) { perror("pin render node"); return -1; }
    int fd = -1; struct stat before, after;
    char sys[128], resolved[PATH_MAX], subsystem[PATH_MAX], proc[64];
    if (fstat(pin, &before) || !S_ISCHR(before.st_mode)) { fprintf(stderr, "not a character device\n"); goto out; }
    snprintf(sys, sizeof(sys), "/sys/dev/char/%u:%u", major(before.st_rdev), minor(before.st_rdev));
    if (!realpath(sys, resolved)) { perror("resolve render sysfs"); goto out; }
    const char *base = strrchr(resolved, '/');
    if (!base || strcmp(base + 1, path + sizeof("/dev/dri/") - 1)) { fprintf(stderr, "render sysfs name mismatch\n"); goto out; }
    size_t len = strlen(sys);
    if (len + sizeof("/subsystem") > sizeof(sys)) { fprintf(stderr, "sysfs path too long\n"); goto out; }
    memcpy(sys + len, "/subsystem", sizeof("/subsystem"));
    if (!realpath(sys, subsystem) || strcmp(subsystem, "/sys/class/drm")) { fprintf(stderr, "render sysfs subsystem is not DRM\n"); goto out; }
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", pin);
    fd = open(proc, O_RDWR | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) { perror("open pinned render node"); goto out; }
    if (fstat(fd, &after) || !S_ISCHR(after.st_mode) || before.st_dev != after.st_dev ||
        before.st_ino != after.st_ino || before.st_rdev != after.st_rdev || drmGetNodeTypeFromFd(fd) != DRM_NODE_RENDER) {
        fprintf(stderr, "opened render node identity/type mismatch\n"); close(fd); fd = -1;
    }
out:
    close(pin);
    return fd;
invalid:
    fprintf(stderr, "refusing device: expected /dev/dri/renderD<digits> (no symlinks)\n");
    return -1;
}

/* Measure each actual exported allocation; no tiled size is inferred from pitch. */
static struct allocation *allocate(void) {
    struct allocation *a = calloc(1, sizeof(*a));
    if (!a) return NULL;
    a->fd = -1;
    const uint64_t modifier = TARGET_MOD;
    a->bo = gbm_bo_create_with_modifiers(gbm, WIDTH, HEIGHT, GBM_FORMAT_ARGB8888, &modifier, 1);
    if (!a->bo || gbm_bo_get_width(a->bo) != WIDTH || gbm_bo_get_height(a->bo) != HEIGHT ||
        gbm_bo_get_plane_count(a->bo) != 1 || gbm_bo_get_modifier(a->bo) != modifier ||
        (a->fd = gbm_bo_get_fd(a->bo)) < 0) {
        fprintf(stderr, "GBM tiled single-plane allocation/export failed or returned unexpected layout\n");
        if (a->bo) gbm_bo_destroy(a->bo);
        free(a); return NULL;
    }
    struct stat st;
    /* ParamBuffers size and stride are signed SPA integers. */
    if (fstat(a->fd, &st) || st.st_size <= 0 || (uint64_t)st.st_size > INT32_MAX) {
        fprintf(stderr, "invalid producer DMA-BUF fstat size\n"); close(a->fd); gbm_bo_destroy(a->bo); free(a); return NULL;
    }
    a->dev = st.st_dev; a->ino = st.st_ino;
    a->size = (uint32_t)st.st_size; a->stride = gbm_bo_get_stride(a->bo); a->offset = gbm_bo_get_offset(a->bo, 0);
    if (a->stride < WIDTH * 4 || a->stride > INT32_MAX || a->offset >= a->size || a->offset > INT32_MAX) {
        fprintf(stderr, "unexpected GBM plane layout\n"); close(a->fd); gbm_bo_destroy(a->bo); free(a); return NULL;
    }
    return a;
}

static void add_buffer(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer) return;
    struct spa_buffer *b = buffer->buffer;
    if (!b || b->n_datas != 1 || !b->datas || !b->datas[0].chunk ||
        !(b->datas[0].type & (1u << SPA_DATA_DmaBuf))) {
        fprintf(stderr, "allocator was not offered single-plane DMA-BUF\n");
        atomic_store(&failed, 1); return;
    }
    struct allocation *a = allocate();
    if (!a) { atomic_store(&failed, 1); return; }
    if (a->size != layout->size || a->stride != layout->stride || a->offset != layout->offset) {
        fprintf(stderr, "GBM allocation differs from advertised tiled layout\n");
        close(a->fd); gbm_bo_destroy(a->bo); free(a); atomic_store(&failed, 1); return;
    }
    pthread_mutex_lock(&allocations_lock);
    a->next = allocations; allocations = a;
    pthread_mutex_unlock(&allocations_lock);
    buffer->user_data = a;
    struct spa_data *d = &b->datas[0];
    d->type = SPA_DATA_DmaBuf; d->fd = a->fd; d->mapoffset = 0; d->maxsize = a->size; d->data = NULL;
    d->chunk->offset = a->offset; d->chunk->size = a->size - a->offset; d->chunk->stride = (int32_t)a->stride;
}
static void remove_buffer(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    (void)buffer;
    /* Revocation is failure; never destroy allocations or GPU objects here. */
    if (e->producer || held) atomic_store(&failed, 1);
}
static void process(void *data) {
    struct endpoint *e = data;
    if (atomic_load(&failed) || (!e->producer && held) || (e->producer && atomic_load(&sent))) return;
    struct pw_buffer *b = pw_stream_dequeue_buffer(e->stream);
    if (!b) return;
    if (e->producer) {
        if (!b->user_data) { atomic_store(&failed, 1); return; }
        struct spa_data *d = &b->buffer->datas[0];
        if (pixels(&e->format, d, 1)) {
            fprintf(stderr, "producer EGL rendering/completion failed; lease retained\n"); atomic_store(&failed, 1); return;
        }
        /* Publish the completed allocation before queue: delivery can race its return. */
        atomic_store(&rendered, b->user_data);
        atomic_store(&sent, 1);
    } else {
        struct stat st;
        if (!b->buffer || b->buffer->n_datas != 1 || !b->buffer->datas) goto bad;
        struct spa_data *d = &b->buffer->datas[0];
        if (d->type != SPA_DATA_DmaBuf || d->fd < 0 || fstat((int)d->fd, &st) || !d->chunk ||
            st.st_size <= 0 || (uint64_t)st.st_size > UINT32_MAX || d->mapoffset != 0 ||
            d->maxsize != (uint32_t)st.st_size || d->chunk->offset >= d->maxsize ||
            d->chunk->size != d->maxsize - d->chunk->offset || d->chunk->stride < WIDTH * 4)
            goto bad;
        struct allocation *a = atomic_load(&rendered);
        if (!a || a->dev != st.st_dev || a->ino != st.st_ino || a->size != d->maxsize ||
            a->offset != d->chunk->offset || a->stride != (uint32_t)d->chunk->stride) {
            fprintf(stderr, "consumer DMA-BUF does not match the completed producer allocation\n"); goto bad;
        }
        if (atomic_load(&formats) != 3) { fprintf(stderr, "tiled formats not both active at buffer observation\n"); goto bad; }
        held = b;
        if (pixels(&e->format, d, 0)) {
            fprintf(stderr, "consumer EGL import or pixel verification failed; lease retained\n"); atomic_store(&failed, 1); return;
        }
        atomic_store(&received, 1); return;
    }
    if (pw_stream_queue_buffer(e->stream, b) < 0) atomic_store(&failed, 1);
    else atomic_store(&queue_succeeded, 1);
    return;
bad:
    fprintf(stderr, "invalid DMA-BUF descriptor, size or stride at consumer\n"); atomic_store(&failed, 1);
    /* On any failure retain the consumer lease until process exit. */
}
static void param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    struct endpoint *e = data;
    if (id != SPA_PARAM_Format) return;
    int bit = e->producer ? 1 : 2;
    if (!param) { atomic_fetch_and(&formats, ~bit); return; }
    atomic_fetch_and(&formats, ~bit);
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0 || info.format != SPA_VIDEO_FORMAT_BGRA ||
        info.size.width != WIDTH || info.size.height != HEIGHT || info.framerate.num != 30 || info.framerate.denom != 1 ||
        !(info.flags & SPA_VIDEO_FLAG_MODIFIER) || (info.flags & SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED) ||
        info.modifier != TARGET_MOD) {
        fprintf(stderr, "unexpected negotiated video format\n"); atomic_store(&failed, 1); return;
    }
    e->format = info;
    atomic_fetch_or(&formats, bit);
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *p = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_size, SPA_POD_Int((int32_t)layout->size),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int32_t)layout->stride),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf));
    if (pw_stream_update_params(e->stream, &p, 1) < 0) atomic_store(&failed, 1);
}
static void state(void *data, enum pw_stream_state old, enum pw_stream_state now, const char *error) {
    (void)data; (void)old;
    if (now == PW_STREAM_STATE_ERROR) { fprintf(stderr, "stream error: %s\n", error ? error : "unknown"); atomic_store(&failed, 1); }
}
static const struct pw_stream_events events = { PW_VERSION_STREAM_EVENTS, .state_changed = state, .param_changed = param_changed, .add_buffer = add_buffer, .remove_buffer = remove_buffer, .process = process };
static int setup(struct endpoint *e, const char *name, int producer) {
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA, .flags = SPA_VIDEO_FLAG_MODIFIER,
        .modifier = TARGET_MOD, .size = SPA_RECTANGLE(WIDTH, HEIGHT), .framerate = SPA_FRACTION(30, 1) };
    e->producer = producer; e->loop = pw_thread_loop_new(name, NULL);
    if (!e->loop) return -1;
    e->stream = pw_stream_new_simple(pw_thread_loop_get_loop(e->loop), name,
        pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, producer ? "Capture" : "Playback",
                          PW_KEY_MEDIA_ROLE, "Test", PW_KEY_NODE_NAME, name, PW_KEY_NODE_AUTOCONNECT, "false", NULL), &events, e);
    if (!e->stream) return -1;
    const struct spa_pod *p = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    if (pw_stream_connect(e->stream, producer ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT, PW_ID_ANY,
        producer ? PW_STREAM_FLAG_ALLOC_BUFFERS : PW_STREAM_FLAG_NONE, &p, 1) < 0) return -1;
    return pw_thread_loop_start(e->loop);
}
int main(int argc, char **argv) {
    int rc = 1, started_src = 0, started_dst = 0;
    if (argc > 2) { fprintf(stderr, "usage: %s [render-node]\n", argv[0]); return 2; }
    const char *device = argc == 2 ? argv[1] : "/dev/dri/renderD128";
    drmfd = open_render_node(device);
    if (drmfd < 0) goto done;
    if (!(gbm = gbm_create_device(drmfd))) { fprintf(stderr, "GBM device creation failed\n"); goto done; }
    layout = allocate();
    if (!layout) { fprintf(stderr, "pre-negotiation GBM layout allocation failed\n"); goto done; }
    printf("GBM preallocated tiled layout: size=%u stride=%u offset=%u\n", layout->size, layout->stride, layout->offset);
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
    bind_image = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    EGLint n = 0;
    const EGLint cfg[] = { EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
                           EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE };
    if (!platform || !create_image || !bind_image ||
        (display = platform(EGL_PLATFORM_GBM_KHR, gbm, NULL)) == EGL_NO_DISPLAY ||
        !eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API) ||
        !eglChooseConfig(display, cfg, &config, 1, &n) || n != 1) {
        fprintf(stderr, "EGL GBM initialization failed\n"); goto done;
    }
    pw_init(NULL, NULL);
    if (setup(&src, "uu-dmabuf-producer", 1) < 0) { fprintf(stderr, "producer setup failed\n"); goto cleanup; }
    started_src = 1;
    if (setup(&dst, "uu-dmabuf-consumer", 0) < 0) { fprintf(stderr, "consumer setup failed\n"); goto cleanup; }
    started_dst = 1;
    puts("READY"); fflush(stdout);
    for (int i = 0; i < 500 && !atomic_load(&received) && !atomic_load(&failed); i++) usleep(10000);
    /* Stop both callback threads before inspecting the lease or declaring success. */
    pw_thread_loop_stop(dst.loop); started_dst = 0;
    pw_thread_loop_stop(src.loop); started_src = 0;
    if (atomic_load(&failed)) fprintf(stderr, "DMA-BUF probe failed\n");
    else if (!atomic_load(&received)) fprintf(stderr, "DMA-BUF negotiation or delivery timed out\n");
    else if (!atomic_load(&queue_succeeded)) fprintf(stderr, "producer queue did not succeed\n");
    else if (atomic_load(&formats) != 3) fprintf(stderr, "both tiled formats were not negotiated\n");
    else if (!held) fprintf(stderr, "held buffer disappeared\n");
    else rc = 0;
cleanup:
    if (started_src) pw_thread_loop_stop(src.loop);
    if (started_dst) pw_thread_loop_stop(dst.loop);
    if (atomic_load(&failed)) rc = 1;
    if (!rc) puts("OBSERVED: private PipeWire DMA-BUF modifier 0x0300000000606010 imported from received fd; four GL pattern points verified offscreen after producer glFinish. No CUDA/NVENC or Portal tested.");
done:
    /* Do not tear down a potentially in-flight GPU or buffer lease. */
    fflush(NULL);
    _exit(rc);
}
