/* Independent PipeWire clients; only DMA-BUF metadata crosses the control socket. */
#define main old_main
#include "../pipewire_dmabuf_synthetic.c"
#undef main
#include <sys/socket.h>
#include <errno.h>
#include <poll.h>
#include <time.h>
#include "../../c_engine/portal_nvenc_spa.c"
#include "../../c_engine/portal_nvenc_timeline.c"
/* Synthetic premise: no GPU access and exclusive system-wide signalers. This
 * is not a proof of Portal lease safety or exclusivity against other clients. */
static uint32_t producer_handle, acquire_check;
static int consumer_drm = -1;
static struct pnt_ctx *owner;
static struct spa_video_info_raw fixed_format;
static uint64_t sentinel = UINT64_C(0x4000000000000123);
struct sync_allocation { struct allocation *allocation; uint32_t handle; int fd; };


struct record { uint64_t pid, dev, ino, size, stride, modifier, format, width, height; };
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
static void sync_add(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer) return;
    struct spa_buffer *b = buffer->buffer;
    struct spa_meta_sync_timeline *m;
    if (!b || b->n_datas != 3 || !b->datas || !b->datas[0].chunk ||
        !(b->datas[0].type & (1u << SPA_DATA_DmaBuf)) ||
        !(m = spa_buffer_find_meta_data(b, SPA_META_SyncTimeline, sizeof(*m)))) goto bad;
    uint32_t s, z;
    struct sync_allocation *x = calloc(1, sizeof(*x));
    if (!x || !(x->allocation = allocate(&s, &z)) || s != stride || z != size ||
        drmSyncobjCreate(drmfd, 0, &x->handle) || drmSyncobjHandleToFD(drmfd, x->handle, &x->fd)) goto bad;
    buffer->user_data = x;
    struct spa_data *d = &b->datas[0];
    d->type = SPA_DATA_DmaBuf; d->flags = SPA_DATA_FLAG_READABLE; d->fd = x->allocation->fd;
    d->mapoffset = 0; d->maxsize = size; d->data = NULL;
    d->chunk->offset = 0; d->chunk->size = size; d->chunk->stride = (int32_t)stride; d->chunk->flags = 0;
    for (unsigned i = 1; i < 3; i++) {
        b->datas[i].type = SPA_DATA_SyncObj; b->datas[i].flags = SPA_DATA_FLAG_READABLE; b->datas[i].fd = x->fd;
    }
    *m = (struct spa_meta_sync_timeline){ .acquire_point = 1, .release_point = 2,
        .flags = SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE };
    return;
bad: atomic_store(&failed, 1);
}
static void sync_param(void *data, uint32_t id, const struct spa_pod *param) {
    struct endpoint *e = data;
    if (id != SPA_PARAM_Format) return;
    int bit = e->producer ? 1 : 2;
    atomic_fetch_and(&formats, ~bit);
    if (!param) return;
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0 || info.format != SPA_VIDEO_FORMAT_BGRA ||
        info.size.width != WIDTH || info.size.height != HEIGHT || info.framerate.num != 30 || info.framerate.denom != 1 ||
        !(info.flags & SPA_VIDEO_FLAG_MODIFIER) || (info.flags & SPA_VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED) || info.modifier != modifier) {
        atomic_store(&failed, 1); return;
    }
    fixed_format = info; atomic_fetch_or(&formats, bit);
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_pod_frame f;
    spa_pod_builder_push_object(&b, &f, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers);
    spa_pod_builder_add(&b, SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(3), SPA_PARAM_BUFFERS_size, SPA_POD_Int((int32_t)size),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int((int32_t)stride),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf), 0);
    spa_pod_builder_prop(&b, SPA_PARAM_BUFFERS_metaType, SPA_POD_PROP_FLAG_MANDATORY);
    spa_pod_builder_int(&b, 1 << SPA_META_SyncTimeline);
    const struct spa_pod *params[2]; params[0] = spa_pod_builder_pop(&b, &f);
    params[1] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamMeta, SPA_PARAM_Meta,
        SPA_PARAM_META_type, SPA_POD_Id(SPA_META_SyncTimeline),
        SPA_PARAM_META_size, SPA_POD_Int(sizeof(struct spa_meta_sync_timeline)));
    if (pw_stream_update_params(e->stream, params, 2) < 0) atomic_store(&failed, 1);
}
/* This controlled producer sets exactly READABLE on every block. Require the
 * same on receipt; no inference about flags from other PipeWire producers. */
