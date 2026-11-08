#define _POSIX_C_SOURCE 200809L
#include "portal_nvenc_fence.h"

#include <errno.h>
#include <limits.h>
#include <linux/dma-buf.h>
#include <linux/sync_file.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <unistd.h>

static int real_ioctl(int fd, unsigned long request, void *arg) { return ioctl(fd, request, arg); }
static int real_clock(struct timespec *ts) { return clock_gettime(CLOCK_MONOTONIC, ts); }
static const struct portal_nvenc_fence_ops real_ops = { real_ioctl, poll, close, real_clock };

/* Round up so a sub-millisecond remainder is not treated as expired. */
static int remaining(const struct portal_nvenc_fence_ops *ops, const struct timespec *end)
{
    struct timespec now;
    int64_t ns;
    if (ops->clock_fn(&now) || now.tv_nsec < 0 || now.tv_nsec >= 1000000000L)
        return -1;
    if (now.tv_sec > end->tv_sec || (now.tv_sec == end->tv_sec && now.tv_nsec >= end->tv_nsec))
        return 0;
    /* The deadline is at most INT_MAX milliseconds after the initial time. */
    if (now.tv_sec < end->tv_sec - (int64_t)INT_MAX / 1000 - 2)
        return -1; /* clock moved backwards */
    ns = (int64_t)(end->tv_sec - now.tv_sec) * 1000000000 + end->tv_nsec - now.tv_nsec;
    return ns / 1000000 + (ns % 1000000 != 0) > INT_MAX ? INT_MAX :
        (int)(ns / 1000000 + (ns % 1000000 != 0));
}

/* Check cancellation even when the deadline has expired. A cancellation poll
 * interrupted repeatedly is itself bounded by the same monotonic deadline. */
static enum portal_nvenc_fence_result check_budget(
    const struct portal_nvenc_fence_ops *ops, const struct timespec *end, int cancel_fd)
{
    int r, ms;
    struct pollfd p = { .fd = cancel_fd, .events = POLLIN };
    if (cancel_fd >= 0) {
        for (;;) {
            p.revents = 0;
            r = ops->poll_fn(&p, 1, 0);
            if (r > 0) return p.revents ? PORTAL_FENCE_CANCELLED : PORTAL_FENCE_ERROR;
            if (!r) break;
            if (errno != EINTR) return PORTAL_FENCE_ERROR;
            ms = remaining(ops, end);
            if (ms <= 0) return ms ? PORTAL_FENCE_ERROR : PORTAL_FENCE_TIMEOUT;
        }
    }
    ms = remaining(ops, end);
    return ms < 0 ? PORTAL_FENCE_ERROR : ms == 0 ? PORTAL_FENCE_TIMEOUT : PORTAL_FENCE_SNAPSHOT_COMPLETED;
}

enum portal_nvenc_fence_result portal_nvenc_fence_wait(
    int dmabuf_fd, int cancel_fd, int timeout_ms, int contract_verified, const struct portal_nvenc_fence_ops *ops)
{
    struct dma_buf_export_sync_file export = { .flags = DMA_BUF_SYNC_READ, .fd = -1 };
    struct sync_file_info info = {0};
    struct timespec end;
    struct pollfd fds[2];
    enum portal_nvenc_fence_result result = PORTAL_FENCE_ERROR;
    int r, ms, count = cancel_fd >= 0 ? 2 : 1;
    if (contract_verified != 1) return PORTAL_FENCE_CONTRACT_UNVERIFIED;
    if (!ops) ops = &real_ops;
    if (dmabuf_fd < 0 || cancel_fd < -1 || timeout_ms < 0 || !ops->ioctl_fn ||
        !ops->poll_fn || !ops->close_fn || !ops->clock_fn || ops->clock_fn(&end) ||
        end.tv_sec < 0 || end.tv_nsec < 0 || end.tv_nsec >= 1000000000L)
        return result;
    /* Avoid overflowing time_t without relying on its signedness or width. */
    if ((uint64_t)end.tv_sec > (uint64_t)INT64_MAX - (uint64_t)timeout_ms / 1000 - 1)
        return result;
    {
        int64_t sec = (int64_t)end.tv_sec + timeout_ms / 1000;
        long nsec = end.tv_nsec + (long)(timeout_ms % 1000) * 1000000L;
        if (nsec >= 1000000000L) { sec++; nsec -= 1000000000L; }
        end.tv_sec = (time_t)sec;
        if ((int64_t)end.tv_sec != sec) return result;
        end.tv_nsec = nsec;
    }
    for (;;) {
        result = check_budget(ops, &end, cancel_fd);
        if (result != PORTAL_FENCE_SNAPSHOT_COMPLETED) return result;
        result = PORTAL_FENCE_ERROR;
        r = ops->ioctl_fn(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &export);
        if (!r) break;
        if (errno != EINTR) return (errno == ENOTTY || errno == EOPNOTSUPP || errno == ENOSYS) ? PORTAL_FENCE_UNSUPPORTED : result;
    }
    if (export.fd < 0) return result;
    fds[0] = (struct pollfd){ .fd = export.fd, .events = POLLIN };
    fds[1] = (struct pollfd){ .fd = cancel_fd, .events = POLLIN };
    for (;;) {
        result = check_budget(ops, &end, cancel_fd);
        if (result != PORTAL_FENCE_SNAPSHOT_COMPLETED) break;
        result = PORTAL_FENCE_ERROR;
        ms = remaining(ops, &end);
        if (ms < 0) break;
        r = ops->poll_fn(fds, (nfds_t)count, ms);
        if (r < 0) {
            if (errno == EINTR) continue;
            break;
        }
        if (!r) { result = PORTAL_FENCE_TIMEOUT; break; }
        if (count == 2 && fds[1].revents) { result = PORTAL_FENCE_CANCELLED; break; }
        if (fds[0].revents & (POLLERR | POLLHUP | POLLNVAL)) break;
        if (!(fds[0].revents & POLLIN)) break;
        for (;;) {
            result = check_budget(ops, &end, cancel_fd);
            if (result != PORTAL_FENCE_SNAPSHOT_COMPLETED) goto out;
            result = PORTAL_FENCE_ERROR;
            r = ops->ioctl_fn(export.fd, SYNC_IOC_FILE_INFO, &info);
            if (!r) break;
            if (errno != EINTR) goto out;
        }
        if (!info.num_fences) result = PORTAL_FENCE_EMPTY;
        else if (info.status > 0) result = check_budget(ops, &end, cancel_fd);
        break;
    }
out:
    /* Linux close(EINTR) must not be retried: the fd may already be reused. */
    if (ops->close_fn(export.fd)) result = PORTAL_FENCE_ERROR;
    return result;
}
