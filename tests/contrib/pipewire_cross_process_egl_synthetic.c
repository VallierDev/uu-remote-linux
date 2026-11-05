/* Independent PipeWire clients; only DMA-BUF metadata crosses the control socket. */
#define main old_main
#include "../pipewire_dmabuf_synthetic.c"
#undef main
#include <sys/socket.h>
#include <errno.h>
#include <poll.h>
#include <time.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include "../../c_engine/portal_nvenc_egl_attrs.h"

static EGLDisplay display;
static EGLConfig config;
static PFNEGLCREATEIMAGEKHRPROC create_image;
static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;

/* Only these self-created synthetic buffers are tested. Unknown completion is
 * not safe for a real Portal lease, and process exit/timeout may release BOs. */
enum pixel_result { PIXEL_OK, PIXEL_FAILED_FINISHED, PIXEL_UNKNOWN };
static atomic_int consumer_result;
static int force_mismatch;
static int gl_errors(int *unknown) {
    int bad = 0;
    for (int i = 0; i < 32; i++) {
        GLenum error = glGetError();
        if (error == GL_NO_ERROR) return bad;
        fprintf(stderr, "GL operation error 0x%x\n", error);
        bad = 1;
        if (error == 0x0507 /* GL_CONTEXT_LOST */) *unknown = 1;
    }
    *unknown = 1;
    return 1;
}
static enum pixel_result pixels(const struct spa_video_info_raw *format, const struct spa_data *data, int producer) {
    struct pnspa_snapshot s = { .format = format->format, .width = format->size.width,
        .height = format->size.height, .modifier = format->modifier, .pixel_fd = (int)data->fd,
        .mapoffset = data->mapoffset, .offset = data->chunk->offset, .stride = data->chunk->stride };
    EGLint attrs[PNEGL_ATTR_COUNT];
    const EGLint ca[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    if (!pnegl_attrs(&s, attrs) || !eglBindAPI(EGL_OPENGL_ES_API)) return PIXEL_FAILED_FINISHED;
    EGLContext ctx = eglCreateContext(display, config, EGL_NO_CONTEXT, ca);
    if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx))
        return PIXEL_FAILED_FINISHED;
    EGLImageKHR image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attrs);
    if (image == EGL_NO_IMAGE_KHR) { fprintf(stderr, "EGL DMA-BUF import error 0x%x\n", eglGetError()); return PIXEL_FAILED_FINISHED; }
    GLuint tex, fbo;
    glGenTextures(1, &tex); glBindTexture(GL_TEXTURE_2D, tex);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    bind_image(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &fbo); glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    int unknown = 0;
    int bad = status != GL_FRAMEBUFFER_COMPLETE;
    if (bad) fprintf(stderr, "EGL image framebuffer status=0x%x\n", status);
    bad |= gl_errors(&unknown);
    if (!bad && producer) {
        glEnable(GL_SCISSOR_TEST);
        const GLfloat colors[4][3] = {{1,0,0},{0,1,0},{0,0,1},{1,1,1}};
        for (int q = 0; q < 4; q++) {
            glScissor((q % 2) * WIDTH / 2, (q / 2) * HEIGHT / 2, WIDTH / 2, HEIGHT / 2);
            glClearColor(colors[q][0], colors[q][1], colors[q][2], 1);
            glClear(GL_COLOR_BUFFER_BIT);
        }
    } else if (!bad) {
        const unsigned char expected[4][3] = {{255,0,0},{0,255,0},{0,0,255},{255,255,255}};
        for (int q = 0; q < 4; q++) {
            unsigned char p[4] = {0};
            glReadPixels((q % 2) * WIDTH / 2 + WIDTH / 4, (q / 2) * HEIGHT / 2 + HEIGHT / 4,
                         1, 1, GL_RGBA, GL_UNSIGNED_BYTE, p);
            if (gl_errors(&unknown)) bad = 1;
            if (memcmp(p, expected[q], 3)) bad = 1;
        }
        /* Inject only after all genuine samples passed and had no GL errors. */
        if (!bad && !unknown && force_mismatch) {
            fprintf(stderr, "INJECTED_MISMATCH_AFTER_VALID_SAMPLES\n");
            bad = 1;
        }
    }
    /* Remember all prior errors; draining them never turns failure into success. */
    bad |= gl_errors(&unknown);
    glFinish();
    if (glGetError() != GL_NO_ERROR) unknown = 1;
    if (unknown) {
        fprintf(stderr, "GPU completion unknown; BO retention after process exit/timeout is not proven (synthetic only)\n");
        return PIXEL_UNKNOWN;
    }
    if (bad) fprintf(stderr, "GL verification failed; glFinish completed without error\n");
    return bad ? PIXEL_FAILED_FINISHED : PIXEL_OK;
}