static int valid_data_flags(const struct spa_buffer *b) {
    if (!b || b->n_datas != 3 || !b->datas) return 0;
    for (unsigned i = 0; i < 3; i++)
        if (b->datas[i].flags != SPA_DATA_FLAG_READABLE) return 0;
    return 1;
}
/* Forge descriptors in a private copy, never mutate the live PipeWire lease. */
static int flags_negative_test(const struct spa_buffer *b) {
    struct spa_data data[3];
    if (!valid_data_flags(b)) return 0;
    memcpy(data, b->datas, sizeof(data));
    struct spa_buffer copy = *b; copy.datas = data;
    for (unsigned i = 0; i < 3; i++) {
        data[i].flags = 0;
        if (valid_data_flags(&copy)) return 0;
        data[i].flags = SPA_DATA_FLAG_READABLE | (1u << 31);
        if (valid_data_flags(&copy)) return 0;
        data[i].flags = SPA_DATA_FLAG_READABLE;
    }
    return valid_data_flags(&copy);
}
static void cross_remove(void *data, struct pw_buffer *buffer) {
    struct endpoint *e = data;
    if (!e->producer && held == buffer) atomic_store(&failed, 1);
    /* Producer BOs deliberately remain alive until process exit. */
}
static void cross_process(void *data) {
    struct endpoint *e = data;
    if (e->producer && atomic_load(&queued)) return;
    if (!e->producer && (held || atomic_load(&received))) return;
    struct pw_buffer *b = pw_stream_dequeue_buffer(e->stream);
    if (!b) return;
    if (e->producer) {
        struct sync_allocation *x = b->user_data;
        if (!x || atomic_load(&formats) != 1) goto bad;
        struct allocation *a = x->allocation;
        struct record r = { getpid(), a->dev, a->ino, size, stride, modifier, SPA_VIDEO_FORMAT_BGRA, WIDTH, HEIGHT };
        uint64_t one = 1;
        producer_handle = x->handle;
        /* CPU signal is a synthetic stand-in, not evidence of GPU completion. */
        if (drmSyncobjTimelineSignal(drmfd, &x->handle, &one, 1) ||
            send(control_fd, &r, sizeof(r), MSG_NOSIGNAL) != sizeof(r)) goto bad;
        atomic_store(&queued, 1);
        if (pw_stream_queue_buffer(e->stream, b) < 0) goto bad;
        atomic_store(&queue_succeeded, 1);
    } else {
        struct stat st; struct record r; struct pnspa_snapshot s;
        held = b; /* Do not queue on failure; process exit or revocation may still release it. */
        if (atomic_load(&formats) != 2 || !pnspa_snapshot(b->buffer, &fixed_format, modifier, &s) ||
            s.format != SPA_VIDEO_FORMAT_BGRA || s.width != WIDTH || s.height != HEIGHT ||
            s.acquire_point != 1 || s.release_point != 2 || s.sync_flags != SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE ||
            !valid_data_flags(b->buffer) || !flags_negative_test(b->buffer) ||
            fstat(s.pixel_fd, &st) || s.mapoffset || s.offset || s.size != size || s.maxsize != size ||
            s.stride != (int32_t)stride || st.st_size != size || receive_record(&r) ||
            r.dev != (uint64_t)st.st_dev || r.ino != (uint64_t)st.st_ino || r.size != size || r.stride != stride ||
            r.modifier != modifier || r.format != s.format || r.width != s.width || r.height != s.height) goto bad;
        struct timespec now;
        if (drmSyncobjFDToHandle(consumer_drm, s.acquire_fd, &acquire_check) ||
            pnt_create(&owner, consumer_drm, s.acquire_fd, 1, s.release_fd, 2, NULL, NULL) ||
            clock_gettime(CLOCK_MONOTONIC, &now) ||
            pnt_wait(owner, (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000) ||
            pnt_no_gpu(owner)) goto bad;
        struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(b->buffer, SPA_META_SyncTimeline, sizeof(*m));
        uint64_t before = 0;
        if (pnt_release(owner, m) || pnt_destroy(&owner) ||
            drmSyncobjQuery(consumer_drm, &acquire_check, &before, 1) || before >= sentinel ||
            pw_stream_queue_buffer(e->stream, b) < 0) goto bad;
        /* Queue success relinquishes the consumer lease; it does not prove
         * the producer dequeued or recovered the buffer. */
        held = NULL; atomic_store(&received, 1);
    }
    return;
bad:
    fprintf(stderr, "cross-process buffer/record validation failed\n");
    atomic_store(&failed, 1);
}
static const struct pw_stream_events cross_events = { PW_VERSION_STREAM_EVENTS,
    .state_changed = state, .param_changed = sync_param, .add_buffer = sync_add,
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
    long pid = strtol(argv[5], &end, 10);
    if (*end || pid < 0 || pid > INT_MAX || (!producer && pid == 0)) return 2;
    producer_pid = (pid_t)pid;
    if (!producer) {
        int enabled = 0; socklen_t len = sizeof(enabled);
        if (getsockopt(control_fd, SOL_SOCKET, SO_PASSCRED, &enabled, &len) || !enabled) return 2;
    }
    struct endpoint e = {0}; modifier = TARGET_MOD;
    if ((drmfd = open_render_node(argv[3])) < 0 || !(gbm = gbm_create_device(drmfd))) goto done;
    if (!producer) {
        struct stat a, b;
        consumer_drm = open_render_node(argv[3]);
        if (consumer_drm < 0 || fstat(drmfd, &a) || fstat(consumer_drm, &b) ||
            a.st_dev != b.st_dev || a.st_ino != b.st_ino || a.st_rdev != b.st_rdev) goto done;
    }
    struct allocation *probe = allocate(&stride, &size);
    if (!probe) goto done;
    close(probe->fd); gbm_bo_destroy(probe->bo); free(probe);
    pw_init(NULL, NULL);
    if (cross_setup(&e, argv[4], producer) < 0) goto cleanup;
    started = 1;
    for (int i = 0; i < 1500 && !atomic_load(&failed); i++) {
        if (!producer && atomic_load(&received)) break;
        if (producer) {
            if (!got_ack) {
                char ack[4]; ssize_t n = recv(control_fd, ack, sizeof(ack), MSG_DONTWAIT | MSG_TRUNC);
                if (n >= 0) {
                    if (n != 3 || memcmp(ack, "ACK", 3) || !atomic_load(&queued)) {
                        fprintf(stderr, "unexpected ACK or peer closure\n"); break;
                    }
                    got_ack = 1;
                } else if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) break;
            }
            if (got_ack && atomic_load(&queue_succeeded)) break;
        }
        usleep(10000);
    }
    pw_thread_loop_stop(e.loop); started = 0;
    if (producer && !atomic_load(&failed) && got_ack && atomic_load(&queue_succeeded) && atomic_load(&formats) == 1) {
        uint64_t point = 0;
        if (producer_handle && !drmSyncobjQuery(drmfd, &producer_handle, &point, 1) && point == 2 &&
            !drmSyncobjTimelineSignal(drmfd, &producer_handle, &sentinel, 1) &&
            send(control_fd, "DONE", 4, MSG_NOSIGNAL) == 4) rc = 0;
    } else if (!producer && !atomic_load(&failed) && atomic_load(&received) && atomic_load(&formats) == 2) {
        struct timespec now; uint64_t point = 0;
        if (send(control_fd, "ACK", 3, MSG_NOSIGNAL) == 3 && wait_for_done() == 0 &&
            !clock_gettime(CLOCK_MONOTONIC, &now) &&
            !drmSyncobjTimelineWait(consumer_drm, &acquire_check, &sentinel, 1,
                (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 2000000000,
                DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, NULL) &&
            !drmSyncobjQuery(consumer_drm, &acquire_check, &point, 1) && point >= sentinel) rc = 0;
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
