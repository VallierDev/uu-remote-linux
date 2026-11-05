#ifndef UU_PORTAL_ENGINE_H
#define UU_PORTAL_ENGINE_H
#include <stddef.h>
#include <stdint.h>
#include "dmabuf_frame.h"
struct uu_portal;
struct uu_portal_async_lease;
struct spa_meta_sync_timeline;
typedef struct {
    UuDmaFrame pixels;
    int acquire_fd, release_fd;
    uint64_t acquire_point, release_point, epoch;
} UuPortalAsyncFrame;
/* Explicit opt-in renegotiation from an idle SHM session. Caller must have
 * independently validated producer/device/modifier and lifetime contracts.
 * No environment/default activation. Negotiation failure, reset or disable
 * requires ending this session and opening a fresh SHM session; no downgrade
 * while explicit leases exist. Success means request sent, not negotiation done. */
int uu_portal_request_explicit(struct uu_portal *, uint64_t modifier);
/* Explicit producer contract, never inferred from flags or a version string.
 * 0: UNSCHEDULED_RELEASE handshake (default). 1: flagless release-point protocol;
 * caller proves producer waits for release before any reuse and independent
 * signaling/lifetime authority. A new session is required to change protocol. */
int uu_portal_request_explicit_protocol(struct uu_portal *, uint64_t modifier, int protocol);
/* Experimental explicit-sync lease storage; no producer contract verification. Only a separately validated explicit pending frame may
 * be taken. Returned fds belong to the lease, must not be closed by the worker.
 * Keep portal alive until completion/abandon; no concurrent calls on one lease.
 * Closing with a live lease ends the session but defers object destruction;
 * after joining and abandoning the lease, call close again.
 * Negative errno results; -EPROTONOSUPPORT identifies unsupported release flags
 * and requires a fresh SHM session, never a same-session retry. No other capture
 * consumer may share the session. */
int uu_portal_async_acquire(struct uu_portal *, struct uu_portal_async_lease **, UuPortalAsyncFrame *);
int uu_portal_async_cancelled(const struct uu_portal_async_lease *);
/* gpu_complete must attest completion of ALL source access. release runs under
 * loop/frame locks and must not reenter Portal APIs. It must update the live
 * metadata before signaling, e.g. pnt_release. On ANY failure retain the lease;
 * never retry release/queue. A failed transaction must end the session. */
int uu_portal_async_finish(struct uu_portal_async_lease **, int gpu_complete,
    int (*release)(void *, struct spa_meta_sync_timeline *), void *);
/* Only after session end and independently proven GPU completion. Never queues
 * or signals. Caller joins workers before finish/abandon and portal close. */
int uu_portal_async_abandon(struct uu_portal_async_lease **, int gpu_complete);
typedef int (*uu_portal_cancelled_fn)(void *data);
typedef void (*uu_portal_save_detailed_fn)(const char *token, void *data, int has_position, int32_t x, int32_t y);
/* Reports the granted position alongside the rotated token before video initialization. */
struct uu_portal *uu_portal_open_detailed(const char *restore, uu_portal_save_detailed_fn save_token, void *save_data, uu_portal_cancelled_fn cancelled, void *cancel_data, int prefer_dmabuf, uint32_t source_types, int *error, uint32_t *width, uint32_t *height);
/* One object owns one consent/session and one PipeWire stream. */
struct uu_portal *uu_portal_open(int *error, uint32_t *width, uint32_t *height);
/* Saves the rotated token immediately after Start, before video initialization. */
struct uu_portal *uu_portal_open_virtual(const char *restore, void (*save_token)(const char *, void *), void *data, int *error, uint32_t *width, uint32_t *height);
struct uu_portal *uu_portal_open_persist(const char *restore, void (*save_token)(const char *, void *), void *data, int *error, uint32_t *width, uint32_t *height);
/* The cancellation callback and data are used only until this call returns. */
struct uu_portal *uu_portal_open_persist_cancellable(const char *restore, void (*save_token)(const char *, void *), void *save_data, uu_portal_cancelled_fn cancelled, void *cancel_data, int *error, uint32_t *width, uint32_t *height);
/* Select the capture transport before connecting, including on same-process recovery. */
struct uu_portal *uu_portal_open_persist_capture(const char *restore, void (*save_token)(const char *, void *), void *save_data, uu_portal_cancelled_fn cancelled, void *cancel_data, int prefer_dmabuf, int *error, uint32_t *width, uint32_t *height);
struct uu_portal *uu_portal_open_restore(const char *restore, int *error, uint32_t *width, uint32_t *height);
const char *uu_portal_restore_token(struct uu_portal *portal);
void uu_portal_close(struct uu_portal *portal);
int uu_portal_clipboard_ready(struct uu_portal *portal);
int uu_portal_clipboard_set(struct uu_portal *portal, const uint8_t *text, size_t length);
int uu_portal_clipboard_poll(struct uu_portal *portal, uint8_t *text, size_t capacity);
int uu_portal_alive(struct uu_portal *portal);
int uu_portal_capture_alive(struct uu_portal *portal);
void uu_portal_input_dispatch(struct uu_portal *portal);
void uu_portal_logical_size(struct uu_portal *portal, uint32_t *width, uint32_t *height);
void uu_portal_reset_frame(struct uu_portal *portal);
int uu_portal_capture(struct uu_portal *portal, uint8_t *bgra, size_t length);
int uu_portal_capture_since(struct uu_portal *portal, uint8_t *bgra, size_t length, uint64_t *revision);
/* acquire holds the PipeWire loop until release, including GPU completion.
 * 0/12 = leased new/unchanged frame, 10 = pending, 15 = neutral black.
 * CPU fallback: 14 layout/disabled, 16 shared memory, 17 timeout, 18 explicit sync, 19 modifier. */
