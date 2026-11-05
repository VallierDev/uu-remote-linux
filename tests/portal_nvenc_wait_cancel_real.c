/* Self-created unsignaled syncobj, no Portal or GPU buffers. */
#include "../c_engine/portal_nvenc_timeline.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
struct work { struct pnt_ctx *ctx; atomic_int cancel, checks; int result; int64_t deadline; };
static int64_t now(void) {
    struct timespec ts; assert(!clock_gettime(CLOCK_MONOTONIC, &ts));
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}
static int cancelled(void *v) {
    struct work *w = v; atomic_fetch_add(&w->checks, 1); return atomic_load(&w->cancel);
}
static void *waiter(void *v) {
    struct work *w = v;
    w->result = pnt_wait_cancelable(w->ctx, w->deadline, cancelled, NULL, w);
    return NULL;
}
int main(int argc, char **argv) {
    const char *path = argc == 2 ? argv[1] : "/dev/dri/renderD128";
    int producer = open(path, O_RDWR | O_CLOEXEC), consumer = open(path, O_RDWR | O_CLOEXEC), obj = -1;
    assert(producer >= 0 && consumer >= 0);
    uint32_t h; assert(!drmSyncobjCreate(producer, 0, &h));
    assert(!drmSyncobjHandleToFD(producer, h, &obj));
    struct work w = { .deadline = now() + 2000000000 };
    assert(!pnt_create(&w.ctx, consumer, obj, 1, obj, 2, NULL, NULL));
    pthread_t thread; assert(!pthread_create(&thread, NULL, waiter, &w));
    int64_t ready_deadline = now() + 1000000000;
    /* Two checks prove at least one finite wait call returned before cancel. */
    while (atomic_load(&w.checks) < 2 && now() < ready_deadline) usleep(1000);
    assert(atomic_load(&w.checks) >= 2);
    int64_t start = now(); atomic_store(&w.cancel, 1);
    assert(!pthread_join(thread, NULL));
    int64_t elapsed = now() - start;
    assert(w.result == -ECANCELED && elapsed < 500000000);
    assert(pnt_state(w.ctx) == PNT_FAILED && pnt_gpu_begin(w.ctx) == -EINVAL);
    uint64_t point = 99; assert(!drmSyncobjQuery(producer, &h, &point, 1) && point == 0);
    /* No external producer or GPU work exists in this synthetic session. */
    assert(!pnt_abandon_session(w.ctx)); assert(!pnt_destroy(&w.ctx));
    assert(!close(obj)); assert(!drmSyncobjDestroy(producer, h));
    assert(!close(consumer)); assert(!close(producer));
    printf("PASS: real unsignaled syncobj cancelled in %.3f ms; no GPU admission or release\n", elapsed / 1000000.0);
    return 0;
}
