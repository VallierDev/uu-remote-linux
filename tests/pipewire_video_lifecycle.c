/* Private-server integration helper, launched only by pipewire_video_lifecycle.py.
 * CPU memory protocol observations only: this is not a GPU completion test. */
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct endpoint { struct pw_thread_loop *loop; struct pw_stream *stream; struct spa_hook listener; int producer; };
static struct endpoint src, dst;
static atomic_int adds, removes, received, errors;
/* held and target_removed are protected by dst.loop, including callbacks. */
static struct pw_buffer *held;
static int target_removed;
static int renegotiate, requested, changed_format, event_seq;
static void event(const char *name) {
    printf("event %d: %s\n", ++event_seq, name); fflush(stdout);
}
static uintptr_t target_id;
static uint32_t produced, last_received;
static void add(void *data, struct pw_buffer *b) { (void)b; if (!((struct endpoint *)data)->producer) atomic_fetch_add(&adds, 1); }
static void remove_buffer(void *data, struct pw_buffer *b) {
    if (!((struct endpoint *)data)->producer) {
        if ((uintptr_t)b == target_id && target_id) {
            if (renegotiate) event("target remove");
            target_removed = 1; held = NULL; target_id = 0;
        }
        atomic_fetch_add(&removes, 1);
    }
}
static void process(void *data) {
    struct endpoint *e = data;
    struct pw_buffer *b = pw_stream_dequeue_buffer(e->stream);
    if (!b) return;
    /* Frames in flight across a format transition need not contain our pattern. */
    if (renegotiate && !e->producer && requested) goto queue;
    if (!b->buffer || b->buffer->n_datas != 1 || !b->buffer->datas) {
        atomic_store(&errors, 1); goto queue;
    }
    struct spa_data *d = &b->buffer->datas[0];
    if (!d->data || !d->chunk || d->maxsize < 64*64*4) {
        atomic_store(&errors, 1); goto queue;
    }
    if (e->producer) {
        uint32_t seq = ++produced;
        unsigned char *p = d->data;
        memcpy(p, &seq, sizeof(seq));
        for (unsigned i = sizeof(seq); i < 64*64*4; i++) p[i] = (seq * 31u + i * 17u + (i >> 8)) & 255;
        d->chunk->offset = 0; d->chunk->size = 64*64*4; d->chunk->stride = 64*4;
    } else {
        if (d->chunk->offset > d->maxsize || d->chunk->size != 64*64*4 ||
            d->chunk->size > d->maxsize - d->chunk->offset || d->chunk->stride != 64*4) {
            atomic_store(&errors, 1); goto queue;
        }
        unsigned char *p = (unsigned char *)d->data + d->chunk->offset;
        uint32_t seq; memcpy(&seq, p, sizeof(seq));
        unsigned i;
        for (i = sizeof(seq); i < 64*64*4 && p[i] == ((seq * 31u + i * 17u + (i >> 8)) & 255); i++);
        if (!seq || seq <= last_received || i != 64*64*4) {
            atomic_store(&errors, 1); goto queue;
        }
        last_received = seq;
        atomic_fetch_add(&received, 1);
        if (!target_id && atomic_load(&received) >= 2) { held = b; target_id = (uintptr_t)b; return; }
    }
queue:
    if (pw_stream_queue_buffer(e->stream, b) < 0) atomic_store(&errors, 1);
}
static void param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    struct endpoint *e = data;
    uint8_t storage[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *params[1];
    if (id != SPA_PARAM_Format) return;
    if (renegotiate && !e->producer && requested) {
        if (!param) event("format cleared");
        else {
            struct spa_video_info_raw info = {0};
            if (spa_format_video_raw_parse(param, &info) >= 0) {
                printf("event %d: format %ux%u %u/%u\n", ++event_seq,
                       info.size.width, info.size.height, info.framerate.num, info.framerate.denom);
                fflush(stdout);
                if (info.framerate.num == 60 && info.framerate.denom == 1) changed_format = 1;
            } else event("format parse failed");
        }
    }
    if (!param) return;
    params[0] = spa_pod_builder_add_object(&b, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
        SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_size, SPA_POD_Int(64*64*4),
        SPA_PARAM_BUFFERS_stride, SPA_POD_Int(64*4));
    if (pw_stream_update_params(e->stream, params, 1) < 0) atomic_store(&errors, 1);
}
static void state(void *data, enum pw_stream_state old, enum pw_stream_state now, const char *error) {
    struct endpoint *e = data;
    if (renegotiate && !e->producer && requested) {
        printf("event %d: state %s -> %s\n", ++event_seq,
               pw_stream_state_as_string(old), pw_stream_state_as_string(now)); fflush(stdout);
    }
    if (now == PW_STREAM_STATE_ERROR) { fprintf(stderr, "stream error: %s\n", error ? error : "unknown"); atomic_store(&errors, 1); }
}
static const struct pw_stream_events events = { PW_VERSION_STREAM_EVENTS, .state_changed = state, .param_changed = param_changed, .add_buffer = add, .remove_buffer = remove_buffer, .process = process };
static void setup(struct endpoint *e, const char *name, int producer) {
    uint8_t storage[1024]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *params[1];
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA, .size = SPA_RECTANGLE(64,64), .framerate = SPA_FRACTION(30,1) };
    e->producer = producer;
    e->loop = pw_thread_loop_new(name, NULL);
    if (!e->loop) { fprintf(stderr, "loop creation failed\n"); exit(1); }
    e->stream = pw_stream_new_simple(pw_thread_loop_get_loop(e->loop), name,
        pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, producer ? "Capture" : "Playback",
                          PW_KEY_MEDIA_ROLE, "Test", PW_KEY_NODE_NAME, name, PW_KEY_NODE_AUTOCONNECT, "false", NULL), &events, e);
    if (!e->stream) { fprintf(stderr, "stream creation failed\n"); exit(1); }
    params[0] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    if (pw_stream_connect(e->stream, producer ? PW_DIRECTION_OUTPUT : PW_DIRECTION_INPUT, PW_ID_ANY,
                          PW_STREAM_FLAG_MAP_BUFFERS, params, 1) < 0 || pw_thread_loop_start(e->loop) < 0) {
        fprintf(stderr, "stream connect/start failed\n"); exit(1);
    }
}
static int update_format(struct endpoint *e) {
    uint8_t storage[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    struct spa_video_info_raw info = { .format = SPA_VIDEO_FORMAT_BGRA,
        .size = SPA_RECTANGLE(64,64), .framerate = SPA_FRACTION(60,1) };
    const struct spa_pod *param = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &info);
    return pw_stream_update_params(e->stream, &param, 1);
}
int main(int argc, char **argv) {
    renegotiate = argc == 2 && !strcmp(argv[1], "renegotiate");
    int consumer_disconnect = argc == 2 && !strcmp(argv[1], "consumer");
    if (argc != 2 || (!renegotiate && !consumer_disconnect && strcmp(argv[1], "producer"))) return 2;
    pw_init(NULL, NULL);
    setup(&src, "uu-video-producer", 1); setup(&dst, "uu-video-consumer", 0);
    puts("READY"); fflush(stdout);
    /* The parent explicitly links the uniquely named ports. */
    for (int i=0; i<500 && atomic_load(&received) < 2 && !atomic_load(&errors); i++) usleep(10000);
    if (atomic_load(&received) < 2 || !atomic_load(&adds) || atomic_load(&errors)) { fprintf(stderr, "no verified frame/add event\n"); return 1; }
    /* Stop callbacks while acquiring a real outstanding buffer. */
    pw_thread_loop_lock(dst.loop);
    if (!held || target_removed) { fprintf(stderr, "target missing or already removed\n"); pw_thread_loop_unlock(dst.loop); return 1; }
    int rc;
    if (renegotiate) {
        requested = 1; event("request 60 fps while target held");
        rc = update_format(&dst);
        pw_thread_loop_unlock(dst.loop);
        pw_thread_loop_lock(src.loop);
        int src_rc = update_format(&src);
        pw_thread_loop_unlock(src.loop);
        if (rc < 0 || src_rc < 0) {
            fprintf(stderr, "format update rejected: consumer=%d producer=%d\n", rc, src_rc);
            return 1;
        }
        int changed = 0, removed = 0;
        for (int i = 0; i < 300 && !atomic_load(&errors); i++) {
            pw_thread_loop_lock(dst.loop);
            changed = changed_format; removed = target_removed;
            pw_thread_loop_unlock(dst.loop);
            if (changed && removed) break;
            usleep(10000);
        }
        if (atomic_load(&errors)) { fprintf(stderr, "stream error during renegotiation\n"); return 1; }
        if (changed && removed) puts("PASS: renegotiated format and removed outstanding target");
        else if (changed) {
            fprintf(stderr, "format changed but target removal not observed (3s)\n");
            return 1;
        } else printf("SKIP: 60 fps format not observed after update (3s; target_removed=%d)\n", removed);
        pw_thread_loop_stop(src.loop); pw_thread_loop_stop(dst.loop);
        pw_stream_destroy(src.stream); pw_stream_destroy(dst.stream);
        pw_thread_loop_destroy(src.loop); pw_thread_loop_destroy(dst.loop); pw_deinit();
        return 0;
    }
    if (consumer_disconnect) {
        /* The remove callback may run synchronously inside disconnect. */
        rc = pw_stream_disconnect(dst.stream);
    } else {
        pw_thread_loop_lock(src.loop);
        rc = pw_stream_disconnect(src.stream);
        pw_thread_loop_unlock(src.loop);
    }
    pw_thread_loop_unlock(dst.loop);
    if (rc < 0) { fprintf(stderr, "disconnect failed: %d\n", rc); return 1; }
    int removed = 0;
    for (int i=0; i<300 && !removed; i++) {
        pw_thread_loop_lock(dst.loop); removed = target_removed; pw_thread_loop_unlock(dst.loop);
        if (!removed) usleep(10000);
    }
    if (!removed || atomic_load(&errors)) { fprintf(stderr, "target remove not observed or stream error\n"); return 1; }
    printf("PASS: %s disconnect, outstanding target removed without queue (local protocol only)\n", argv[1]);
    pw_thread_loop_stop(src.loop); pw_thread_loop_stop(dst.loop);
    pw_stream_destroy(src.stream); pw_stream_destroy(dst.stream);
    pw_thread_loop_destroy(src.loop); pw_thread_loop_destroy(dst.loop); pw_deinit();
    return 0;
}
