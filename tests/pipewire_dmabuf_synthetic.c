/* Private PipeWire DMA-BUF protocol probe; no pixels are accessed.
 * Default device is /dev/dri/renderD128; use a trusted local render node. */
#define _GNU_SOURCE
#include <pipewire/pipewire.h>
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

#define WIDTH 64
#define HEIGHT 64
#define TARGET_MOD UINT64_C(0x0300000000606010)
static uint64_t modifier = DRM_FORMAT_MOD_LINEAR;
struct endpoint { struct pw_thread_loop *loop; struct pw_stream *stream; int producer; };
struct allocation { struct gbm_bo *bo; int fd; dev_t dev; ino_t ino; struct allocation *next; };
static struct endpoint src, dst;
static struct gbm_device *gbm;
static int drmfd = -1;
static atomic_int failed, received, used_modifier_fallback, formats;
static struct pw_buffer *held;
/* Initialized before either PipeWire loop starts; immutable thereafter. */
static uint32_t stride, size;
static pthread_mutex_t allocations_lock = PTHREAD_MUTEX_INITIALIZER;
static struct allocation *allocations;

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

static struct allocation *allocate(uint32_t *out_stride, uint32_t *out_size) {
    struct allocation *a = calloc(1, sizeof(*a));
    if (!a) return NULL;
    a->fd = -1;
    const uint64_t linear = DRM_FORMAT_MOD_LINEAR;
    if (modifier != linear)
        a->bo = gbm_bo_create_with_modifiers(gbm, WIDTH, HEIGHT, GBM_FORMAT_ARGB8888, &modifier, 1);
    else
        a->bo = gbm_bo_create(gbm, WIDTH, HEIGHT, GBM_FORMAT_ARGB8888, GBM_BO_USE_LINEAR | GBM_BO_USE_RENDERING);
    if (!a->bo && modifier == linear) {
        a->bo = gbm_bo_create_with_modifiers(gbm, WIDTH, HEIGHT, GBM_FORMAT_ARGB8888, &linear, 1);
        if (a->bo && !atomic_exchange(&used_modifier_fallback, 1)) {
            puts("GBM: explicit linear modifier fallback succeeded"); fflush(stdout);
        }
    }
    struct stat st;
    if (!a->bo || gbm_bo_get_plane_count(a->bo) != 1 || gbm_bo_get_modifier(a->bo) != modifier ||
        gbm_bo_get_offset(a->bo, 0) != 0 || (a->fd = gbm_bo_get_fd(a->bo)) < 0 ||
        fstat(a->fd, &st) || st.st_size <= 0 || st.st_size > INT32_MAX ||
        gbm_bo_get_stride(a->bo) < WIDTH * 4 || gbm_bo_get_stride(a->bo) > INT32_MAX) {
        fprintf(stderr, "GBM allocation/export or layout validation failed\n");
        if (a->fd >= 0) close(a->fd);
        if (a->bo) gbm_bo_destroy(a->bo);
        free(a); return NULL;
    }
    a->dev = st.st_dev; a->ino = st.st_ino;
    *out_stride = gbm_bo_get_stride(a->bo); *out_size = (uint32_t)st.st_size;
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
    uint32_t actual_stride, actual_size;
    struct allocation *a = allocate(&actual_stride, &actual_size);
    if (!a) { atomic_store(&failed, 1); return; }
    if (actual_stride != stride || actual_size != size) {
        fprintf(stderr, "GBM allocation differs from negotiated stride or size\n");
        close(a->fd); gbm_bo_destroy(a->bo); free(a); atomic_store(&failed, 1); return;
    }
    pthread_mutex_lock(&allocations_lock);
    a->next = allocations; allocations = a;
    pthread_mutex_unlock(&allocations_lock);
    buffer->user_data = a;
    struct spa_data *d = &b->datas[0];
    d->type = SPA_DATA_DmaBuf; d->fd = a->fd; d->mapoffset = 0; d->maxsize = size; d->data = NULL;
    d->chunk->offset = 0; d->chunk->size = size; d->chunk->stride = (int32_t)stride;
}
static void remove_buffer(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (e->producer) {
        struct allocation *a = buffer->user_data;
        if (a) {
            pthread_mutex_lock(&allocations_lock);
            struct allocation **p = &allocations;
            while (*p && *p != a) p = &(*p)->next;
            if (*p) *p = a->next;
            pthread_mutex_unlock(&allocations_lock);
            close(a->fd); gbm_bo_destroy(a->bo); free(a); buffer->user_data = NULL;
        }
    } else if (held == buffer) { held = NULL; if (atomic_load(&received)) atomic_store(&failed, 1); }
}
static void process(void *data) {
    struct endpoint *e = data;
    if (!e->producer && held) return;
    struct pw_buffer *b = pw_stream_dequeue_buffer(e->stream);
    if (!b) return;
    if (e->producer) {
        if (!b->user_data) { atomic_store(&failed, 1); return; }
        struct spa_data *d = &b->buffer->datas[0];
        d->chunk->offset = 0; d->chunk->size = size; d->chunk->stride = (int32_t)stride;
    } else {
        struct stat st;
        if (!b->buffer || b->buffer->n_datas != 1 || !b->buffer->datas) goto bad;
        struct spa_data *d = &b->buffer->datas[0];
        if (d->type != SPA_DATA_DmaBuf || d->fd < 0 || fstat((int)d->fd, &st) || !d->chunk ||
            d->chunk->stride != (int32_t)stride || d->chunk->stride < WIDTH * 4 || d->chunk->offset != 0 ||
            d->chunk->size != size ||
            (st.st_size <= 0 || (uint64_t)st.st_size != d->chunk->size) || d->maxsize < d->chunk->size)
            goto bad;
        int match = 0;
        pthread_mutex_lock(&allocations_lock);
        for (struct allocation *a = allocations; a; a = a->next)
            if (a->dev == st.st_dev && a->ino == st.st_ino) { match = 1; break; }
        pthread_mutex_unlock(&allocations_lock);
        if (!match) { fprintf(stderr, "consumer DMA-BUF does not match a producer allocation\n"); goto bad; }
        if (atomic_load(&formats) != 3) { fprintf(stderr, "requested formats not both active at buffer observation\n"); goto bad; }
        held = b; atomic_store(&received, 1); return;
    }
    if (pw_stream_queue_buffer(e->stream, b) < 0) atomic_store(&failed, 1);
    return;
bad:
    fprintf(stderr, "invalid DMA-BUF descriptor, size or stride at consumer\n"); atomic_store(&failed, 1);
    pw_stream_queue_buffer(e->stream, b);
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
        info.modifier != modifier) {
        fprintf(stderr, "unexpected negotiated video format\n"); atomic_store(&failed, 1); return;
    }
    atomic_fetch_or(&formats, bit);
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *p = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_size, SPA_POD_Int((int32_t)size),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int32_t)stride),
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
        .modifier = modifier, .size = SPA_RECTANGLE(WIDTH, HEIGHT), .framerate = SPA_FRACTION(30, 1) };
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
    const char *device = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--modifier-nonlinear") && modifier == DRM_FORMAT_MOD_LINEAR) modifier = TARGET_MOD;
        else if (argv[i][0] != '-' && !device) device = argv[i];
        else { fprintf(stderr, "usage: %s [--modifier-nonlinear] [render-node]\n", argv[0]); return 2; }
    }
    if (!device) device = "/dev/dri/renderD128";
    drmfd = open_render_node(device);
    if (drmfd < 0) goto done;
    if (!(gbm = gbm_create_device(drmfd))) { fprintf(stderr, "GBM device creation failed\n"); goto done; }
    struct allocation *probe = allocate(&stride, &size);
    if (!probe) goto done;
    printf("GBM negotiated layout: stride=%u size=%u\n", stride, size);
    close(probe->fd); gbm_bo_destroy(probe->bo); free(probe);
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
    else if (atomic_load(&formats) != 3) fprintf(stderr, "both requested formats were not negotiated\n");
    else if (!held) fprintf(stderr, "held buffer disappeared\n");
    else rc = 0;
    /* Destruction ends the outstanding lease without queueing it. */
    atomic_store(&received, 0);
cleanup:
    if (started_src) pw_thread_loop_stop(src.loop);
    if (started_dst) pw_thread_loop_stop(dst.loop);
    if (src.stream) pw_stream_destroy(src.stream);
    if (dst.stream) pw_stream_destroy(dst.stream);
    if (src.loop) pw_thread_loop_destroy(src.loop);
    if (dst.loop) pw_thread_loop_destroy(dst.loop);
    pw_deinit();
    if (atomic_load(&failed)) rc = 1;
    if (rc == 0) printf("PASS: observed one producer DMA-BUF identity match with modifier 0x%016llx on both formats; no pixels accessed. "
                        "No remote-error roundtrip barrier: later or pending events and long-term validity are not tested.\n", (unsigned long long)modifier);
done:
    if (gbm) gbm_device_destroy(gbm);
    if (drmfd >= 0) close(drmfd);
    return rc;
}
