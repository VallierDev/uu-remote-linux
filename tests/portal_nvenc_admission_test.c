#include "../c_engine/portal_nvenc_admission.h"
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>

static struct pna_frame frame(void) { return (struct pna_frame){.buffer_id=1,.generation=1,.acquire=1,.release=2,.width=10,.height=10,.fourcc=0x34325258,.planes=1,.stride=40,.chunk_size=400,.allocation_size=400,.fd=3,.dmabuf=1,.metadata_valid=1,.unscheduled_release=1,.full_frame=1}; }
static struct pna_evidence evidence(void) { return (struct pna_evidence){.explicit_opt_in=1,.negotiated=1,.mutter_same_gpu=1,.trusted_producer=1,.exclusive_timeline=1,.pinned_lease_and_metadata=1,.verified_fd_and_size=1,.stable_unique_buffer_timeline=1,.width=10,.height=10,.fourcc=0x34325258}; }
struct args { struct pna_session *s; struct pna_frame f; struct pna_evidence e; int result; };
static void *race(void *p) { struct args *a=p; a->result=pna_admit(a->s,&a->f,&a->e); return NULL; }
static void release(struct pna_session *s, struct pna_frame f) {
    assert(!pna_acquired(s,f.buffer_id,f.generation,1));
    assert(!pna_no_gpu(s,f.buffer_id,f.generation,1));
    assert(!pna_release(s,f.buffer_id,f.generation,1));
}
/* Synthetic assertions only: these tests establish no real producer or GPU trust. */
static void tiled_tests(void) {
    struct pna_session s; struct pna_frame f=frame(), b; struct pna_evidence e=evidence(), x;
    uint64_t rng=UINT64_C(0x123456789abcdef);
    f.modifier=e.modifier=UINT64_C(0x0300000000606010);
    assert(!pna_init(&s));
    assert(pna_admit(&s,&f,&e)==-EINVAL);
    e.verified_tiled_layout_fd_size_and_device=2;
    assert(pna_admit(&s,&f,&e)==-EINVAL);
    e.verified_tiled_layout_fd_size_and_device=1;
    b=f; b.modifier^=1; assert(pna_admit(&s,&b,&e)==-EINVAL);
    x=e; x.modifier^=1; assert(pna_admit(&s,&f,&x)==-EINVAL);
    b=f; x=e; b.modifier=x.modifier=UINT64_C(0x00ffffffffffffff);
    assert(pna_admit(&s,&b,&x)==-EINVAL);
    b=f; b.full_frame=0; assert(pna_admit(&s,&b,&e)==-EINVAL);
    b=f; b.planes=2; assert(pna_admit(&s,&b,&e)==-EINVAL);
    b=f; b.stride=0; assert(pna_admit(&s,&b,&e)==-EINVAL);
    b=f; b.chunk_size=0; assert(pna_admit(&s,&b,&e)==-EINVAL);
    /* A tiny chunk is not a layout proof: success depends solely on the
     * independent attestation, not on any inferred linear/tile size. */
    b=f; b.stride=1; b.chunk_size=1; x=e;
    x.verified_tiled_layout_fd_size_and_device=0;
    assert(pna_admit(&s,&b,&x)==-EINVAL);
    assert(!pna_admit(&s,&b,&e)); release(&s,b);
    /* Exercise range arithmetic at both ends, with fresh identities. */
    for (uint64_t i=0; i<20000; i++) {
        rng^=rng<<13; rng^=rng>>7; rng^=rng<<17;
        b=f; b.buffer_id=2; b.generation=i+1; b.acquire=2*i+3; b.release=b.acquire+1;
        b.allocation_size=(i%3==0)?UINT64_MAX:(rng&1023);
        b.offset=(i%4==0)?UINT64_MAX:((rng>>10)&1023);
        b.chunk_size=(i%5==0)?UINT64_MAX:((rng>>20)&1023);
        int ok=b.chunk_size && b.offset<=b.allocation_size &&
            b.chunk_size<=b.allocation_size-b.offset;
        int r=pna_admit(&s,&b,&e);
        assert(ok ? r==0 : r==-EINVAL);
        if (ok) release(&s,b);
    }
    assert(!pna_destroy(&s));
}
int main(void) {
    tiled_tests();
    struct pna_session s; struct pna_frame f=frame(), bad; struct pna_evidence e=evidence(), be;
    assert(!pna_init(&s));
#define BAD_F(field,value) do { bad=f; bad.field=(value); assert(pna_admit(&s,&bad,&e)<0); } while(0)
#define BAD_E(field) do { be=e; be.field=0; assert(pna_admit(&s,&f,&be)<0); } while(0)
    BAD_E(explicit_opt_in); BAD_E(negotiated); BAD_E(mutter_same_gpu); BAD_E(trusted_producer);
    BAD_E(exclusive_timeline); BAD_E(pinned_lease_and_metadata); BAD_E(verified_fd_and_size); BAD_E(stable_unique_buffer_timeline);
    BAD_F(buffer_id,0); BAD_F(generation,0); BAD_F(acquire,0); BAD_F(release,1); BAD_F(width,0); BAD_F(height,8193);
    BAD_F(fourcc,0); BAD_F(modifier,1); BAD_F(planes,2); BAD_F(fd,-1); BAD_F(dmabuf,0);
    BAD_F(metadata_valid,0); BAD_F(unscheduled_release,0); BAD_F(full_frame,0);
    BAD_F(stride,39); BAD_F(offset,401); BAD_F(chunk_size,399); BAD_F(allocation_size,399);
    BAD_F(chunk_size,401); BAD_F(width,11);
    bad=f; be=e; bad.modifier=be.modifier=UINT64_C(0x0300000000606010);
    assert(pna_admit(&s,&bad,&be)<0); /* Exact negotiation alone does not prove layout. */
    struct args a={.s=&s,.f=f,.e=e}, b=a; pthread_t t,u;
    b.f.buffer_id=2;
    assert(!pthread_create(&t,0,race,&a)); assert(!pthread_create(&u,0,race,&b));
    pthread_join(t,0); pthread_join(u,0); assert((a.result==0 && b.result==-EBUSY)||(b.result==0 && a.result==-EBUSY));
    struct pna_frame winner=a.result==0?a.f:b.f, other=a.result==0?b.f:a.f;
    assert(pna_acquired(&s,other.buffer_id,1,1)<0); assert(pna_release(&s,winner.buffer_id,1,1)<0);
    assert(pna_destroy(&s)==-EBUSY); release(&s,winner);
    /* Per-buffer point and generation histories are independent, including equal points. */
    assert(!pna_admit(&s,&other,&e));
    assert(pna_acquired(&s,winner.buffer_id,1,1)<0);
    assert(!pna_acquired(&s,other.buffer_id,1,1));
    assert(!pna_gpu_begin(&s,other.buffer_id,1,1)); assert(pna_release(&s,other.buffer_id,1,1)<0);
    assert(!pna_gpu_complete(&s,other.buffer_id,1,1)); assert(!pna_release(&s,other.buffer_id,1,1));
    assert(pna_admit(&s,&winner,&e)==-ESTALE);
    winner.generation=2; assert(pna_admit(&s,&winner,&e)==-ESTALE);
    winner.acquire=2; winner.release=3; assert(pna_admit(&s,&winner,&e)==-ESTALE);
    winner.acquire=3; winner.release=4; assert(!pna_admit(&s,&winner,&e)); release(&s,winner);
    other.generation=2; other.acquire=3; other.release=4; assert(!pna_admit(&s,&other,&e)); release(&s,other);
    /* Old frame with matching generation but wrong buffer must not mutate active state. */
    winner.generation=3; winner.acquire=5; winner.release=6; assert(!pna_admit(&s,&winner,&e));
    assert(pna_revoke(&s,other.buffer_id,3)<0); release(&s,winner);
    assert(!pna_destroy(&s));
    /* Concurrent duplicate and inter-buffer attempts have exactly one winner. */
    assert(!pna_init(&s)); a=(struct args){.s=&s,.f=f,.e=e}; b=a;
    assert(!pthread_create(&t,0,race,&a)); assert(!pthread_create(&u,0,race,&b));
    pthread_join(t,0); pthread_join(u,0); assert((a.result==0 && b.result==-EBUSY)||(b.result==0 && a.result==-EBUSY));
    assert(!pna_acquired(&s,1,1,1)); assert(!pna_gpu_begin(&s,1,1,1));
    assert(!pna_revoke(&s,1,1)); assert(pna_abandon(&s,1,1,1)==-EBUSY);
    assert(pna_gpu_complete(&s,1,1,0)==-EPERM); assert(!pna_gpu_complete(&s,1,1,1));
    assert(pna_release(&s,1,1,1)<0); assert(pna_abandon(&s,1,1,0)==-EBUSY); assert(!pna_abandon(&s,1,1,1));
    assert(pna_admit(&s,&f,&e)==-EBUSY); assert(!pna_destroy(&s));
    for (int stage=0; stage<4; stage++) {
        assert(!pna_init(&s)); assert(!pna_admit(&s,&f,&e));
        if (stage==0) assert(pna_acquired(&s,1,1,0)==-EPERM);
        else { assert(!pna_acquired(&s,1,1,1));
            if (stage==1) assert(pna_gpu_begin(&s,1,1,0)==-EPERM);
            if (stage==2) assert(pna_no_gpu(&s,1,1,0)==-EPERM);
            if (stage==3) { assert(!pna_no_gpu(&s,1,1,1)); assert(pna_release(&s,1,1,0)==-EPERM); }
        }
        assert(pna_release(&s,1,1,1)<0); assert(!pna_abandon(&s,1,1,1)); assert(!pna_destroy(&s));
    }
    /* Histories are never evicted, and overflow cannot bypass them. */
    assert(!pna_init(&s));
    for (uint64_t i=1; i<=PNA_MAX_BUFFERS; i++) { f.buffer_id=i; assert(!pna_admit(&s,&f,&e)); release(&s,f); }
    f.buffer_id=PNA_MAX_BUFFERS+1; assert(pna_admit(&s,&f,&e)==-ENOSPC);
    f.buffer_id=1; assert(pna_admit(&s,&f,&e)==-ESTALE); assert(!pna_destroy(&s));
    assert(!pna_init(&s)); f=frame(); f.generation=UINT64_MAX; f.acquire=UINT64_MAX-1; f.release=UINT64_MAX;
    assert(!pna_admit(&s,&f,&e)); release(&s,f);
    f.generation=1; f.acquire=1; f.release=2; assert(pna_admit(&s,&f,&e)==-ESTALE); assert(!pna_destroy(&s));
    puts("admission tests passed"); return 0;
}
