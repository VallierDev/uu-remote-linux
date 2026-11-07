#define _GNU_SOURCE
/* Portal callback/descriptor ownership checks only; memfds are not GPU buffers
 * or syncobjs and the release callback is fake. No Portal request is made. */
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/mman.h>
#include <unistd.h>
#include <pipewire/pipewire.h>
static pthread_mutex_t loop = PTHREAD_MUTEX_INITIALIZER;
static unsigned queues, releases;
static int queue_error, release_error, preserve_flag, protocol;
static void lock_loop(struct pw_thread_loop *p) { (void)p; assert(!pthread_mutex_lock(&loop)); }
static void unlock_loop(struct pw_thread_loop *p) { (void)p; assert(!pthread_mutex_unlock(&loop)); }
static int queue(struct pw_stream *p, struct pw_buffer *b) {
    (void)p; assert(b); queues++; return queue_error;
}
#define pw_thread_loop_lock lock_loop
#define pw_thread_loop_unlock unlock_loop
#define pw_stream_queue_buffer queue
#include "../c_engine/portal_engine.c"
static int release_fake(void *opaque, struct spa_meta_sync_timeline *m) {
    (void)opaque;
    assert(m->acquire_point == 1 && m->release_point == 2);
    assert(m->flags == (protocol ? 0u : SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
    releases++;
    m->flags = preserve_flag ? SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE : 0;
    return release_error;
}
struct fixture {
    struct uu_portal *p;
    struct spa_chunk chunk;
    struct spa_data data[3];
    struct spa_meta_sync_timeline timeline;
    struct spa_meta meta[2];
    struct spa_buffer spa;
    struct pw_buffer buffer;
};
static void setup(struct fixture *f) {
    memset(f, 0, sizeof(*f)); queues = releases = 0; queue_error = release_error = preserve_flag = 0;
    f->p = calloc(1, sizeof(*f->p)); assert(f->p);
    assert(!pthread_mutex_init(&f->p->lock, NULL)); assert(!pthread_cond_init(&f->p->changed, NULL));
    f->p->pw_loop = (void *)1; f->p->stream = (void *)1;
    f->p->format_valid = f->p->dma_format = f->p->dma_requested = 1;
    f->p->explicit_protocol = protocol;
    f->p->width = f->p->height = 2; f->p->format = SPA_VIDEO_FORMAT_BGRA;
    f->chunk = (struct spa_chunk){ .size = 16, .stride = 8 };
    for (unsigned i = 0; i < 3; i++) {
        int fd = memfd_create("lease-test", MFD_CLOEXEC); assert(fd >= 0); assert(!ftruncate(fd, 16));
        f->data[i] = (struct spa_data){ .type = i ? SPA_DATA_SyncObj : SPA_DATA_DmaBuf,
            .flags = SPA_DATA_FLAG_READABLE, .fd = fd, .maxsize = 16, .chunk = i ? NULL : &f->chunk };
    }
    f->timeline = (struct spa_meta_sync_timeline){ .acquire_point = 1, .release_point = 2,
        .flags = protocol ? 0u : SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE };
    f->meta[0] = (struct spa_meta){ .type = SPA_META_SyncTimeline, .size = sizeof(f->timeline), .data = &f->timeline };
    f->spa = (struct spa_buffer){ .n_datas = 3, .datas = f->data, .n_metas = 1, .metas = f->meta };
    f->buffer.buffer = &f->spa; f->p->dma_pending = &f->buffer;
}
static void cleanup(struct fixture *f) {
    for (unsigned i = 0; i < 3; i++) if (f->data[i].fd >= 0) assert(!close((int)f->data[i].fd));
    f->p->pw_loop = NULL; f->p->stream = NULL; uu_portal_close(f->p);
}
static void assert_closed(UuPortalAsyncFrame *s) {
    int fds[] = {s->pixels.fd, s->acquire_fd, s->release_fd};
    for (unsigned i = 0; i < 3; i++) { errno = 0; assert(fcntl(fds[i], F_GETFD) == -1 && errno == EBADF); }
}
static void *remove_during_lease(void *opaque) {
    struct fixture *f = opaque;
    lock_loop(f->p->pw_loop); stream_remove_buffer(f->p, &f->buffer); unlock_loop(f->p->pw_loop);
    return NULL;
}
int main(void) {
    alarm(15);
    for (int selected = 0; selected < 2; selected++) {
      protocol = selected;
      for (int mode = 0; mode < 6; mode++) {
        struct fixture f; setup(&f);
        struct uu_portal_async_lease *lease = NULL; UuPortalAsyncFrame snapshot;
        assert(!uu_portal_async_acquire(f.p, &lease, &snapshot));
        assert(!f.p->dma_pending && f.p->dma_async == lease && !uu_portal_async_cancelled(lease));
        assert(snapshot.pixels.fd != f.data[0].fd && snapshot.pixels.width == 2);
        assert(fcntl(snapshot.pixels.fd, F_GETFD) & FD_CLOEXEC);
        struct uu_portal_async_lease *second = NULL;
        assert(uu_portal_async_acquire(f.p, &second, &snapshot) == -EBUSY);
        assert(uu_portal_async_finish(&lease, 0, release_fake, NULL) == -EBUSY);
        assert(!queues && !releases);
        if (mode == 1) {
            pthread_t thread; assert(!pthread_create(&thread, NULL, remove_during_lease, &f));
            assert(!pthread_join(thread, NULL)); /* Loop is not held by worker lease. */
            assert(uu_portal_async_cancelled(lease) && !lease->buffer);
            memset(&f.spa, 0xa5, sizeof(f.spa)); /* Removed metadata must not be read. */
            for (unsigned i = 0; i < 3; i++) { assert(!close((int)f.data[i].fd)); f.data[i].fd = -1; }
            assert(fcntl(snapshot.pixels.fd, F_GETFD) >= 0);
        } else if (mode == 2) release_error = -EIO;
        else if (mode == 3) queue_error = -EPIPE;
        else if (mode == 4) uu_portal_close(f.p); /* Must defer object free. */
        else if (mode == 5) preserve_flag = 1;
        int result = uu_portal_async_finish(&lease, 1, release_fake, NULL);
        if (!mode) { assert(!result && !lease && queues == 1 && releases == 1); }
        else {
            assert(result < 0 && lease);
            assert(queues == (mode == 3 ? 1u : 0u));
            assert(releases == (mode == 2 || mode == 3 || mode == 5 ? 1u : 0u));
            if (mode == 3) {
                assert(result == -EPIPE && lease->attempted && f.p->dma_async == lease);
                assert(f.timeline.flags == 0 && fcntl(snapshot.pixels.fd, F_GETFD) >= 0);
            }
            unsigned before_q = queues, before_r = releases;
            assert(uu_portal_async_finish(&lease, 1, release_fake, NULL) < 0);
            assert(queues == before_q && releases == before_r);
            if (mode != 4) assert(uu_portal_async_abandon(&lease, 1) == -EBUSY);
            uu_portal_end(f.p);
            assert(uu_portal_async_abandon(&lease, 0) == -EBUSY);
            assert(!uu_portal_async_abandon(&lease, 1) && !lease);
        }
        assert_closed(&snapshot); cleanup(&f);
      }
    }
    struct fixture f; setup(&f);
    struct uu_portal_async_lease *lease = NULL; UuPortalAsyncFrame snapshot;
    f.meta[1] = f.meta[0]; f.spa.n_metas = 2;
    assert(uu_portal_async_acquire(f.p, &lease, &snapshot) == -EINVAL && !lease);
    assert(f.p->dma_pending == &f.buffer && !queues);
    f.spa.n_metas = 1;
    int first_duplicate = fcntl((int)f.data[0].fd, F_DUPFD_CLOEXEC, 0);
    assert(first_duplicate >= 0 && !close(first_duplicate));
    int64_t original = f.data[1].fd; f.data[1].fd = INT_MAX;
    assert(uu_portal_async_acquire(f.p, &lease, &snapshot) == -EBADF && !lease);
    assert(fcntl(first_duplicate, F_GETFD) == -1 && errno == EBADF);
    assert(f.p->dma_pending == &f.buffer && !queues);
    f.data[1].fd = original;
    cleanup(&f);
    puts("PASS: both explicit protocols: async lease snapshot, unlocked removal, retained fds, no retry after release/queue failure, deferred close and metadata rejection");
    return 0;
}
