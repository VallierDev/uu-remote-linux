#define _GNU_SOURCE
/* Standalone: cc -O1 -g -fsanitize=address,undefined -fno-omit-frame-pointer
 * tests/portal_nvenc_lease_lifecycle.c -o /tmp/lease $(pkg-config --cflags --libs
 * gio-2.0 gio-unix-2.0 libpipewire-0.3 libei-1.0) -lpthread -lm */
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <unistd.h>
#include <pipewire/pipewire.h>

static pthread_mutex_t loop = PTHREAD_MUTEX_INITIALIZER;
static struct pw_buffer *incoming;
static unsigned queued;
static void fake_lock(struct pw_thread_loop *p) { (void)p; assert(!pthread_mutex_lock(&loop)); }
static void fake_unlock(struct pw_thread_loop *p) { (void)p; assert(!pthread_mutex_unlock(&loop)); }
static struct pw_buffer *fake_dequeue(struct pw_stream *p) {
    (void)p; struct pw_buffer *b = incoming; incoming = NULL; return b;
}
static int fake_queue(struct pw_stream *p, struct pw_buffer *b) {
    (void)p; assert(b); queued++; return 0;
}
static int fake_update(struct pw_stream *p, const struct spa_pod **v, uint32_t n) {
    (void)p; (void)v; (void)n; return 0;
}
#define pw_thread_loop_lock fake_lock
#define pw_thread_loop_unlock fake_unlock
#define pw_stream_dequeue_buffer fake_dequeue
#define pw_stream_queue_buffer fake_queue
#define pw_stream_update_params fake_update
#include "../c_engine/portal_engine.c"

enum action { REMOVE, FORMAT, DISABLE };
struct job { struct uu_portal *p; struct pw_buffer *b; enum action action; atomic_int blocked, done; };
static void *producer(void *arg) {
    struct job *j = arg;
    /* Prove contention before entering the same loop lock as the callback. */
    assert(pthread_mutex_trylock(&loop) == EBUSY);
    atomic_store(&j->blocked, 1);
    if (j->action == DISABLE) uu_portal_dmabuf_disable(j->p);
    else {
        fake_lock(j->p->pw_loop);
        if (j->action == REMOVE) stream_remove_buffer(j->p, j->b);
        else stream_format(j->p, SPA_PARAM_Format, NULL);
        fake_unlock(j->p->pw_loop);
    }
    atomic_store(&j->done, 1);
    return NULL;
}
static void run(enum action action, int cancel) {
    struct uu_portal *p = calloc(1, sizeof(*p)); assert(p);
    assert(!pthread_mutex_init(&p->lock, NULL)); assert(!pthread_cond_init(&p->changed, NULL));
    p->pw_loop = (struct pw_thread_loop *)1; p->stream = (struct pw_stream *)1;
    p->width = p->height = 2; p->format = SPA_VIDEO_FORMAT_BGRA; p->format_valid = 1;
    p->dma_requested = p->dma_negotiated = p->dma_format = 1;
    struct spa_chunk chunk = { .size = 16, .stride = 8 };
    struct spa_data data = { .type = SPA_DATA_DmaBuf, .fd = 5, .maxsize = 16, .chunk = &chunk };
    struct spa_buffer spa = { .n_datas = 1, .datas = &data };
    struct pw_buffer buffer = { .buffer = &spa };
    queued = 0; incoming = &buffer;
    fake_lock(p->pw_loop); stream_process(p); fake_unlock(p->pw_loop);
    UuDmaFrame frame; uint64_t revision = 0;
    assert(uu_portal_dmabuf_acquire(p, &frame, &revision) == 0);
    assert(p->dma_current == &buffer && frame.fd == 5 && !queued);
    uint64_t epoch = uu_portal_capture_epoch(p);
    assert(!uu_portal_capture_cancelled(p, epoch));
    struct job j = { .p = p, .b = &buffer, .action = action };
    pthread_t thread; assert(!pthread_create(&thread, NULL, producer, &j));
    while (!atomic_load(&j.blocked)) usleep(1000);
    /* This interval represents an outstanding GPU operation, not an actual GPU fence. */
    if (cancel) {
        uu_portal_end(p); assert(atomic_load(&p->closed));
        assert(uu_portal_capture_cancelled(p, epoch));
    }
    usleep(20000);
    assert(!atomic_load(&j.done) && p->dma_current == &buffer && !queued);
    uu_portal_dmabuf_release(p);
    assert(!pthread_join(thread, NULL)); assert(atomic_load(&j.done));
    assert(!p->dma_current);
    assert(uu_portal_capture_cancelled(p, epoch));
    assert(queued == (action == REMOVE ? 0u : 1u));
    if (action == FORMAT) assert(!p->format_valid);
    if (action == DISABLE) assert(p->dma_blocked);
    /* close is only safe here after all lease users and producer callbacks joined. */
    p->stream = NULL; p->pw_loop = NULL; uu_portal_close(p);
}
int main(void) {
    alarm(10);
    for (int a = REMOVE; a <= DISABLE; a++) { run(a, 0); run(a, 1); }
    return 0;
}
