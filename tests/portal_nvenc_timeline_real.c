/* Synthetic syncobjs only: no Portal, screen, GPU buffers or GPU work. This
 * does NOT prove actual Portal messages, producer behavior or sync contracts.
 * From the repository root:
 * cc -std=gnu11 -Wall -Wextra -Werror -g -fsanitize=address,undefined \
 *   tests/portal_nvenc_timeline_real.c c_engine/portal_nvenc_timeline.c \
 *   $(pkg-config --cflags --libs libdrm libspa-0.2) -o /tmp/pnt-real
 * ASAN_OPTIONS=detect_leaks=1 /tmp/pnt-real
 * Exit 0: pass, 77: NVIDIA render node / required syncobj support unavailable,
 * 1: failure (including cleanup failure). No elevated privileges are needed. */
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <xf86drm.h>
#include <drm.h>
#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int errnum(int r) { return r == -1 ? (errno ? errno : EIO) : (r < 0 ? -r : EIO); }
static int unsupported(int e) { return e == ENOTTY || e == EOPNOTSUPP || e == ENOSYS || e == EINVAL; }
static int64_t deadline(int ms) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec + (int64_t)ms * 1000000;
}
#define CHECK(x) do { if (!(x)) { fprintf(stderr, "FAIL line %d: %s\n", __LINE__, #x); goto done; } } while (0)
#define DRM(x) do { errno=0; r=(x); if (r) { fprintf(stderr, "FAIL line %d: %s: %s\n", __LINE__, #x, strerror(errnum(r))); goto done; } } while (0)

