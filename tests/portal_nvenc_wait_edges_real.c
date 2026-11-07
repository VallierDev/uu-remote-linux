/* Kernel sync primitive observations only, using exclusively owned synthetic
 * syncobjs. No Portal, GPU buffers or GPU work: success or failure here says
 * nothing about a real producer's contract or completion of a current frame.
 * cc -std=gnu11 -Wall -Wextra -Werror -g tests/portal_nvenc_wait_edges_real.c \
 *   $(pkg-config --cflags --libs libdrm) -pthread -o /tmp/wait-edges
 * Add -fsanitize=address,undefined for the sanitizer build. Exit 77 means
 * the NVIDIA render node or a required syncobj operation is unsupported. */
#include <xf86drm.h>
#include <drm.h>
#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static int error_of(int r) { return r == -1 ? (errno ? errno : EIO) : (r < 0 ? -r : 0); }
static int unsupported(int e) { return e == ENOTTY || e == EOPNOTSUPP || e == ENOSYS || e == EINVAL; }
static int64_t deadline(int ms) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec + (int64_t)ms * 1000000;
}
static void observation(const char *name, int r, int e) {
    printf("%s: return=%d errno=%d (%s)\n", name, r, e, e ? strerror(e) : "none");
}
struct producer { int fd; uint32_t handle; int r, e; };
static void *signal_later(void *arg) {
    struct producer *p=arg;
    struct timespec delay={0, 100000000};
    while (nanosleep(&delay,&delay) && errno==EINTR) {}
    uint64_t high=2;
    errno=0; p->r=drmSyncobjTimelineSignal(p->fd,&p->handle,&high,1); p->e=error_of(p->r);
    return NULL;
}
#define CHECK(x) do { if (!(x)) { fprintf(stderr,"FAIL line %d: %s\n",__LINE__,#x); goto done; } } while (0)
int main(void) {
    glob_t paths={0};
    int a=-1,b=-1,shared[3]={-1,-1,-1},created[3]={0}, imported[3]={0};
    uint32_t h[3]={0}, ph[3]={0};
    int status=1,r,e,gr,thread_live=0; pthread_t thread;
    struct producer prod={0}; const char *path=NULL;
    gr=glob("/dev/dri/renderD*",0,NULL,&paths);
    if (gr==GLOB_NOMATCH) { fprintf(stderr,"UNSUPPORTED: no render node\n"); globfree(&paths); return 77; }
    if (gr) { fprintf(stderr,"FAIL: render node enumeration\n"); globfree(&paths); return 1; }
    for (size_t i=0;i<paths.gl_pathc;i++) {
        a=open(paths.gl_pathv[i],O_RDWR|O_CLOEXEC);
        if (a<0) continue;
        drmVersionPtr v=drmGetVersion(a);
        int match=v && ((v->name_len==6 && !memcmp(v->name,"nvidia",6)) ||
                        (v->name_len==10 && !memcmp(v->name,"nvidia-drm",10)));
        if (v) drmFreeVersion(v);
        if (match) { path=paths.gl_pathv[i]; break; }
        if (close(a)) { perror("close candidate"); a=-1; goto done; }
        a=-1;
    }
    if (!path) { fprintf(stderr,"UNSUPPORTED: no accessible NVIDIA render node\n"); status=77; goto done; }
    printf("NVIDIA render node: %s\n",path);
    b=open(path,O_RDWR|O_CLOEXEC); CHECK(b>=0);
    for (int i=0;i<3;i++) {
        errno=0; r=drmSyncobjCreate(a,0,&h[i]); e=error_of(r);
        if (r && unsupported(e)) { fprintf(stderr,"UNSUPPORTED: create: %s\n",strerror(e)); status=77; goto done; }
        CHECK(!r); created[i]=1;
        CHECK(!drmSyncobjHandleToFD(a,h[i],&shared[i]));
        CHECK(!drmSyncobjFDToHandle(b,shared[i],&ph[i])); imported[i]=1;
    }
    uint64_t current=UINT64_MAX, low=1, high=2;
    errno=0; r=drmSyncobjQuery(a,&h[0],&current,1); e=error_of(r);
    observation("initial query",r,e);
    if (r && unsupported(e)) { status=77; goto done; }
    CHECK(!r); printf("initial timeline value=%" PRIu64 "\n",current); CHECK(current==0);
    int64_t until=deadline(100); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[0],&low,1,until,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL); e=error_of(r);
    observation("unpublished WAIT_FOR_SUBMIT finite wait",r,e);
    if (r && unsupported(e)) { fprintf(stderr,"UNSUPPORTED: timeline wait\n"); status=77; goto done; }
    CHECK(r && (e==ETIME || e==ETIMEDOUT));
    until=deadline(100); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[0],&low,1,until,0,NULL); e=error_of(r);
    observation("unpublished no flags finite wait",r,e);
    /* Do not assume a particular errno for the no-flags case. */
    CHECK(r!=0);
    /* A CPU signal at the expected point has no GPU/frame provenance. */
    errno=0; r=drmSyncobjTimelineSignal(b,&ph[0],&low,1); e=error_of(r);
    observation("CPU signals expected point without GPU work",r,e);
    if (r && unsupported(e)) { fprintf(stderr,"UNSUPPORTED: timeline signal\n"); status=77; goto done; }
    CHECK(!r);
    until=deadline(1000); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[0],&low,1,until,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE|DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL); e=error_of(r);
    observation("expected-point WAIT_AVAILABLE after CPU signal",r,e);
    if (r && !unsupported(e)) { fprintf(stderr,"FAIL: expected-point WAIT_AVAILABLE observation\n"); goto done; }
    uint64_t exact=UINT64_MAX;
    errno=0; r=drmSyncobjQuery2(a,&h[0],&exact,1,DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED); e=error_of(r);
    observation("expected-point query LAST_SUBMITTED after CPU signal",r,e);
    if (!r) printf("last submitted=%" PRIu64 " (CPU signal, no GPU/frame)\n",exact);
    else if (!unsupported(e)) { fprintf(stderr,"FAIL: expected-point LAST_SUBMITTED observation\n"); goto done; }
    /* First independent object: low point prepared but never published; the
     * separate producer signals high after preparation and before the wait. */
    errno=0; r=drmSyncobjTimelineSignal(b,&ph[1],&high,1); e=error_of(r);
    observation("prepared low, producer signals high before wait",r,e);
    if (r && unsupported(e)) { fprintf(stderr,"UNSUPPORTED: timeline signal\n"); status=77; goto done; }
    CHECK(!r);
    until=deadline(1000); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[1],&low,1,until,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL); e=error_of(r);
    observation("lower wait after high signal",r,e);
    if (r) { fprintf(stderr,"DIFFERENT: lower wait did not succeed; no frame completion inferred\n"); goto done; }
    /* Neither WAIT_AVAILABLE nor LAST_SUBMITTED authenticates a fence's
     * producer or associates it with the expected frame. Observe their actual
     * behavior after only a higher point was signaled on this private object. */
    until=deadline(1000); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[1],&low,1,until,
        DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE|DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL); e=error_of(r);
    observation("lower WAIT_AVAILABLE after high signal",r,e);
    if (r && !unsupported(e)) { fprintf(stderr,"FAIL: WAIT_AVAILABLE observation\n"); goto done; }
    uint64_t submitted=UINT64_MAX;
    errno=0; r=drmSyncobjQuery2(a,&h[1],&submitted,1,DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED); e=error_of(r);
    observation("query LAST_SUBMITTED after high signal",r,e);
    if (!r) printf("last submitted timeline value=%" PRIu64 "\n",submitted);
    else if (!unsupported(e)) { fprintf(stderr,"FAIL: LAST_SUBMITTED observation\n"); goto done; }
    /* Second independent object: launch a delayed producer then issue the
     * bounded wait. Scheduling cannot prove precisely when the kernel entered
     * the wait; join ensures only completed operations are reported. */
    prod.fd=b; prod.handle=ph[2];
    r=pthread_create(&thread,NULL,signal_later,&prod); CHECK(!r); thread_live=1;
    until=deadline(1000); CHECK(until>=0);
    errno=0; r=drmSyncobjTimelineWait(a,&h[2],&low,1,until,DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL); e=error_of(r);
    int wait_r=r, wait_e=e;
    r=pthread_join(thread,NULL); if (!r) thread_live=0; CHECK(!r);
    observation("delayed producer high signal",prod.r,prod.e);
    observation("lower wait with delayed producer",wait_r,wait_e);
    CHECK(!prod.r);
    if (wait_r) { fprintf(stderr,"DIFFERENT: lower wait did not succeed; no frame completion inferred\n"); goto done; }
    puts("PASS: synthetic kernel syncobj observations only; no real frame completion inferred"); status=0;
done:
    if (thread_live) { r=pthread_join(thread,NULL); if (r) { fprintf(stderr,"FAIL: join: %s\n",strerror(r)); status=1; } }
    for (int i=0;i<3;i++) {
        if (imported[i] && drmSyncobjDestroy(b,ph[i])) { fprintf(stderr,"FAIL: destroy imported %d\n",i); status=1; }
        if (shared[i]>=0 && close(shared[i])) { perror("close syncobj fd"); status=1; }
        if (created[i] && drmSyncobjDestroy(a,h[i])) { fprintf(stderr,"FAIL: destroy syncobj %d\n",i); status=1; }
    }
    if (b>=0 && close(b)) { perror("close producer DRM fd"); status=1; }
    if (a>=0 && close(a)) { perror("close consumer DRM fd"); status=1; }
    globfree(&paths); return status;
}
