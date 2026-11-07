#define _POSIX_C_SOURCE 200809L
#include "../c_engine/portal_nvenc_fence.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/dma-buf.h>
#include <linux/sync_file.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static int export_error, info_error, poll_error, close_error, interrupts;
static int status, fences, event, cancel_event, closed, calls, tick, cancel_after_calls, info_interrupts, info_advance;
static int fake_ioctl(int fd, unsigned long req, void *arg)
{
    calls++;
    if (interrupts > 0) { interrupts--; errno = EINTR; return -1; }
    if (req == DMA_BUF_IOCTL_EXPORT_SYNC_FILE) {
        struct dma_buf_export_sync_file *e = arg;
        assert(fd == 3 && e->flags == DMA_BUF_SYNC_READ);
        if (export_error) { errno = export_error; return -1; }
        e->fd = 9; return 0;
    }
    assert(fd == 9 && req == SYNC_IOC_FILE_INFO);
    if (info_interrupts > 0) { info_interrupts--; errno = EINTR; return -1; }
    if (info_error) { errno = info_error; return -1; }
    tick += info_advance;
    ((struct sync_file_info *)arg)->status = status;
    ((struct sync_file_info *)arg)->num_fences = fences;
    return 0;
}
static int fake_poll(struct pollfd *p, nfds_t n, int ms)
{
    assert(ms >= 0);
    if (n == 1 && p[0].fd == 4) {
        p[0].revents = cancel_event || (cancel_after_calls && calls >= cancel_after_calls) ? POLLIN : 0;
        return p[0].revents ? 1 : 0;
    }
    assert(p[0].fd == 9);
    if (poll_error) { errno = poll_error; return -1; }
    p[0].revents = event;
    if (n == 2) p[1].revents = cancel_event;
    return event || (n == 2 && cancel_event) ? 1 : 0;
}
static int fake_close(int fd) { assert(fd == 9); closed++; if (close_error) { errno = EINTR; return -1; } return 0; }
static int fake_clock(struct timespec *ts) { ts->tv_sec = 1 + tick / 1000; ts->tv_nsec = (long)(tick++ % 1000) * 1000000; return 0; }
static const struct portal_nvenc_fence_ops ops = { fake_ioctl, fake_poll, fake_close, fake_clock };
static void reset(void)
{
    export_error = info_error = poll_error = close_error = interrupts = 0;
    status = fences = 1; event = POLLIN; cancel_event = closed = calls = tick = 0;
    cancel_after_calls = info_interrupts = info_advance = 0;
}
static void check(enum portal_nvenc_fence_result expected, int closes)
{
    assert(portal_nvenc_fence_wait(3, 4, 100, 1, &ops) == expected);
    assert(closed == closes);
}
int main(void)
{
    int fd;
    reset(); check(PORTAL_FENCE_SNAPSHOT_COMPLETED, 1);
    reset(); fences = 0; check(PORTAL_FENCE_EMPTY, 1);
    reset(); status = -EIO; check(PORTAL_FENCE_ERROR, 1);
    reset(); status = 0; check(PORTAL_FENCE_ERROR, 1);
    reset(); export_error = ENOTTY; check(PORTAL_FENCE_UNSUPPORTED, 0);
    reset(); export_error = EIO; check(PORTAL_FENCE_ERROR, 0);
    reset(); info_error = EIO; check(PORTAL_FENCE_ERROR, 1);
    reset(); poll_error = EIO; check(PORTAL_FENCE_ERROR, 1);
    reset(); poll_error = EINTR; check(PORTAL_FENCE_TIMEOUT, 1);
    reset(); event = 0; check(PORTAL_FENCE_TIMEOUT, 1);
    reset(); cancel_event = POLLIN; check(PORTAL_FENCE_CANCELLED, 0);
    reset(); event = POLLNVAL; check(PORTAL_FENCE_ERROR, 1);
    reset(); close_error = 1; check(PORTAL_FENCE_ERROR, 1);
    reset(); interrupts = 2; check(PORTAL_FENCE_SNAPSHOT_COMPLETED, 1);
    reset(); interrupts = 1000; check(PORTAL_FENCE_TIMEOUT, 0);
    reset(); interrupts = 1000; cancel_after_calls = 1; check(PORTAL_FENCE_CANCELLED, 0);
    reset(); info_interrupts = 1000; cancel_after_calls = 2; check(PORTAL_FENCE_CANCELLED, 1);
    reset(); cancel_after_calls = 2; check(PORTAL_FENCE_CANCELLED, 1);
    reset(); info_advance = 100; check(PORTAL_FENCE_TIMEOUT, 1);
    reset(); info_advance = 100; cancel_after_calls = 2; check(PORTAL_FENCE_CANCELLED, 1);
    /* A signaled one-fence stub is indistinguishable from a real fence. */
    reset(); assert(portal_nvenc_fence_wait(3, 4, 100, 0, &ops) == PORTAL_FENCE_CONTRACT_UNVERIFIED);
    assert(calls == 0 && closed == 0);
    reset(); fences = 1; status = 1; check(PORTAL_FENCE_SNAPSHOT_COMPLETED, 1);
    fd = open("/dev/null", O_RDONLY | O_CLOEXEC); assert(fd >= 0);
    assert(portal_nvenc_fence_wait(fd, -1, 50, 1, NULL) == PORTAL_FENCE_UNSUPPORTED);
    assert(fcntl(fd, F_GETFD) >= 0); close(fd);
    puts("portal_nvenc_fence: ok");
    return 0;
}