struct record { uint64_t pid, dev, ino, size, stride; };
static int control_fd;
static atomic_int queued, queue_succeeded;
static pid_t producer_pid;
static int wait_for_done(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
    int64_t deadline = (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000 + 15000;
    for (;;) {
        if (clock_gettime(CLOCK_MONOTONIC, &now)) return -1;
        int64_t remaining = deadline - ((int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
        if (remaining <= 0) return -1;
        struct pollfd p = { .fd = control_fd, .events = POLLIN | POLLRDHUP };
        int n = poll(&p, 1, (int)remaining);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0 || (p.revents & (POLLERR | POLLNVAL))) return -1;
        char message[4];
        ssize_t received_bytes = recv(control_fd, message, sizeof(message), MSG_DONTWAIT | MSG_TRUNC);
        if (received_bytes == 4 && !memcmp(message, "DONE", 4)) return 0;
        if (received_bytes < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) continue;
        return -1;
    }
}
static int receive_record(struct record *r) {
    char ancillary[CMSG_SPACE(sizeof(struct ucred))];
    struct iovec iov = { .iov_base = r, .iov_len = sizeof(*r) };
    struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1,
                          .msg_control = ancillary, .msg_controllen = sizeof(ancillary) };
    ssize_t n = recvmsg(control_fd, &msg, MSG_DONTWAIT | MSG_TRUNC);
    if (n != sizeof(*r) || (msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC))) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    if (!c || c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_CREDENTIALS ||
        c->cmsg_len != CMSG_LEN(sizeof(struct ucred)) || CMSG_NXTHDR(&msg, c)) return -1;
    struct ucred cred;
    memcpy(&cred, CMSG_DATA(c), sizeof(cred));
    return cred.pid == producer_pid && r->pid == (uint64_t)producer_pid && producer_pid != getpid() ? 0 : -1;
}
static void cross_remove(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer && held == buffer) atomic_store(&failed, 1);
    /* Producer BOs deliberately remain alive until process exit. */
}
static void cross_process(void *data) {
    struct endpoint *e = data;
    if (e->producer && atomic_load(&queued)) return;
    if (!e->producer && held) return;
    struct pw_buffer *b = pw_stream_dequeue_buffer(e->stream);
    if (!b) return;
    if (e->producer) {
        struct allocation *a = b->user_data;
        if (!a || atomic_load(&formats) != 1) goto bad;
        struct spa_video_info_raw format = { .format = SPA_VIDEO_FORMAT_BGRA,
            .size = SPA_RECTANGLE(WIDTH, HEIGHT), .modifier = modifier };
        struct spa_data *d = &b->buffer->datas[0];
        if (pixels(&format, d, 1)) goto bad;
        struct record r = { (uint64_t)getpid(), (uint64_t)a->dev, (uint64_t)a->ino, size, stride };
        d->chunk->offset = 0; d->chunk->size = size; d->chunk->stride = (int32_t)stride;
        if (send(control_fd, &r, sizeof(r), MSG_NOSIGNAL) != sizeof(r)) goto bad;
        atomic_store(&queued, 1);
        if (pw_stream_queue_buffer(e->stream, b) < 0) goto bad;
        atomic_store(&queue_succeeded, 1);
    } else {
        struct stat st; struct record r;
        if (!b->buffer || b->buffer->n_datas != 1 || !b->buffer->datas) goto bad;
        struct spa_data *d = &b->buffer->datas[0];
        if (d->type != SPA_DATA_DmaBuf || d->fd < 0 || fstat((int)d->fd, &st) || !d->chunk ||
            d->mapoffset != 0 || d->chunk->offset != 0 || d->chunk->size != size ||
            d->chunk->stride != (int32_t)stride || d->maxsize < size || st.st_size != size ||
            atomic_load(&formats) != 2) goto bad;
        if (receive_record(&r) < 0 || r.dev != (uint64_t)st.st_dev ||
            r.ino != (uint64_t)st.st_ino || r.size != size || r.stride != stride) goto bad;
        held = b;
        struct spa_video_info_raw format = { .format = SPA_VIDEO_FORMAT_BGRA,
            .size = SPA_RECTANGLE(WIDTH, HEIGHT), .modifier = modifier };
        enum pixel_result result = pixels(&format, d, 0);
        atomic_store(&consumer_result, result);
        if (result == PIXEL_UNKNOWN) goto bad;
        atomic_store(&received, 1);
    }
    return;
bad:
    fprintf(stderr, "cross-process buffer/record validation failed\n");
    atomic_store(&failed, 1);
}
static const struct pw_stream_events cross_events = { PW_VERSION_STREAM_EVENTS,
    .state_changed = state, .param_changed = param_changed, .add_buffer = add_buffer,
    .remove_buffer = cross_remove, .process = cross_process };
