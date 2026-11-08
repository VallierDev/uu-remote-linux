#include "portal_nvenc_timeline.h"
#include <errno.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <time.h>
#include <xf86drm.h>
#include <drm.h>
#include <spa/buffer/meta.h>

/* Retained until all handle destruction completes, even on errors. */
static atomic_flag active_frame = ATOMIC_FLAG_INIT;

struct pnt_ctx {
    struct pnt_ops ops;
    void *opaque;
    int fd, cancelled, promised, protocol, gpu_started, gpu_completed;
    uint32_t acquire, release;
    int have_acquire, have_release;
    uint64_t ap, rp;
    enum pnt_state state;
};
static int result(int r) { return r == 0 ? 0 : (r == -1 ? -(errno ? errno : EIO) : (r < 0 ? r : -EIO)); }
static int imp(void *o, int fd, int obj, uint32_t *h) { (void)o; return result(drmSyncobjFDToHandle(fd, obj, h)); }
static int wait_one(void *o, int fd, uint32_t h, uint64_t p, int64_t t) {
    (void)o; return result(drmSyncobjTimelineWait(fd, &h, &p, 1, t, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, NULL));
}
static int query(void *o, int fd, uint32_t h, uint64_t *p) { (void)o; return result(drmSyncobjQuery(fd, &h, p, 1)); }
static int sig(void *o, int fd, uint32_t h, uint64_t p) { (void)o; return result(drmSyncobjTimelineSignal(fd, &h, &p, 1)); }
static int dest(void *o, int fd, uint32_t h) { (void)o; return result(drmSyncobjDestroy(fd, h)); }
static const struct pnt_ops real_ops = { imp, wait_one, query, sig, dest };

int pnt_create(struct pnt_ctx **out, int fd, int af, uint64_t ap, int rf, uint64_t rp,
               const struct pnt_ops *ops, void *opaque) {
    return pnt_create_protocol(out, fd, af, ap, rf, rp, ops, opaque, 0);
}
int pnt_create_protocol(struct pnt_ctx **out, int fd, int af, uint64_t ap, int rf, uint64_t rp,
    const struct pnt_ops *ops, void *opaque, int protocol) {
    if (protocol != 0 && protocol != 1) return -EINVAL;
    struct pnt_ctx *c; int r;
    if (!out || *out || fd < 0 || af < 0 || rf < 0 || !ap || ap >= rp) return -EINVAL;
    if (!ops) ops = &real_ops;
    if (!ops->import || !ops->wait || !ops->query || !ops->signal || !ops->destroy) return -EINVAL;
    c = calloc(1, sizeof(*c)); if (!c) return -ENOMEM;
    if (atomic_flag_test_and_set_explicit(&active_frame, memory_order_acquire)) { free(c); return -EBUSY; }
    c->ops = *ops; c->opaque = opaque; c->fd = fd; c->ap = ap; c->rp = rp; c->protocol = protocol;
    /* Publish even on import failure so failed handle cleanup can be retried. */
    *out = c;
    r = c->ops.import(opaque, fd, af, &c->acquire);
    if (r) { c->state = PNT_FAILED; return r < 0 ? r : -EIO; }
    c->have_acquire = 1;
    r = c->ops.import(opaque, fd, rf, &c->release);
    if (r) { c->state = PNT_FAILED; return r < 0 ? r : -EIO; }
    c->have_release = 1;
    return 0;
}
static int release_pending(struct pnt_ctx *c);
int pnt_wait(struct pnt_ctx *c, int64_t timeout) {
    int r;
    if (!c || c->state != PNT_NEW || timeout < 0) return -EINVAL;
    r = c->ops.wait(c->opaque, c->fd, c->acquire, c->ap, timeout);
    if (r) { c->state = PNT_FAILED; return r < 0 ? r : -EIO; }
    r = release_pending(c);
    if (r) return r;
    c->state = PNT_ACQUIRED; return 0;
}
static int64_t monotonic_now(void *opaque) {
    struct timespec ts; (void)opaque;
    if (clock_gettime(CLOCK_MONOTONIC, &ts)) return -(errno ? errno : EIO);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}
