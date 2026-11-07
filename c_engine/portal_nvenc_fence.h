#ifndef PORTAL_NVENC_FENCE_H
#define PORTAL_NVENC_FENCE_H

#include <poll.h>
#include <time.h>

/* Experimental only. snapshot completed means the exported READ sync_file
 * reports completion without error; even a nonempty sync_file may contain a
 * kernel-created stub for an empty reservation. No count or status proves a
 * producer contract or excludes future writes. The caller must independently
 * validate the applicable producer contract and pass contract_verified=1;
 * otherwise the operation fails closed without touching any fd.
 * No fd ownership is transferred except the exported sync_file, closed exactly
 * once. cancel_fd is borrowed, -1 disables cancellation; any event cancels.
 * timeout_ms is a finite, nonnegative budget including interrupted retries.
 * Cancellation takes precedence over timeout at each check. Completion is
 * accepted only if a final cancellation check finds no event and the deadline
 * has not expired. Cancellation arriving after that check may not be observed.
 */
enum portal_nvenc_fence_result {
    PORTAL_FENCE_SNAPSHOT_COMPLETED, PORTAL_FENCE_TIMEOUT, PORTAL_FENCE_CANCELLED,
    PORTAL_FENCE_UNSUPPORTED, PORTAL_FENCE_EMPTY, PORTAL_FENCE_ERROR,
    PORTAL_FENCE_CONTRACT_UNVERIFIED
};

/* Injectable syscall boundary. Functions follow libc return/errno conventions.
 * NULL ops selects real syscalls. All callbacks are required otherwise. */
struct portal_nvenc_fence_ops {
    int (*ioctl_fn)(int, unsigned long, void *);
    int (*poll_fn)(struct pollfd *, nfds_t, int);
    int (*close_fn)(int);
    int (*clock_fn)(struct timespec *);
};

enum portal_nvenc_fence_result portal_nvenc_fence_wait(
    int dmabuf_fd, int cancel_fd, int timeout_ms, int contract_verified,
    const struct portal_nvenc_fence_ops *ops);
#endif
