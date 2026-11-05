/* Standalone: cc -std=gnu11 -Wall -Wextra -Werror -fsanitize=address,undefined
 * tests/portal_nvenc_timeline.c c_engine/portal_nvenc_timeline.c
 * $(pkg-config --cflags --libs libdrm libspa-0.2) -o /tmp/pnt-test && /tmp/pnt-test */
#include "../c_engine/portal_nvenc_timeline.h"
#include <spa/buffer/meta.h>
#include <assert.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>

struct fake { int flagless, imports, fail_import, wait_error, query_error, signal_error, signal_applies, destroy_error, destroy_fail_at, signals, destroys, queries; uint64_t current; struct spa_meta_sync_timeline meta; };
static int imp(void *v, int d, int f, uint32_t *h) { struct fake *x=v; assert(d==10 && (f==11 || f==12)); x->imports++; if (x->imports==x->fail_import) return -EIO; *h=(uint32_t)f; return 0; }
static int wait_f(void *v, int d, uint32_t h, uint64_t p, int64_t t) { struct fake *x=v; assert(d==10 && h==11 && p==5 && t==123); return x->wait_error; }
static int query_f(void *v, int d, uint32_t h, uint64_t *p) { struct fake *x=v; assert(d==10 && h==12); x->queries++; *p=x->current; return x->query_error; }
static int signal_f(void *v, int d, uint32_t h, uint64_t p) { struct fake *x=v; assert(d==10 && h==12 && p==6 && x->meta.flags==(x->flagless ? 0u : 2u)); x->signals++; if (x->signal_applies) x->current=p; return x->signal_error; }
static int destroy_f(void *v, int d, uint32_t h) { struct fake *x=v; assert(d==10 && (h==11 || h==12)); x->destroys++; if (x->destroy_error || x->destroys==x->destroy_fail_at) { x->destroy_error=0; return -EIO; } return 0; }
static const struct pnt_ops ops={imp,wait_f,query_f,signal_f,destroy_f};
static int create(struct pnt_ctx **c, struct fake *f) { f->meta=(struct spa_meta_sync_timeline){.flags=3,.acquire_point=5,.release_point=6}; return pnt_create(c,10,11,5,12,6,&ops,f); }
int main(void) {
    struct pnt_ctx *c=NULL; struct fake f={0}; struct spa_meta_sync_timeline other;
    assert(pnt_create(&c,10,11,0,12,6,&ops,&f)==-EINVAL && !c);
    assert(pnt_create(&c,10,11,6,12,6,&ops,&f)==-EINVAL && !c);
    assert(pnt_create(&c,10,11,7,12,6,&ops,&f)==-EINVAL && !c);
    f.fail_import=1; assert(create(&c,&f)==-EIO); assert(pnt_state(c)==PNT_FAILED); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c) && !c);
    f=(struct fake){.fail_import=2}; assert(create(&c,&f)==-EIO); assert(pnt_destroy(&c)==-EBUSY); assert(!pnt_abandon_session(c)); f.destroy_error=1; assert(pnt_destroy(&c)==-EIO && c); assert(pnt_release(c,&f.meta)==-EINVAL); assert(!pnt_destroy(&c) && f.destroys==2);
    f=(struct fake){.wait_error=-ETIMEDOUT}; assert(!create(&c,&f)); assert(pnt_wait(c,123)==-ETIMEDOUT); assert(pnt_gpu_begin(c)==-EINVAL); assert(pnt_release(c,&f.meta)==-EINVAL); assert(pnt_destroy(&c)==-EBUSY); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(pnt_wait(c,-1)==-EINVAL); assert(!pnt_wait(c,123)); assert(!pnt_gpu_begin(c)); assert(pnt_release(c,&f.meta)==-EINVAL); assert(pnt_abandon_session(c)==-EBUSY); assert(pnt_destroy(&c)==-EBUSY); assert(!pnt_cancel(c)); assert(pnt_abandon_session(c)==-EBUSY); assert(!pnt_gpu_complete(c)); assert(pnt_state(c)==PNT_FAILED); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_no_gpu(c));
    assert(pnt_release(c,NULL)==-EINVAL && f.meta.flags==3); f.meta.release_point=7; assert(pnt_release(c,&f.meta)==-EINVAL && f.meta.flags==3); f.meta.release_point=6; f.meta.acquire_point=7; assert(pnt_release(c,&f.meta)==-EINVAL && f.meta.flags==3); f.meta.acquire_point=5; f.meta.flags=2; assert(pnt_release(c,&f.meta)==-EINVAL && !f.signals); f.meta.flags=3;
    assert(!pnt_release(c,&f.meta)); assert(f.meta.flags==2 && f.signals==1); assert(pnt_abandon_session(c)==-EINVAL); assert(pnt_release(c,&f.meta)==-EINVAL); f.destroy_error=1; assert(pnt_destroy(&c)==-EIO); assert(!pnt_destroy(&c));
    /* Fake side effects model an ambiguous error; they prove nothing about libdrm. */
    f=(struct fake){.signal_error=-EIO,.signal_applies=1}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_gpu_begin(c)); assert(!pnt_gpu_complete(c)); assert(pnt_release(c,&f.meta)==-EIO); assert(f.meta.flags==2 && f.current==6 && f.signals==1 && pnt_state(c)==PNT_FAILED); assert(pnt_cancel(c)==-EINVAL && pnt_destroy(&c)==-EBUSY); other=f.meta; assert(pnt_release(c,&other)==-EINVAL); f.signal_error=0; assert(pnt_release(c,&f.meta)==-EINVAL && f.signals==1 && f.current==6); { struct pnt_ctx *blocked=NULL; struct fake g={0}; assert(create(&blocked,&g)==-EBUSY && !blocked && !g.imports); } assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){.signal_error=-EIO}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_no_gpu(c)); assert(pnt_release(c,&f.meta)==-EIO); assert(pnt_release(c,&f.meta)==-EINVAL && f.signals==1 && f.meta.flags==2); assert(pnt_destroy(&c)==-EBUSY); assert(!pnt_abandon_session(c)); assert(pnt_state(c)==PNT_ABANDONED && f.signals==1); assert(pnt_release(c,&f.meta)==-EINVAL && pnt_cancel(c)==-EINVAL); f.destroy_error=1; assert(pnt_destroy(&c)==-EIO); assert(pnt_state(c)==PNT_ABANDONED); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_abandon_session(c)); assert(pnt_wait(c,123)==-EINVAL && !f.signals); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_abandon_session(c)); assert(pnt_gpu_begin(c)==-EINVAL); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_no_gpu(c)); assert(!pnt_abandon_session(c)); assert(f.meta.flags==3 && !f.signals); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_cancel(c)); assert(pnt_wait(c,123)==-EINVAL); assert(pnt_destroy(&c)==-EBUSY); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){.query_error=-EIO}; assert(!create(&c,&f)); assert(pnt_wait(c,123)==-EIO); assert(pnt_state(c)==PNT_FAILED); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); f.query_error=-EIO; assert(pnt_gpu_begin(c)==-EIO); assert(pnt_state(c)==PNT_FAILED && !f.signals); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); f.query_error=-EIO; assert(pnt_no_gpu(c)==-EIO); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_no_gpu(c)); f.query_error=-EIO; assert(pnt_release(c,&f.meta)==-EIO && f.meta.flags==3 && !f.signals); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); f.current=6; assert(pnt_gpu_begin(c)==-ESTALE && !f.signals); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123)); assert(!pnt_no_gpu(c)); f.current=8; assert(pnt_release(c,&f.meta)==-ESTALE && f.meta.flags==3 && !f.signals); assert(!pnt_abandon_session(c)); assert(!pnt_destroy(&c));
    for (int error=0;error<2;error++) {
        f=(struct fake){.flagless=1,.signal_error=error ? -EIO : 0};
        f.meta=(struct spa_meta_sync_timeline){.flags=0,.acquire_point=5,.release_point=6};
        assert(pnt_create_protocol(&c,10,11,5,12,6,&ops,&f,2)==-EINVAL && !c);
        assert(!pnt_create_protocol(&c,10,11,5,12,6,&ops,&f,1));
        assert(!pnt_wait(c,123) && !pnt_no_gpu(c));
        f.meta.flags=1; assert(pnt_release(c,&f.meta)==-EINVAL && !f.signals);
        f.meta.flags=0; assert(pnt_release(c,&f.meta)==(error ? -EIO : 0));
        assert(f.signals==1 && f.meta.flags==0);
        assert(pnt_release(c,&f.meta)==-EINVAL && f.signals==1);
        if(error) assert(!pnt_abandon_session(c));
        assert(!pnt_destroy(&c));
    }
    /* Retirement never invents completion or retries a release signal. */
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123));
    assert(!pnt_gpu_begin(c)); assert(pnt_retire_proven_session(&c)==-EBUSY && c);
    assert(!f.destroys && !f.signals); assert(!pnt_gpu_complete(c));
    f.destroy_error=1; assert(pnt_retire_proven_session(&c)==-EIO && c);
    assert(pnt_state(c)==PNT_ABANDONED && !f.signals);
    assert(!pnt_retire_proven_session(&c) && !c && !f.signals);
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123));
    assert(!pnt_no_gpu(c)); assert(!pnt_release(c,&f.meta)); assert(f.signals==1);
    f.destroy_error=1; assert(pnt_retire_proven_session(&c)==-EIO && c);
    assert(pnt_state(c)==PNT_DONE && f.signals==1);
    assert(!pnt_retire_proven_session(&c) && !c && f.signals==1);
    f=(struct fake){.destroy_fail_at=2}; assert(!create(&c,&f));
    assert(pnt_retire_proven_session(&c)==-EIO && c && f.destroys==2);
    assert(!pnt_retire_proven_session(&c) && !c && f.destroys==3 && !f.signals);
    assert(pnt_retire_proven_session(&c)==-EINVAL);

    /* Local cleanup may close only our imports, never signal or recycle a lease. */
    for (int stage=0; stage<6; stage++) {
        f=(struct fake){0};
        if (stage<2) f.fail_import=stage+1;
        int created=create(&c,&f);
        if (stage<2) assert(created==-EIO); else assert(!created);
        if (stage==3) { f.wait_error=-ETIMEDOUT; assert(pnt_wait(c,123)==-ETIMEDOUT); }
        if (stage>=4) assert(!pnt_wait(c,123));
        if (stage==5) assert(!pnt_no_gpu(c));
        assert(!pnt_discard_unsubmitted(&c) && !c && !f.signals && f.meta.flags==3);
    }
    f=(struct fake){0}; assert(!create(&c,&f)); assert(!pnt_wait(c,123));
    assert(!pnt_gpu_begin(c)); assert(pnt_discard_unsubmitted(&c)==-EBUSY);
    assert(!pnt_gpu_complete(c)); assert(pnt_discard_unsubmitted(&c)==-EBUSY);
    assert(!pnt_retire_proven_session(&c)); /* synthetic owner only */
    f=(struct fake){.signal_error=-EIO}; assert(!create(&c,&f)); assert(!pnt_wait(c,123));
    assert(!pnt_no_gpu(c)); assert(pnt_release(c,&f.meta)==-EIO);
    assert(pnt_discard_unsubmitted(&c)==-EBUSY && f.signals==1);
    assert(!pnt_retire_proven_session(&c));
    f=(struct fake){.destroy_fail_at=2}; assert(!create(&c,&f));
    assert(pnt_discard_unsubmitted(&c)==-EIO && c && pnt_state(c)==PNT_DISCARDED);
    assert(pnt_wait(c,123)==-EINVAL && pnt_cancel(c)==-EINVAL);
    struct pnt_ctx *busy=NULL; struct fake another={0}; assert(create(&busy,&another)==-EBUSY && !busy);
    assert(!pnt_discard_unsubmitted(&c) && !c && f.destroys==3 && !f.signals);
    assert(pnt_discard_unsubmitted(&c)==-EINVAL);
    for (int cancelled=0; cancelled<2; cancelled++) {
        f=(struct fake){.destroy_fail_at=2}; assert(!create(&c,&f)); assert(!pnt_wait(c,123));
        assert(!pnt_gpu_begin(c));
        if (cancelled) assert(!pnt_cancel(c));
        assert(pnt_discard_before_release(&c)==-EBUSY && !f.destroys);
        assert(!pnt_gpu_complete(c));
        assert(pnt_discard_before_release(&c)==-EIO && c);
        assert(!pnt_discard_before_release(&c) && !c && f.destroys==3);
        assert(!f.signals && f.meta.flags==3);
    }
    for (int failure=0; failure<2; failure++) {
        f=(struct fake){.signal_error=failure ? -EIO : 0};
        assert(!create(&c,&f)); assert(!pnt_wait(c,123));
        assert(!pnt_gpu_begin(c)); assert(!pnt_gpu_complete(c));
        assert(pnt_release(c,&f.meta)==(failure ? -EIO : 0));
        assert(pnt_discard_before_release(&c)==-EBUSY && f.signals==1 && !f.destroys);
        assert(!pnt_retire_proven_session(&c)); /* synthetic-only old path */
    }
    assert(pnt_discard_before_release(&c)==-EINVAL);
    for (int protocol=0; protocol<2; protocol++) for (int failure=0; failure<3; failure++) {
        f=(struct fake){.signal_error=failure ? -EIO : 0,.signal_applies=failure==2};
        assert(!pnt_create_protocol(&c,10,11,5,12,6,&ops,&f,protocol));
        f.flagless=protocol;
        f.meta=(struct spa_meta_sync_timeline){.acquire_point=5,.release_point=6,.flags=protocol ? 0u : 3u};
        assert(!pnt_wait(c,123)); assert(!pnt_gpu_begin(c));
        assert(pnt_discard_completed(&c)==-EBUSY && !f.destroys);
        assert(!pnt_gpu_complete(c));
        assert(pnt_release(c,&f.meta)==(failure ? -EIO : 0));
        unsigned flags=f.meta.flags; uint64_t current=f.current;
        f.destroy_fail_at=2;
        assert(pnt_discard_completed(&c)==-EIO && c);
        assert(pnt_release(c,&f.meta)==-EINVAL);
        struct pnt_ctx *blocked=NULL; struct fake g={0};
        assert(create(&blocked,&g)==-EBUSY && !blocked);
        assert(!pnt_discard_completed(&c) && !c && f.destroys==3);
        assert(f.signals==1 && f.meta.flags==flags && f.current==current);
    }
    assert(pnt_discard_completed(&c)==-EINVAL);
    assert(pnt_abandon_session(NULL)==-EINVAL);
    return 0;
}