static int wait_check(int64_t deadline, int (*cancel)(void *),
                      int64_t (*now)(void *), void *opaque, int64_t *time) {
    if (cancel(opaque)) return -ECANCELED;
    *time = now(opaque);
    if (*time < 0) return -EIO;
    return *time >= deadline ? -ETIMEDOUT : 0;
}
int pnt_wait_cancelable(struct pnt_ctx *c, int64_t deadline,
                        int (*cancel)(void *), int64_t (*now)(void *), void *opaque) {
    if (!c || c->state != PNT_NEW || deadline < 0 || !cancel) return -EINVAL;
    if (!now) now = monotonic_now;
    int r; int64_t time;
    for (;;) {
        r = wait_check(deadline, cancel, now, opaque, &time);
        if (r) break;
        int64_t slice = deadline - time > 10000000 ? time + 10000000 : deadline;
        r = c->ops.wait(c->opaque, c->fd, c->acquire, c->ap, slice);
        int checked = wait_check(deadline, cancel, now, opaque, &time);
        if (checked) { r = checked; break; }
        if (r == -EINTR || r == -ETIME || r == -ETIMEDOUT) continue;
        if (r) { if (r > 0) r = -EIO; break; }
        r = release_pending(c);
        checked = wait_check(deadline, cancel, now, opaque, &time);
        if (checked) r = checked;
        if (!r) { c->state = PNT_ACQUIRED; return 0; }
        break;
    }
    c->state = PNT_FAILED;
    return r;
}
static int release_pending(struct pnt_ctx *c) {
    uint64_t current = 0;
    int r = c->ops.query(c->opaque, c->fd, c->release, &current);
    if (r || current >= c->rp) {
        c->state = PNT_FAILED;
        return r ? (r < 0 ? r : -EIO) : -ESTALE;
    }
    return 0;
}
int pnt_gpu_begin(struct pnt_ctx *c) { int r; if (!c || c->state != PNT_ACQUIRED) return -EINVAL; r = release_pending(c); if (r) return r; c->gpu_started = 1; c->state = PNT_GPU; return 0; }
int pnt_gpu_complete(struct pnt_ctx *c) { if (!c || c->state != PNT_GPU) return -EINVAL; c->gpu_completed = 1; c->state = c->cancelled ? PNT_FAILED : PNT_READY; return 0; }
int pnt_no_gpu(struct pnt_ctx *c) { int r; if (!c || c->state != PNT_ACQUIRED) return -EINVAL; r = release_pending(c); if (r) return r; c->state = PNT_READY; return 0; }
int pnt_release(struct pnt_ctx *c, struct spa_meta_sync_timeline *metadata) {
#ifndef SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE
    /* Older SPA headers cannot describe explicit sync; Portal admission also
     * rejects it. Keep legacy builds usable without guessing the metadata ABI. */
    (void)c; (void)metadata; return -ENOTSUP;
#else
    int r;
    if (!c || c->state != PNT_READY || !metadata) return -EINVAL;
    if (!c->promised) {
        if (metadata->acquire_point != c->ap || metadata->release_point != c->rp ||
            (c->protocol == 1 ? metadata->flags != 0 : !(metadata->flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE))) return -EINVAL;
        r = release_pending(c);
        if (r) return r;
        /* No fallible operation or callback between recording and clearing. */
        c->promised = 1;
        if (c->protocol == 0) metadata->flags &= ~SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    } else return -EINVAL;
    r = c->ops.signal(c->opaque, c->fd, c->release, c->rp);
    if (r) { c->state = PNT_FAILED; return r < 0 ? r : -EIO; }
    c->state = PNT_DONE; return 0;
#endif
}
int pnt_cancel(struct pnt_ctx *c) {
    if (!c || c->state == PNT_DONE || c->state == PNT_ABANDONED || c->state == PNT_DISCARDED || c->promised) return -EINVAL;
    if (c->state == PNT_GPU) c->cancelled = 1; else c->state = PNT_FAILED;
    return 0;
}
enum pnt_state pnt_state(const struct pnt_ctx *c) { return c ? c->state : PNT_FAILED; }
int pnt_abandon_session(struct pnt_ctx *c) {
    if (!c || c->state == PNT_DONE) return -EINVAL;
    if (c->state == PNT_GPU) return -EBUSY;
    c->state = PNT_ABANDONED;
    return 0;
}
int pnt_destroy(struct pnt_ctx **p) {
    struct pnt_ctx *c; int r;
    if (!p || !(c = *p)) return -EINVAL;
    if (c->state != PNT_DONE && c->state != PNT_ABANDONED && c->state != PNT_DISCARDED) return -EBUSY;
    if (c->have_release) { r = c->ops.destroy(c->opaque, c->fd, c->release); if (r) return r < 0 ? r : -EIO; c->have_release = 0; }
    if (c->have_acquire) { r = c->ops.destroy(c->opaque, c->fd, c->acquire); if (r) return r < 0 ? r : -EIO; c->have_acquire = 0; }
    free(c); *p = NULL;
    atomic_flag_clear_explicit(&active_frame, memory_order_release);
    return 0;
}

int pnt_retire_proven_session(struct pnt_ctx **ctx) {
    if (!ctx || !*ctx) return -EINVAL;
    if (pnt_state(*ctx) != PNT_DONE) {
        int result = pnt_abandon_session(*ctx);
        if (result) return result;
    }
    return pnt_destroy(ctx);
}

int pnt_discard_unsubmitted(struct pnt_ctx **ctx) {
    if (!ctx || !*ctx) return -EINVAL;
    struct pnt_ctx *c = *ctx;
    if (c->gpu_started || c->promised) return -EBUSY;
    c->state = PNT_DISCARDED;
    return pnt_destroy(ctx);
}

int pnt_discard_before_release(struct pnt_ctx **ctx) {
    if (!ctx || !*ctx) return -EINVAL;
    struct pnt_ctx *c = *ctx;
    if ((c->gpu_started && !c->gpu_completed) || c->promised) return -EBUSY;
    c->state = PNT_DISCARDED;
    return pnt_destroy(ctx);
}

int pnt_discard_completed(struct pnt_ctx **ctx) {
    if (!ctx || !*ctx) return -EINVAL;
    struct pnt_ctx *c = *ctx;
    if (c->gpu_started && !c->gpu_completed) return -EBUSY;
    /* Drop local handle references only. A past signal is neither undone nor
     * retried, even if its ioctl returned an ambiguous error. No GPU access may
     * survive the independent completion attestation or begin after this call. */
    c->state = PNT_DISCARDED;
    return pnt_destroy(ctx);
}
