#ifndef PORTAL_NVENC_TIMELINE_H
#define PORTAL_NVENC_TIMELINE_H

#include <stdint.h>

/* Experimental explicit-sync backend. A process-wide atomic guard permits at most one
 * live frame context, regardless of timeline identity; it remains held until
 * destroy completes after release or session abandonment. Concurrent calls on
 * one context are forbidden. This cannot coordinate other processes, foreign
 * timeline signals, or forked children: caller must ensure exclusive signaling
 * authority over all related timelines across the whole system, must load only
 * one copy of this module (including DSOs), and must not fork while using it.
 * Without that proof do not use this module. Acquire and release may share a
 * timeline (Mutter 51 does this); acquire_point must be less than release_point.
 * No pending signal at or above release_point may exist when acquire completes;
 * thereafter no other actor may signal or schedule such a point until this
 * frame is safely released. Query sees only completed points, not pending
 * higher signals, later signals or substituted fds. The
 * caller must independently prove this authority and timeline/lease lifetime;
 * query and local serialization cannot establish it.
 * pnt_discard_unsubmitted, pnt_discard_before_release and pnt_discard_completed are local
 * cleanup exceptions; neither permits recycling a lease or accessing pixels.
 * All fds and the frame lease are borrowed;
 * caller keeps them valid and exclusively controls metadata and lease throughout.
 * Never return/recycle the lease on any failure, cancellation or timeout. The
 * caller must also keep the DRM fd open until destroy succeeds. No callbacks may
 * reenter this API. Return values are 0 or negative errno values.
 *
 * pnt_release records the promise and clears UNSCHEDULED_RELEASE directly in
 * the live SPA metadata before signal, preserving other flags. Caller must
 * exclusively own the metadata (no concurrent readers/writers or reentrancy),
 * keep it valid until release succeeds or session teardown, and not publish or
 * requeue the buffer before release succeeds. This is a serialized protocol
 * operation, not an interprocess atomic transaction or crash recovery. On signal
 * error the signal may already have taken effect. No retry is permitted for
 * libdrm or custom ops; retain the promise, lease, handles and exclusive guard
 * until proven session termination permits abandonment, or independently known
 * GPU completion permits local disposal via pnt_discard_completed without any
 * retry, metadata mutation or buffer return. Unknown GPU work must remain
 * quarantined. GPU completion must be independently
 * established, not inferred from submission.
 */
struct spa_meta_sync_timeline;
enum pnt_state { PNT_NEW, PNT_ACQUIRED, PNT_GPU, PNT_READY, PNT_FAILED, PNT_DONE, PNT_ABANDONED, PNT_DISCARDED };
struct pnt_ops {
    int (*import)(void *, int, int, uint32_t *);
    int (*wait)(void *, int, uint32_t, uint64_t, int64_t);
    int (*query)(void *, int, uint32_t, uint64_t *);
    int (*signal)(void *, int, uint32_t, uint64_t);
    int (*destroy)(void *, int, uint32_t);
};
struct pnt_ctx;
/* ops == NULL selects libdrm. timeout is absolute CLOCK_MONOTONIC ns.
 * A second live context fails -EBUSY before importing anything. Even a failed
 * import returns a context when allocated; abandon the terminated session and
 * destroy it before another create. *out must initially be NULL. */
int pnt_create(struct pnt_ctx **out, int render_fd, int acquire_fd, uint64_t acquire_point,
               int release_fd, uint64_t release_point, const struct pnt_ops *ops, void *opaque);
/* protocol=0 preserves the handshake; protocol=1 requires exactly flags=0
 * and an independently proven producer release-point wait/reuse contract.
 * All explicit-sync authority/lifetime restrictions above still apply. */
int pnt_create_protocol(struct pnt_ctx **out, int render_fd, int acquire_fd, uint64_t acquire_point,
    int release_fd, uint64_t release_point, const struct pnt_ops *ops, void *opaque, int protocol);
int pnt_wait(struct pnt_ctx *, int64_t absolute_timeout_ns);
/* Worker-thread wait with <=10ms kernel wait slices. cancel is required and
 * reads a separately synchronized cancellation token; never call pnt_cancel
 * concurrently on this context. No callback may reenter this API. now returns
 * CLOCK_MONOTONIC ns, or a negative error; NULL selects clock_gettime.
 * Cancellation wins at each check, including after a successful wait/query.
 * The deadline includes interrupted retries. Neither timeout nor cancellation
 * grants permission to signal, queue or abandon a lease. Custom wait callbacks
 * must honor their absolute deadline. Cancellation after the final check still
 * needs caller-side lease validation before GPU access and publication. */
