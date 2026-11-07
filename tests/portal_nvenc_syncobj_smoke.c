/* Standalone synthetic syncobj test; does not connect to Portal or read pixels.
 * cc -std=gnu11 -Wall -Wextra -Werror tests/portal_nvenc_syncobj_smoke.c \
 *   $(pkg-config --cflags --libs libdrm) -o /tmp/pnt-syncobj-smoke
 * /tmp/pnt-syncobj-smoke
 * Exit 0: passed, 77: unavailable/unsupported, 1: unexpected failure. */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <drm.h>

static int errnum(int r) { return r == -1 ? (errno ? errno : EIO) : (r < 0 ? -r : EIO); }
static int unavailable(int e) { return e == ENOENT || e == ENODEV || e == EACCES || e == EPERM || e == ENOTTY || e == EOPNOTSUPP || e == ENOSYS || e == EINVAL; }
static int failure(const char *step, int e) {
    int skip = unavailable(e);
    fprintf(stderr, "%s: %s (errno=%d: %s)\n", skip ? "UNAVAILABLE" : "FAIL", step, e, strerror(e));
    return skip ? 77 : 1;
}
static int64_t deadline_ms(int ms) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * INT64_C(1000000000) + t.tv_nsec + (int64_t)ms * 1000000;
}
int main(void) {
    const char *path = "/dev/dri/renderD128";
    int a=-1, b=-1, obj=-1, have_a=0, have_b=0, have_c=0, status=1, r, e;
    uint32_t ha=0, hb=0, hc=0;
    uint64_t point=7, current=0;
    int64_t deadline;
    drmVersionPtr version;
    a=open(path,O_RDWR|O_CLOEXEC);
    if (a<0) return failure("open renderD128", errno);
    version=drmGetVersion(a);
    if (!version) { status=failure("DRM version", errno ? errno : EIO); goto done; }
    printf("DRM driver: %.*s\n", version->name_len, version->name);
    if (!((version->name_len == 6 && !memcmp(version->name,"nvidia",6)) ||
          (version->name_len == 10 && !memcmp(version->name,"nvidia-drm",10)))) {
        fprintf(stderr,"UNAVAILABLE: renderD128 is not NVIDIA\n"); drmFreeVersion(version); status=77; goto done;
    }
    drmFreeVersion(version);
    b=open(path,O_RDWR|O_CLOEXEC);
    if (b<0) { status=failure("second renderD128 open",errno); goto done; }
    errno=0; r=drmSyncobjCreate(a,0,&ha);
    if (r) { status=failure("syncobj create",errnum(r)); goto done; }
    have_a=1; puts("syncobj create: ok");
    errno=0; r=drmSyncobjHandleToFD(a,ha,&obj);
    if (r) { status=failure("handle to fd",errnum(r)); goto done; }
    puts("handle to fd: ok");
    errno=0; r=drmSyncobjFDToHandle(b,obj,&hb);
    if (r) { status=failure("fd to handle",errnum(r)); goto done; }
    have_b=1; puts("fd to handle on second DRM open: ok");
    /* Mirror acquire/release: import the same exported fd twice on one DRM open. */
    errno=0; r=drmSyncobjFDToHandle(b,obj,&hc);
    if (r) { status=failure("second fd to handle on same DRM open",errnum(r)); goto done; }
    have_c=1;
    printf("same DRM fd imports: %" PRIu32 " and %" PRIu32 " (%s)\n",hb,hc,hb==hc ? "same" : "different");
    errno=0; r=drmSyncobjDestroy(b,hc);
    if (r) { fprintf(stderr,"FAIL: second imported handle destroy: %s\n",strerror(errnum(r))); goto done; }
    have_c=0;
    errno=0; r=drmSyncobjQuery(b,&hb,&current,1);
    if (hb==hc) {
        if (!r) { fprintf(stderr,"FAIL: identical handle survived first destroy\n"); goto done; }
        printf("identical handle query after destroy: errno=%d (%s)\n",errnum(r),strerror(errnum(r)));
        errno=0; r=drmSyncobjDestroy(b,hb);
        if (!r) { have_b=0; fprintf(stderr,"FAIL: identical handle destroyed twice successfully\n"); goto done; }
        e=errnum(r); have_b=0;
        printf("identical handle second destroy: errno=%d (%s)\n",e,strerror(e));
        errno=0; r=drmSyncobjFDToHandle(b,obj,&hb);
        if (r) { status=failure("reimport after alias destroy",errnum(r)); goto done; }
        have_b=1;
    } else if (r) {
        fprintf(stderr,"FAIL: first handle invalid after destroying distinct second handle: %s\n",strerror(errnum(r))); goto done;
    } else puts("first handle remains valid after distinct second handle destroy: ok");
    errno=0; r=drmSyncobjQuery(b,&hb,&current,1);
    if (r) { status=failure("initial timeline query",errnum(r)); goto done; }
    if (current>=point) { fprintf(stderr,"FAIL: fresh timeline already reached point 7\n"); goto done; }
    printf("initial queried point: %" PRIu64 "\n",current);
    deadline=deadline_ms(100);
    if (deadline<0) { status=failure("monotonic clock",errno); goto done; }
    errno=0; r=drmSyncobjTimelineWait(b,&hb,&point,1,deadline,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL);
    e=r ? errnum(r) : 0;
    if (e!=ETIME && e!=ETIMEDOUT) {
        if (r) status=failure("unsignaled timeline wait",e);
        else { fprintf(stderr,"FAIL: unsignaled timeline wait unexpectedly passed\n"); status=1; }
        goto done;
    }
    puts("unsignaled point 7: timed out as expected");
    errno=0; r=drmSyncobjTimelineSignal(a,&ha,&point,1);
    if (r) { status=failure("timeline signal",errnum(r)); goto done; }
    puts("signal point 7: ok");
    deadline=deadline_ms(1000);
    if (deadline<0) { status=failure("monotonic clock",errno); goto done; }
    errno=0; r=drmSyncobjTimelineWait(b,&hb,&point,1,deadline,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL);
    if (r) { status=failure("signaled timeline wait",errnum(r)); goto done; }
    puts("wait point 7: ok");
    errno=0; r=drmSyncobjQuery(b,&hb,&current,1);
    if (r) { status=failure("signaled timeline query",errnum(r)); goto done; }
    if (current<point) { fprintf(stderr,"FAIL: query below signaled point\n"); goto done; }
    printf("queried signaled point: %" PRIu64 "\n",current); status=0;
done:
    if (have_c) { errno=0; r=drmSyncobjDestroy(b,hc); if (r) { fprintf(stderr,"FAIL: second imported handle cleanup: %s\n",strerror(errnum(r))); status=1; } else if (have_b && hb==hc) have_b=0; }
    if (have_b) { errno=0; r=drmSyncobjDestroy(b,hb); if (r) { fprintf(stderr,"FAIL: imported handle destroy: %s\n",strerror(errnum(r))); status=1; } else puts("imported handle destroy: ok"); }
    if (obj>=0 && close(obj)) { perror("close syncobj fd"); status=1; }
    if (have_a) { errno=0; r=drmSyncobjDestroy(a,ha); if (r) { fprintf(stderr,"FAIL: original handle destroy: %s\n",strerror(errnum(r))); status=1; } else puts("original handle destroy: ok"); }
    if (b>=0 && close(b)) { perror("close second DRM fd"); status=1; }
    if (a>=0 && close(a)) { perror("close DRM fd"); status=1; }
    return status;
}
