/* Isolated, handwritten caller model; not connected to production.
 * cc -std=gnu11 -Wall -Wextra -Werror -g -fsanitize=address,undefined \
 *   $(pkg-config --cflags libspa-0.2 libdrm) tests/portal_nvenc_multiframe_model.c \
 *   c_engine/portal_nvenc_admission.c c_engine/portal_nvenc_timeline.c \
 *   $(pkg-config --libs libdrm) -pthread -o /tmp/nvenc-multiframe && /tmp/nvenc-multiframe
 * Fake object tokens and event assertions are not OS identity, exclusive signal
 * authority, producer termination, GPU completion, or queue atomicity proofs.
 * Those are independent caller attestations; pending signals are invisible to query.
 */
#include "../c_engine/portal_nvenc_admission.h"
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>

struct model {
    struct pna_session s;
    struct pnt_ctx *t;
    struct pna_frame f;
    struct spa_meta_sync_timeline meta;
    uint64_t point, object, pinned_object;
    int fd, pending_high, signals, queues, imports, destroys, consumer_ack, exited, quarantined;
};
static const struct pna_evidence ev={.explicit_opt_in=1,.negotiated=1,.mutter_same_gpu=1,
    .trusted_producer=1,.exclusive_timeline=1,.pinned_lease_and_metadata=1,
    .verified_fd_and_size=1,.stable_unique_buffer_timeline=1,
    .width=4,.height=2,.fourcc=0x34325258};
static int imp(void *o,int drm,int fd,uint32_t *h) { struct model *m=o; (void)drm; assert(fd==m->fd); m->imports++; *h=1; return 0; }
/* Synthetic producer publication/completion, separate from observing a wait. */
static void publish_complete(struct model *m,uint64_t p) { assert(p>m->point); m->point=p; }
static int wait_event(void *o,int drm,uint32_t h,uint64_t p,int64_t timeout) {
    struct model *m=o; (void)drm; (void)timeout; assert(h==1); return m->point>=p ? 0 : -ETIMEDOUT;
}
static int query(void *o,int drm,uint32_t h,uint64_t *p) { struct model *m=o; (void)drm; assert(h==1); *p=m->point; return 0; }
static int signal_event(void *o,int drm,uint32_t h,uint64_t p) { struct model *m=o; (void)drm; assert(h==1 && p>m->point); m->point=p; m->signals++; return 0; }
static int destroy(void *o,int drm,uint32_t h) { struct model *m=o; (void)drm; assert(h==1); m->destroys++; return 0; }
static const struct pnt_ops ops={imp,wait_event,query,signal_event,destroy};
static void init(struct model *m) {
    *m=(struct model){.fd=4,.object=42,.pinned_object=42,.exited=1};
    m->f=(struct pna_frame){.buffer_id=1,.width=4,.height=2,.fourcc=0x34325258,
        .planes=1,.stride=16,.chunk_size=32,.allocation_size=32,.fd=3,
        .dmabuf=1,.metadata_valid=1,.unscheduled_release=1,.full_frame=1};
    assert(!pna_init(&m->s));
}
/* This gate represents independent caller knowledge, not a module identity check. */
static int start(struct model *m,uint64_t g,uint64_t a,uint64_t r) {
    if (!m->exited || m->quarantined || m->object!=m->pinned_object || m->pending_high) return -EPERM;
    m->f.generation=g; m->f.acquire=a; m->f.release=r;
    int rc=pna_admit(&m->s,&m->f,&ev); if (rc) return rc;
    m->meta=(struct spa_meta_sync_timeline){.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,.acquire_point=a,.release_point=r};
    assert(!pnt_create(&m->t,7,m->fd,a,m->fd,r,&ops,m));
    m->exited=0; m->consumer_ack=0; return 0;
}
static void acquired(struct model *m) {
    publish_complete(m,m->f.acquire);
    assert(!pnt_wait(m->t,1)); assert(!pna_acquired(&m->s,1,m->f.generation,1));
    assert(!pnt_gpu_begin(m->t)); assert(!pna_gpu_begin(&m->s,1,m->f.generation,1));
    /* Synthetic independent completion attestation. */
    assert(!pnt_gpu_complete(m->t)); assert(!pna_gpu_complete(&m->s,1,m->f.generation,1));
}
static void release_event(struct model *m) {
    assert(!pnt_release(m->t,&m->meta)); assert(!pna_release(&m->s,1,m->f.generation,1));
    m->queues++; /* exactly one hand-written queue event for this lease */
}
static int exit_event(struct model *m,int proven) {
    if (!proven || !m->consumer_ack) return -EPERM;
    assert(!pnt_destroy(&m->t)); m->exited=1; return 0;
}
static void multiframe(void) {
    struct model m; init(&m);
    for (uint64_t g=1;g<=4;g++) {
        uint64_t a=2*g-1, r=2*g;
        assert(!start(&m,g,a,r)); acquired(&m);
        if (g>1) {
            struct spa_meta_sync_timeline old={.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,.acquire_point=a-2,.release_point=r-2};
            assert(pnt_release(m.t,&old)==-EINVAL && pnt_state(m.t)==PNT_READY);
            assert(old.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE);
            assert(pna_release(&m.s,1,g-1,1)==-EINVAL);
        }
        release_event(&m); assert(m.signals==(int)g && m.queues==(int)g);
        m.consumer_ack=1;
        assert(start(&m,g+1,r+1,r+2)==-EPERM);
        assert(exit_event(&m,0)==-EPERM && m.queues==(int)g);
        assert(pnt_release(m.t,&m.meta)==-EINVAL && m.signals==(int)g);
        assert(!exit_event(&m,1));
        assert(start(&m,g,a,r)==-ESTALE);
        assert(start(&m,g+1,r,r+1)==-ESTALE);
        assert(!m.quarantined);
    }
    /* Same numeric descriptor with a different fake object is rejected by caller,
     * not by pna/pnt; neither API can establish actual kernel object identity. */
    m.object=43; assert(start(&m,5,9,10)==-EPERM); m.object=42;
    m.fd=5; /* changed descriptor alone likewise gives no identity evidence */
    m.object=43; assert(start(&m,5,9,10)==-EPERM); m.object=42;
    assert(!start(&m,5,9,10)); acquired(&m); release_event(&m);
    m.consumer_ack=1; assert(!exit_event(&m,1)); assert(!pna_destroy(&m.s));
}
/* Bypass the caller gate: the revoked admission and retained timeline guard
 * independently reject a valid next frame without changing bookkeeping. */