int uu_portal_dmabuf_acquire(struct uu_portal *portal, UuDmaFrame *frame, uint64_t *revision);
void uu_portal_dmabuf_release(struct uu_portal *portal);
void uu_portal_dmabuf_disable(struct uu_portal *portal);
/* Cancellation tokens scoped to this live object. UINT64_MAX is always invalid;
 * zero is a valid initial epoch. Read the epoch while establishing a separately
 * validated lease, then poll cancelled from a worker without taking loop/frame
 * locks. Format/state changes, any buffer removal, reset, disable, errors and
 * session closure invalidate old tokens. A new token does NOT mean a stream or
 * lease is usable. These calls do not retain the object or any fd/buffer, prevent
 * producer reuse, or make check-and-publish atomic. Caller must join every token
 * reader and lease user before uu_portal_close; do not reuse tokens on a new
 * object. Existing VAAPI acquire/release locking is unchanged. */
uint64_t uu_portal_capture_epoch(struct uu_portal *portal);
int uu_portal_capture_cancelled(struct uu_portal *portal, uint64_t epoch);
int uu_portal_move(struct uu_portal *portal, double x, double y, int relative);
int uu_portal_button(struct uu_portal *portal, int button, int pressed);
int uu_portal_scroll(struct uu_portal *portal, double dx, double dy, int finish);
int uu_portal_touch_supported(struct uu_portal *portal);
int uu_portal_touch_available(struct uu_portal *portal);
int uu_portal_touch(struct uu_portal *portal, int phase, double x, double y);
int uu_portal_key(struct uu_portal *portal, uint32_t keysym, int keycode, int pressed);
int uu_portal_stream_position(struct uu_portal *portal, int32_t *x, int32_t *y);
int uu_portal_monitors(char *out, size_t length);
/* Cancel locally, then request remote Close (at most 3 seconds). Returns 0 only
 * for a Close reply or an observed Closed signal, -EBUSY for another in-flight
 * request, -ENOTCONN without a session, -EIO on D-Bus failure. Failed requests
 * may be retried; success is cached. None of these outcomes proves GPU completion,
 * producer buffer non-reuse, or permission to abandon a timeline. Keep the object
 * alive throughout; do not race final close. The void wrapper discards the result. */
int uu_portal_end_checked(struct uu_portal *portal);
/* Returns -ECANCELED if Closed was observed before our first local end request,
 * otherwise 0 (-EINVAL for NULL). Sticky across cleanup and Close retries.
 * This records observation order, not the user's intent: a Closed racing AFTER
 * local end is indistinguishable from its acknowledgment. No lifetime proof. */
int uu_portal_recovery_status(struct uu_portal *portal);
/* After local session end, joined workers and disposal of every GPU lease.
 * Disconnect only this consumer stream; not a producer/GPU termination proof.
 * Refuses live sessions or outstanding leases. Idempotent after success. */
int uu_portal_disconnect_capture(struct uu_portal *portal);
void uu_portal_end(struct uu_portal *portal);
#endif
