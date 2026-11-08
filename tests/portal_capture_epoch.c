#define _GNU_SOURCE
/* Real Portal callbacks + self-created DRM syncobj. No Portal request or GPU
 * frames. Compile with the same GIO/PipeWire/libei flags as portal_engine tests,
 * plus portal_nvenc_timeline.c, libdrm and -pthread. */
#include "../c_engine/portal_engine.c"
#include "../c_engine/portal_nvenc_timeline.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <xf86drm.h>

struct waiting {
    struct uu_portal *portal;
    struct pnt_ctx *timeline;
    uint64_t epoch;
    atomic_int checks;
    int result;
};
static int64_t test_ns(void) {
    struct timespec t; assert(!clock_gettime(CLOCK_MONOTONIC, &t));
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
static int is_cancelled(void *opaque) {
    struct waiting *w = opaque;
    atomic_fetch_add(&w->checks, 1);
    return uu_portal_capture_cancelled(w->portal, w->epoch);
}
static void *wait_acquire(void *opaque) {
    struct waiting *w = opaque;
    w->result = pnt_wait_cancelable(w->timeline, test_ns() + 2000000000,
                                    is_cancelled, NULL, w);
    return NULL;
}
static struct uu_portal *make_portal(void) {
    struct uu_portal *p = calloc(1, sizeof(*p)); assert(p);
    assert(!pthread_mutex_init(&p->lock, NULL));
    assert(!pthread_cond_init(&p->changed, NULL));
    return p;
}
enum event { FORMAT, REMOVE, PAUSE, ERROR, RESET, REPLACE, END, CLOSED };
static void run(enum event event, int render, int obj) {
    struct uu_portal *p = make_portal();
    struct waiting w = { .portal = p, .epoch = uu_portal_capture_epoch(p) };
    assert(w.epoch != UINT64_MAX && !uu_portal_capture_cancelled(p, w.epoch));
    assert(!pnt_create(&w.timeline, render, obj, 1, obj, 2, NULL, NULL));
    pthread_t thread; assert(!pthread_create(&thread, NULL, wait_acquire, &w));
    int64_t until = test_ns() + 1000000000;
    while (atomic_load(&w.checks) < 2 && test_ns() < until) usleep(1000);
    assert(atomic_load(&w.checks) >= 2);
    struct pw_buffer buffer = {0};
    int64_t start = test_ns();
    switch (event) {
    case FORMAT: stream_format(p, SPA_PARAM_Format, NULL); break;
    case REMOVE: p->dma_current = &buffer; stream_remove_buffer(p, &buffer); break;
    case PAUSE: stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL); break;
    case ERROR: fail(p, 9); break;
    case RESET: uu_portal_reset_frame(p); break;
    case REPLACE: p->dma_current = &buffer; return_dma_buffer(p, &p->dma_current); break;
    case END: uu_portal_end(p); break;
    case CLOSED: session_closed(NULL,NULL,NULL,NULL,NULL,NULL,p); break;
    }
    assert(!pthread_join(thread, NULL));
    assert(test_ns() - start < 500000000);
    assert(w.result == -ECANCELED && uu_portal_capture_cancelled(p, w.epoch));
    assert(pnt_state(w.timeline) == PNT_FAILED && pnt_gpu_begin(w.timeline) == -EINVAL);
    /* This synthetic session has no producer work or GPU objects. */
    assert(!pnt_abandon_session(w.timeline)); assert(!pnt_destroy(&w.timeline));
    uu_portal_close(p); /* All token readers joined before freeing the object. */
}
int main(int argc, char **argv) {
    const char *path = argc == 2 ? argv[1] : "/dev/dri/renderD128";
    int producer = open(path, O_RDWR|O_CLOEXEC), consumer = open(path, O_RDWR|O_CLOEXEC), obj;
    assert(producer >= 0 && consumer >= 0);
    uint32_t handle; assert(!drmSyncobjCreate(producer, 0, &handle));
    assert(!drmSyncobjHandleToFD(producer, handle, &obj));
    for (int event = FORMAT; event <= CLOSED; event++) run((enum event)event, consumer, obj);
    uint64_t point = 99; assert(!drmSyncobjQuery(producer, &handle, &point, 1) && !point);
    assert(!close(obj)); assert(!drmSyncobjDestroy(producer, handle));
    assert(!close(consumer)); assert(!close(producer));
    struct uu_portal *p = make_portal();
    atomic_store(&p->capture_epoch, UINT64_MAX - 1);
    uint64_t epoch = uu_portal_capture_epoch(p);
    invalidate_capture(p); invalidate_capture(p);
    assert(uu_portal_capture_epoch(p) == UINT64_MAX);
    assert(uu_portal_capture_cancelled(p, epoch));
    assert(uu_portal_capture_cancelled(p, UINT64_MAX));
    assert(uu_portal_capture_cancelled(NULL, 0));
    uu_portal_close(p);
    puts("PASS: 8 Portal lifecycle events cancel real acquire waits; no GPU admission, no timeline signal; epoch saturation rejects reuse");
    return 0;
}
