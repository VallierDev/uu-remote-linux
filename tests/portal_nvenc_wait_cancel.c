/* Offline cancellation/deadline tests. No real GPU or producer contract. */
#include "../c_engine/portal_nvenc_timeline.h"
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>

struct fake {
    int cancelled, calls, queries, error, cancel_wait, cancel_query, stale;
    int expire_success, clock_error;
    int64_t time, deadline, last_slice;
};
static int import(void *v, int fd, int obj, uint32_t *h) {
    (void)v; (void)fd; *h = (uint32_t)obj; return 0;
}
static int wait_one(void *v, int fd, uint32_t h, uint64_t point, int64_t until) {
    struct fake *f = v; (void)fd; (void)h; assert(point == 1);
    assert(until > f->time && until - f->time <= 10000000 && until <= f->deadline);
    f->last_slice = until; f->calls++;
    if (f->cancel_wait) f->cancelled = 1;
    if (f->error) f->time = until;
    if (f->expire_success) f->time = f->deadline;
    return f->error;
}
static int query(void *v, int fd, uint32_t h, uint64_t *point) {
    struct fake *f = v; (void)fd; (void)h; f->queries++;
    if (f->cancel_query) f->cancelled = 1;
    *point = f->stale ? 2 : 1; return 0;
}
static int signal_point(void *v, int fd, uint32_t h, uint64_t point) {
    (void)v; (void)fd; (void)h; (void)point; assert(0); return 0;
}
static int destroy(void *v, int fd, uint32_t h) { (void)v; (void)fd; (void)h; return 0; }
static const struct pnt_ops ops = { import, wait_one, query, signal_point, destroy };
static int cancelled(void *v) { return ((struct fake *)v)->cancelled; }
static int64_t now(void *v) {
    struct fake *f = v; return f->clock_error ? -EIO : f->time;
}
static void run(struct fake *f, int expected, int calls, int queries) {
    struct pnt_ctx *c = NULL;
    assert(!pnt_create(&c, 3, 4, 1, 5, 2, &ops, f));
    assert(pnt_wait_cancelable(c, f->deadline, cancelled, now, f) == expected);
    assert(f->calls == calls && f->queries == queries);
    assert(pnt_state(c) == (expected ? PNT_FAILED : PNT_ACQUIRED));
    if (expected) assert(pnt_gpu_begin(c) == -EINVAL);
    /* Only this fake session is now declared terminated with no GPU access. */
    assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
}
int main(void) {
    struct fake f = { .deadline = 25000000 }; run(&f, 0, 1, 1);
    f = (struct fake){ .deadline = 0, .cancelled = 1 }; run(&f, -ECANCELED, 0, 0);
    f = (struct fake){ .deadline = 0 }; run(&f, -ETIMEDOUT, 0, 0);
    f = (struct fake){ .deadline = 25000000, .cancel_wait = 1 }; run(&f, -ECANCELED, 1, 0);
    f = (struct fake){ .deadline = 25000000, .cancel_query = 1 }; run(&f, -ECANCELED, 1, 1);
    f = (struct fake){ .deadline = 25000000, .expire_success = 1 }; run(&f, -ETIMEDOUT, 1, 0);
    f = (struct fake){ .deadline = 25000000, .stale = 1 }; run(&f, -ESTALE, 1, 1);
    f = (struct fake){ .deadline = 25000000, .clock_error = 1 }; run(&f, -EIO, 0, 0);
    int retries[] = {-ETIME, -ETIMEDOUT, -EINTR};
    for (unsigned i = 0; i < sizeof(retries)/sizeof(retries[0]); i++) {
        f = (struct fake){ .deadline = 25000000, .error = retries[i] };
        run(&f, -ETIMEDOUT, 3, 0); assert(f.last_slice == f.deadline);
    }
    f = (struct fake){ .deadline = 25000000, .error = -EIO }; run(&f, -EIO, 1, 0);
    f = (struct fake){ .deadline = 25000000, .error = -EIO, .cancel_wait = 1 };
    run(&f, -ECANCELED, 1, 0);
    puts("PASS: cancellable acquire deadlines, retries, cancellation precedence and no GPU admission on failure");
    return 0;
}