static void quarantine_checks(struct model *m) {
    struct pna_frame next=m->f;
    struct pnt_ctx *other=NULL, *original=m->t;
    struct pna_history history=m->s.history[0];
    uint64_t point=m->point, id=m->s.active_id, generation=m->s.generation;
    int imports=m->imports, destroys=m->destroys, signals=m->signals, queues=m->queues;
    next.generation++; next.acquire=3; next.release=4;
    assert(m->s.state==PNA_REVOKED && pnt_state(m->t)==PNT_FAILED);
    assert(pna_admit(&m->s,&next,&ev)==-EBUSY);
    assert(pnt_create(&other,7,m->fd,3,m->fd,4,&ops,m)==-EBUSY && !other);
    assert(m->t==original && pnt_state(m->t)==PNT_FAILED && m->s.state==PNA_REVOKED);
    assert(m->s.active_id==id && m->s.generation==generation && !m->s.gpu_pending);
    assert(m->s.history[0].id==history.id && m->s.history[0].generation==history.generation &&
           m->s.history[0].last_release==history.last_release);
    assert(m->point==point && m->imports==imports && m->destroys==destroys &&
           m->signals==signals && m->queues==queues);
    assert(pnt_destroy(&m->t)==-EBUSY && pna_destroy(&m->s)==-EBUSY);
    assert(m->t==original && m->destroys==destroys);
}
static void teardown_quarantine(struct model *m) {
    /* Synthetic irrevocable termination/no-reuse and quiescence attestation. */
    assert(!pnt_abandon_session(m->t)); assert(!pna_abandon(&m->s,1,1,1));
    assert(!pnt_destroy(&m->t) && !m->t && m->destroys==2);
    assert(!pna_destroy(&m->s));
}
static void high_and_pending(void) {
    struct model m; init(&m);
    m.pending_high=1; assert(start(&m,1,1,2)==-EPERM); /* independent attestation */
    m.pending_high=0; publish_complete(&m,2); assert(!start(&m,1,1,2));
    assert(pnt_wait(m.t,1)==-ESTALE); assert(pna_acquired(&m.s,1,1,0)==-EPERM);
    m.quarantined=1; assert(!m.signals && !m.queues);
    assert(start(&m,2,3,4)==-EPERM);
    quarantine_checks(&m); teardown_quarantine(&m);
}
static void unpublished_timeout(void) {
    struct model m; init(&m);
    assert(!start(&m,1,1,2));
    assert(m.point==0 && pnt_wait(m.t,1)==-ETIMEDOUT && m.point==0);
    assert(pna_acquired(&m.s,1,1,0)==-EPERM);
    m.quarantined=1; assert(!m.signals && !m.queues);
    quarantine_checks(&m); teardown_quarantine(&m);
}
int main(void) { multiframe(); high_and_pending(); unpublished_timeout(); puts("PASS: synthetic caller state only; external attestations not verified"); return 0; }
