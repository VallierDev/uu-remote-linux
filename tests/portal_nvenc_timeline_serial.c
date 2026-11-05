/* cc -std=gnu11 -Wall -Wextra -Werror -fsanitize=address,undefined -pthread
 * tests/portal_nvenc_timeline_serial.c c_engine/portal_nvenc_timeline.c
 * $(pkg-config --cflags --libs libdrm libspa-0.2) -o /tmp/pnt-serial && /tmp/pnt-serial */
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>

/* Model timeline semantics: signal(8) makes point 6 signaled too. */
static uint64_t timeline;
static int imports, fail_destroy;
static int imp(void *o,int d,int f,uint32_t *h) { (void)o;(void)d; imports++; *h=(uint32_t)f; return 0; }
static int wait_f(void *o,int d,uint32_t h,uint64_t p,int64_t t) { (void)o;(void)d;(void)h;(void)t; if (timeline<p) timeline=p; return 0; }
static int query_f(void *o,int d,uint32_t h,uint64_t *p) { (void)o;(void)d;(void)h; *p=timeline; return 0; }
static int signal_f(void *o,int d,uint32_t h,uint64_t p) { (void)o;(void)d;(void)h; if (timeline<p) timeline=p; return 0; }
static int dest(void *o,int d,uint32_t h) { (void)o;(void)d;(void)h; if (fail_destroy) { fail_destroy=0; return -EIO; } return 0; }
static const struct pnt_ops ops={imp,wait_f,query_f,signal_f,dest};
/* Both SPA data slots use the same syncobj fd, as in Mutter 51. */
static int create(struct pnt_ctx **c,uint64_t acquire,uint64_t release) { return pnt_create(c,10,11,acquire,11,release,&ops,NULL); }
#define N 16
static pthread_barrier_t start, attempted;
static atomic_int wins, busy;
static void *worker(void *unused) {
    struct pnt_ctx *c=NULL;
    int r;
    (void)unused;
    pthread_barrier_wait(&start);
    r=create(&c,5,6);
    if (!r) atomic_fetch_add(&wins,1);
    else { assert(r==-EBUSY && !c); atomic_fetch_add(&busy,1); }
    /* Winner keeps the guard until all other attempts finish. */
    pthread_barrier_wait(&attempted);
    if (!r) { assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c)); }
    return NULL;
}
int main(void) {
    struct pnt_ctx *a=NULL,*b=NULL;
    struct spa_meta_sync_timeline ma={.flags=1,.acquire_point=5,.release_point=6};
    struct spa_meta_sync_timeline mb={.flags=1,.acquire_point=7,.release_point=8};
    pthread_t threads[N]; int i, before;
    assert(!create(&a,5,6)); assert(!pnt_wait(a,1)); assert(!pnt_gpu_begin(a));
    before=imports; assert(create(&b,7,8)==-EBUSY && !b && imports==before);
    assert(timeline==5); /* signal(8) here would incorrectly release in-flight 6 */
    assert(pnt_destroy(&a)==-EBUSY); assert(create(&b,7,8)==-EBUSY);
    assert(!pnt_gpu_complete(a)); assert(!pnt_release(a,&ma)); assert(timeline==6);
    assert(create(&b,7,8)==-EBUSY); /* DONE still holds guard until destroy */
    assert(!pnt_destroy(&a)); assert(!create(&b,7,8)); assert(!pnt_wait(b,1));
    assert(timeline==7); assert(!pnt_no_gpu(b)); assert(!pnt_release(b,&mb)); assert(timeline==8);
    fail_destroy=1; assert(pnt_destroy(&b)==-EIO); assert(create(&a,9,10)==-EBUSY);
    assert(!pnt_destroy(&b));
    /* Reused and reversed points are already signaled; reject before GPU. */
    assert(!create(&a,5,6)); assert(pnt_wait(a,1)==-ESTALE); assert(pnt_gpu_begin(a)==-EINVAL); assert(!pnt_abandon_session(a)); assert(!pnt_destroy(&a));
    assert(!create(&a,7,8)); assert(pnt_wait(a,1)==-ESTALE); assert(pnt_gpu_begin(a)==-EINVAL); assert(!pnt_abandon_session(a)); assert(!pnt_destroy(&a));
    assert(!create(&a,9,10)); assert(!pnt_abandon_session(a));
    assert(create(&b,11,12)==-EBUSY); assert(!pnt_destroy(&a));
    /* Mutter-style single timeline with flags=0: currently rejected, not a
     * claim that SPA forbids zero. Keep lease/guard; never synthesize the bit. */
    timeline=0; ma.flags=0;
    assert(!create(&a,5,6)); assert(!pnt_wait(a,1)); assert(!pnt_gpu_begin(a));
    assert(!pnt_gpu_complete(a)); assert(pnt_release(a,&ma)==-EINVAL);
    assert(ma.flags==0 && timeline==5 && pnt_state(a)==PNT_READY);
    assert(pnt_release(a,&ma)==-EINVAL && timeline==5);
    assert(pnt_destroy(&a)==-EBUSY && create(&b,7,8)==-EBUSY);
    /* Simulate only after independent proof of complete session termination. */
    assert(!pnt_abandon_session(a)); assert(!pnt_destroy(&a));
    assert(!pthread_barrier_init(&start,NULL,N)); assert(!pthread_barrier_init(&attempted,NULL,N));
    for (i=0;i<N;i++) assert(!pthread_create(&threads[i],NULL,worker,NULL));
    for (i=0;i<N;i++) assert(!pthread_join(threads[i],NULL));
    assert(atomic_load(&wins)==1 && atomic_load(&busy)==N-1);
    assert(!pthread_barrier_destroy(&start)); assert(!pthread_barrier_destroy(&attempted));
    return 0;
}