int pnt_wait_cancelable(struct pnt_ctx *, int64_t absolute_timeout_ns,
                        int (*cancel)(void *), int64_t (*now)(void *), void *opaque);
/* Query must prove release point not yet signaled immediately BEFORE access;
 * query failure or a reached point permanently fails the frame. This is not a
 * substitute for the independent authority/lifetime proofs above.
 * Call immediately BEFORE any GPU operation that could access the frame. */
int pnt_gpu_begin(struct pnt_ctx *);
/* Only after independently verified completion of ALL GPU accesses, including
 * partial submissions and failures. An uncertain completion must not call this. */
int pnt_gpu_complete(struct pnt_ctx *);
/* For a frame never submitted to any GPU; acquire must have succeeded. */
int pnt_no_gpu(struct pnt_ctx *);
/* Requires matching points and the selected protocol (UNSCHEDULED_RELEASE set
 * for protocol 0, exactly zero flags for protocol 1). Invalid metadata
 * leaves it and the state unchanged. A signal error permanently fails the
 * frame, even if the backend actually signaled; no second signal is attempted. */
int pnt_release(struct pnt_ctx *, struct spa_meta_sync_timeline *metadata);
int pnt_cancel(struct pnt_ctx *);
enum pnt_state pnt_state(const struct pnt_ctx *);
/* Explicitly abandon the promise/lease, without signaling or touching metadata.
 * This is a caller assertion, not a check: only call after independently proving
 * the entire producer session is irrevocably terminated, no producer/consumer
 * can ever reuse this buffer through that session, and ALL GPU access has ended.
 * Closing a stream, a timeout, or an error alone is not proof. PNT_GPU is refused:
 * first establish completion and call pnt_gpu_complete (also after cancellation).
 * After success no release/retry is allowed; destroy can be retried. Caller must
 * dispose of the lease according to the terminated session, never requeue it.
 * Without that proof, use only the separately documented local-disposal APIs
 * when their completion conditions hold; otherwise retain/quarantine. */
int pnt_abandon_session(struct pnt_ctx *);
/* Only DONE, ABANDONED or locally DISCARDED can be destroyed. Destroys imported handles before
 * freeing ctx and releasing the process guard; never closes borrowed fds or
 * releases the lease. On destroy failure retain ctx/guard and retry. Failed or
 * cancelled contexts first require proof-dependent abandonment OR one of the
 * local-disposal APIs below. Unknown GPU work must remain quarantined. */
int pnt_destroy(struct pnt_ctx **);
/* Local import cleanup only: requires that gpu_begin NEVER succeeded and no
 * release promise/signal was attempted. Caller must not have submitted source
 * work outside this state machine. No producer-termination assertion is made.
 * Does not signal, change metadata, or return/abandon a PipeWire lease. Caller
 * must still retain/dispose that lease separately and MUST NOT recycle it here.
 * Failed handle destruction retains ctx and guard; retry this call only. */
int pnt_discard_unsubmitted(struct pnt_ctx **);
/* Local cleanup before a release promise: also accepts GPU work independently
 * attested complete via pnt_gpu_complete. Never accepts active/unknown GPU work
 * or a past signal attempt. Caller ends all source access and disposes the lease
 * without requeue, separately. No producer termination claim or signal is made. */
int pnt_discard_before_release(struct pnt_ctx **);
/* Local reference disposal even after a release attempt. Requires no outstanding
 * GPU access (pnt_gpu_complete if begun), no access outside the tracked work,
 * and permanently ending this consumer's use of the lease. A failed signal may
 * already have taken effect: NEVER retry it, undo metadata, or requeue the lease.
 * This does not assert producer termination. Partial destruction retains ctx and
 * guard; retry only disposal. No inference from cancellation/Close is allowed. */
int pnt_discard_completed(struct pnt_ctx **);
/* Same independent proof as abandon_session. Retire a DONE context directly,
 * otherwise abandon then destroy. On any failure retain *ctx and borrowed fds;
 * retries never signal release. PNT_GPU still refuses cleanup. */
int pnt_retire_proven_session(struct pnt_ctx **ctx);
#endif
