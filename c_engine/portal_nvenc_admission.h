#ifndef PORTAL_NVENC_ADMISSION_H
#define PORTAL_NVENC_ADMISSION_H
#include <stdint.h>
#include <pthread.h>

/* Isolated experiment, NOT a DMA-BUF importer or synchronization primitive.
 * Never connected to the production build. All attestations are assertions by
 * a trusted caller, not evidence verified here. Caller must pin descriptors,
 * metadata, lease, format, identity and timeline authority across every call;
 * exclude foreign signals (including pending signals), reuse and mutation, and
 * serialize external GPU/signal/queue operations with revocation. The mutex
 * only serializes this object's bookkeeping, not operations outside it.
 * No fork, copied objects, concurrent destruction, or multiple registries for
 * the same producer session. Initialize once per irrevocably unique session;
 * destroying/reinitializing loses replay history. Each buffer ID is a caller-
 * assigned, nonzero session-unique registration identity, never a raw fd, pointer
 * or recyclable PipeWire ID. Caller must attest a stable one-to-one mapping of
 * buffer ID to its actual syncobj for the whole session, no aliasing between IDs,
 * and identical acquire/release syncobj. IDs are never reused, even on removal.
 * Generations and timeline points must increase per buffer, without wraparound.
 * A descriptor number or equality of caller-supplied IDs proves none of this.
 * If any guarantee cannot be established, do not use this component.
 * No function signals, queues, imports, waits, or frees a lease. Successful release
 * is merely bookkeeping; an integration needs its own atomicity and failure
 * protocol (see portal_nvenc_timeline.h), and must not rely on this API alone. */
enum pna_state { PNA_IDLE, PNA_LEASED, PNA_ACQUIRED, PNA_GPU, PNA_READY, PNA_RELEASED, PNA_REVOKED, PNA_ABANDONED };
#define PNA_MAX_BUFFERS 64
struct pna_history { uint64_t id, generation, last_release; };
struct pna_session { pthread_mutex_t lock; struct pna_history history[PNA_MAX_BUFFERS];
    uint64_t active_id, generation; enum pna_state state; int gpu_pending; };
/* Exactly one packed 4-byte RGB plane. Only width, height, fourcc and
 * modifier are compared to the negotiated snapshot; stride, offsets and sizes
 * are range-checked, not compared against negotiated layout. fd validity,
 * object size and metadata authenticity are caller attestations, not fstat/ioctl
 * checks. No crop, corruption or empty. Nonlinear layouts require the separate
 * per-object, per-generation attestation below; linear arithmetic proves nothing
 * about tiled allocation size. */
struct pna_frame {
    uint64_t buffer_id, generation, acquire, release, modifier;
    uint32_t width, height, fourcc, planes, stride;
    uint64_t offset, chunk_size, allocation_size;
    int fd, dmabuf, metadata_valid, unscheduled_release, full_frame;
};
struct pna_evidence {
    int explicit_opt_in, negotiated, mutter_same_gpu, trusted_producer;
    int exclusive_timeline, pinned_lease_and_metadata, verified_fd_and_size;
    int stable_unique_buffer_timeline;
    uint32_t width, height, fourcc; uint64_t modifier;
    /* Exactly 1 only after an independent trusted step verified the complete
     * tiled layout, actual fd object size and target device support for this
     * buffer object and current generation, including these exact metadata and
     * modifier. This module does not verify any of those claims. Never reuse
     * this attestation for changed metadata, object, generation or device. */
    int verified_tiled_layout_fd_size_and_device;
};
int pna_init(struct pna_session *);
/* Failed admission changes nothing; success consumes generation and points even
 * if subsequently revoked. Only one active frame per session; histories never
 * evict. Registry exhaustion fails closed. Caller serializes signal/queue and
 * must finish those operations before admitting another frame. */
int pna_admit(struct pna_session *, const struct pna_frame *, const struct pna_evidence *);
int pna_acquired(struct pna_session *, uint64_t buffer_id, uint64_t generation, int wait_and_release_pending_verified);
/* Call before ANY operation that might submit GPU work, including import. */
int pna_gpu_begin(struct pna_session *, uint64_t buffer_id, uint64_t generation, int release_pending_verified);
/* Only after independently verifying ALL work, including partial failure, ended. */
int pna_gpu_complete(struct pna_session *, uint64_t buffer_id, uint64_t generation, int all_gpu_complete);
int pna_no_gpu(struct pna_session *, uint64_t buffer_id, uint64_t generation, int release_pending_verified);
/* Completion and continued authority must be independently attested immediately
 * before release; no external operation is performed by this function. */
int pna_release(struct pna_session *, uint64_t buffer_id, uint64_t generation, int authority_and_pending_verified);
/* Irreversible; never signals or returns the buffer. GPU completion still needed. */
int pna_revoke(struct pna_session *, uint64_t buffer_id, uint64_t generation);
/* Caller asserts irrevocable session termination, no future lease reuse, and GPU
 * quiescence. Otherwise quarantine indefinitely. Never queues/signals. */
int pna_abandon(struct pna_session *, uint64_t buffer_id, uint64_t generation, int terminated_and_no_reuse);
/* Only idle/released or safely abandoned session; caller guarantees no races. */
int pna_destroy(struct pna_session *);
#endif