int main(void) {
    glob_t paths = {0};
    int a=-1, b=-1, obj=-1, have=0, status=1, r, e, globbed=0;
    uint32_t h=0;
    uint64_t p=0, current=0;
    int64_t until;
    struct pnt_ctx *ctx=NULL;
    struct spa_meta_sync_timeline meta = {0};
    const char *path=NULL;
    drmVersionPtr v;

    r=glob("/dev/dri/renderD*", 0, NULL, &paths);
    if (r == GLOB_NOMATCH) { fprintf(stderr,"UNSUPPORTED: no render node\n"); return 77; }
    if (r) { fprintf(stderr,"FAIL: render node enumeration\n"); return 1; }
    globbed=1;
    for (size_t i=0; i<paths.gl_pathc; i++) {
        a=open(paths.gl_pathv[i], O_RDWR|O_CLOEXEC);
        if (a<0) continue;
        v=drmGetVersion(a);
        if (v && ((v->name_len==6 && !memcmp(v->name,"nvidia",6)) ||
                  (v->name_len==10 && !memcmp(v->name,"nvidia-drm",10)))) {
            drmFreeVersion(v); path=paths.gl_pathv[i]; break;
        }
        if (v) drmFreeVersion(v);
        if (close(a)) { perror("close candidate"); a=-1; goto done; }
        a=-1;
    }
    if (!path) { fprintf(stderr,"UNSUPPORTED: no accessible NVIDIA render node\n"); status=77; goto done; }
    printf("NVIDIA render node: %s\n", path);
    b=open(path,O_RDWR|O_CLOEXEC);
    CHECK(b>=0);
    errno=0; r=drmSyncobjCreate(a,0,&h);
    if (r && unsupported(errnum(r))) { fprintf(stderr,"UNSUPPORTED: syncobj create: %s\n",strerror(errnum(r))); status=77; goto done; }
    CHECK(!r); have=1;
    DRM(drmSyncobjHandleToFD(a,h,&obj));
    /* Probe timeline support before creating a pnt context. */
    errno=0; r=drmSyncobjQuery(a,&h,&current,1);
    if (r && unsupported(errnum(r))) { fprintf(stderr,"UNSUPPORTED: timeline query: %s\n",strerror(errnum(r))); status=77; goto done; }
    CHECK(!r && current==0);
    p=1; errno=0; r=drmSyncobjTimelineSignal(a,&h,&p,1);
    if (r && unsupported(errnum(r))) { fprintf(stderr,"UNSUPPORTED: timeline signal: %s\n",strerror(errnum(r))); status=77; goto done; }
    CHECK(!r);
    until=deadline(1000); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h,&p,1,until,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL);
    if (r && unsupported(errnum(r))) { fprintf(stderr,"UNSUPPORTED: timeline wait: %s\n",strerror(errnum(r))); status=77; goto done; }
    CHECK(!r);

    /* Producer owns a; pnt imports the same syncobj fd twice on b. */
    CHECK(pnt_create(&ctx,b,obj,2,obj,3,NULL,NULL)==0);
    p=2; DRM(drmSyncobjTimelineSignal(a,&h,&p,1));
    until=deadline(1000); CHECK(until>=0);
    CHECK(pnt_wait(ctx,until)==0 && pnt_state(ctx)==PNT_ACQUIRED);
    CHECK(pnt_no_gpu(ctx)==0);
    meta.acquire_point=2; meta.release_point=3;
    meta.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    CHECK(pnt_release(ctx,&meta)==0 && pnt_state(ctx)==PNT_DONE);
    CHECK(!(meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
    p=3; until=deadline(1000); CHECK(until>=0);
    DRM(drmSyncobjTimelineWait(a,&h,&p,1,until,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL));
    DRM(drmSyncobjQuery(a,&h,&current,1)); CHECK(current>=3);
    CHECK(pnt_destroy(&ctx)==0 && !ctx);

    /* An unsignaled acquire must never release, even after timeout. */
    CHECK(pnt_create(&ctx,b,obj,4,obj,5,NULL,NULL)==0);
    until=deadline(20); CHECK(until>=0);
    r=pnt_wait(ctx,until); CHECK((r==-ETIME || r==-ETIMEDOUT) && pnt_state(ctx)==PNT_FAILED);
    meta.acquire_point=4; meta.release_point=5; meta.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    CHECK(pnt_no_gpu(ctx)!=0 && pnt_release(ctx,&meta)!=0);
    CHECK(meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE);
    DRM(drmSyncobjQuery(a,&h,&current,1)); CHECK(current==3);
    CHECK(pnt_destroy(&ctx)==-EBUSY);
    /* Synthetic producer is this thread: it has stopped irrevocably for this
     * simulated session, no buffer exists, and no GPU work was submitted. */
    CHECK(pnt_abandon_session(ctx)==0); CHECK(pnt_destroy(&ctx)==0 && !ctx);

    /* Reused / backwards releases are rejected against the real timeline. */
    for (uint64_t release=2; release<=3; release++) {
        CHECK(pnt_create(&ctx,b,obj,1,obj,release,NULL,NULL)==0);
        until=deadline(1000); CHECK(until>=0);
        CHECK(pnt_wait(ctx,until)==-ESTALE && pnt_state(ctx)==PNT_FAILED);
        meta.acquire_point=1; meta.release_point=release; meta.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
        CHECK(pnt_no_gpu(ctx)!=0 && pnt_release(ctx,&meta)!=0);
        CHECK(meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE);
        DRM(drmSyncobjQuery(a,&h,&current,1)); CHECK(current==3);
        CHECK(pnt_abandon_session(ctx)==0); CHECK(pnt_destroy(&ctx)==0 && !ctx);
    }
    puts("PASS: synthetic real-ops timeline; Portal contract not tested"); status=0;
done:
    /* The test owns the entire synthetic producer and submits no GPU work.
     * On any failure it stops permanently here; no lease or buffer can be reused. */
    if (ctx) {
        if (pnt_state(ctx)!=PNT_DONE && pnt_state(ctx)!=PNT_ABANDONED) {
            r=pnt_abandon_session(ctx);
            if (r) { fprintf(stderr,"FAIL: cleanup abandon: %d\n",r); status=1; }
        }
        r=pnt_destroy(&ctx);
        if (r) { fprintf(stderr,"FAIL: cleanup pnt_destroy: %d (context quarantined)\n",r); status=1; }
    }
    if (obj>=0 && close(obj)) { perror("close syncobj fd"); status=1; }
    if (have) { errno=0; r=drmSyncobjDestroy(a,h); if (r) { e=errnum(r); fprintf(stderr,"FAIL: destroy producer handle: %s\n",strerror(e)); status=1; } }
    if (b>=0 && close(b)) { perror("close consumer DRM fd"); status=1; }
    if (a>=0 && close(a)) { perror("close producer DRM fd"); status=1; }
    if (globbed) globfree(&paths);
    return status;
}