static int cross_setup(struct endpoint *e, const char *name, int producer) {
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA, .flags = SPA_VIDEO_FLAG_MODIFIER,
        .modifier = modifier, .size = SPA_RECTANGLE(WIDTH, HEIGHT), .framerate = SPA_FRACTION(30, 1) };
    e->producer = producer; e->loop = pw_thread_loop_new(name, NULL);
    if (!e->loop) return -1;
    e->stream = pw_stream_new_simple(pw_thread_loop_get_loop(e->loop), name,
        pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, producer ? "Capture" : "Playback",
                          PW_KEY_MEDIA_ROLE, "Test", PW_KEY_NODE_NAME, name, PW_KEY_NODE_AUTOCONNECT, "false", NULL), &cross_events, e);
    if (!e->stream) return -1;
    const struct spa_pod *p = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    if (pw_stream_connect(e->stream, producer ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT, PW_ID_ANY,
        producer ? PW_STREAM_FLAG_ALLOC_BUFFERS : PW_STREAM_FLAG_NONE, &p, 1) < 0) return -1;
    return pw_thread_loop_start(e->loop);
}
int main(int argc, char **argv) {
    if (argc != 6 || (strcmp(argv[1], "producer") && strcmp(argv[1], "consumer"))) return 2;
    char *end; long fd = strtol(argv[2], &end, 10);
    if (*end || fd < 0 || fd > INT_MAX) return 2;
    control_fd = (int)fd;
    int producer = !strcmp(argv[1], "producer"), started = 0, rc = 1, got_ack = 0;
    force_mismatch = !producer && getenv("UU_PW_EGL_FORCE_MISMATCH") != NULL;
    long pid = strtol(argv[5], &end, 10);
    if (*end || pid < 0 || pid > INT_MAX || (!producer && pid == 0)) return 2;
    producer_pid = (pid_t)pid;
    if (!producer) {
        int enabled = 0; socklen_t len = sizeof(enabled);
        if (getsockopt(control_fd, SOL_SOCKET, SO_PASSCRED, &enabled, &len) || !enabled) return 2;
    }
    struct endpoint e = {0}; modifier = TARGET_MOD;
    if ((drmfd = open_render_node(argv[3])) < 0 || !(gbm = gbm_create_device(drmfd))) goto done;
    struct allocation *probe = allocate(&stride, &size);
    if (!probe) goto done;
    close(probe->fd); gbm_bo_destroy(probe->bo); free(probe);
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
    if (cross_setup(&e, argv[4], producer) < 0) goto cleanup;
    started = 1;
    for (int i = 0; i < 1500 && !atomic_load(&failed); i++) {
        if (!producer && atomic_load(&received)) break;
        if (producer) {
            if (!got_ack) {
                char ack[4]; ssize_t n = recv(control_fd, ack, sizeof(ack), MSG_DONTWAIT | MSG_TRUNC);
                if (n >= 0) {
                    if (n != 3 || (memcmp(ack, "ACK", 3) && memcmp(ack, "ERR", 3)) || !atomic_load(&queued)) {
                        fprintf(stderr, "unexpected ACK or peer closure\n"); break;
                    }
                    got_ack = !memcmp(ack, "ERR", 3) ? 2 : 1;
                } else if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) break;
            }
            if (got_ack && atomic_load(&queue_succeeded)) break;
        }
        usleep(10000);
    }
    pw_thread_loop_stop(e.loop); started = 0;
    if (producer && !atomic_load(&failed) && got_ack && atomic_load(&queue_succeeded) && atomic_load(&formats) == 1) {
        if (send(control_fd, "DONE", 4, MSG_NOSIGNAL) == 4) {
            if (got_ack == 1) rc = 0;
            else fprintf(stderr, "producer received finished-failure ERR; DONE sent\n");
        }
    } else if (!producer && !atomic_load(&failed) && atomic_load(&received) && held && atomic_load(&formats) == 2) {
        int pixel_failed = atomic_load(&consumer_result) == PIXEL_FAILED_FINISHED;
        const char *message = pixel_failed ? "ERR" : "ACK";
        if (send(control_fd, message, 3, MSG_NOSIGNAL) == 3 && wait_for_done() == 0) {
            if (!pixel_failed) rc = 0;
            else fprintf(stderr, "consumer sent finished-failure ERR; DONE received\n");
        }
    }
    if (atomic_load(&failed)) rc = 1;
cleanup:
    if (started) pw_thread_loop_stop(e.loop);
    /* Do not destroy streams/BOs: process exit releases them after the ACK. */
done:
    if (rc) fprintf(stderr, "%s FAIL\n", argv[1]);
    close(control_fd);
    return rc;
}
