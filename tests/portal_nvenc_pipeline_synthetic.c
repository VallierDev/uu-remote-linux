/* Single-threaded synthetic integration, no Portal, device, service or display.
 * cc -std=gnu11 -Wall -Wextra -Werror -g -Ivendor/ffnvcodec \
 *   $(pkg-config --cflags libspa-0.2 libdrm) \
 *   tests/portal_nvenc_pipeline_synthetic.c c_engine/portal_nvenc_spa.c \
 *   c_engine/portal_nvenc_admission.c c_engine/portal_nvenc_timeline.c \
 *   contrib/portal-nvenc-experimental.c $(pkg-config --libs libdrm) \
 *   -pthread -o /tmp/pipeline-synthetic && /tmp/pipeline-synthetic
 * Fake operations cannot prove producer authority, FD identity/lifetime, pending
 * foreign signals, negotiated format, GPU completion scope or external contracts.
 * All attestations and teardown/quiescence below are synthetic assumptions.
 */
#include "../c_engine/portal_nvenc_spa.h"
#include "../c_engine/portal_nvenc_admission.h"
#include "../c_engine/portal_nvenc_timeline.h"
#include "../contrib/portal-nvenc-experimental.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>

struct fake_timeline { uint64_t current; };
struct fake { int waits, queries, signals, imports, destroys, copies, syncs, maps, encodes, locks, queues;
    int missing, copy_fail, sync_fail, encode_fail, revoke_copy;
    struct fake_timeline timeline_object; char events[512]; size_t event_len;
    struct pna_session *session; struct pnt_ctx *timeline; PortalNvencExperimental *gpu;
};
static struct fake *F;
static void event(struct fake *f, const char *name) {
    int n=snprintf(f->events+f->event_len,sizeof(f->events)-f->event_len,"%s%s",f->event_len ? "," : "",name);
    assert(n>=0 && (size_t)n<sizeof(f->events)-f->event_len); f->event_len+=(size_t)n;
}
/* Both imported handles refer to the same synthetic timeline object. */
static struct fake_timeline *object(struct fake *f,uint32_t h) { assert(h==1 || h==2); return &f->timeline_object; }
static int imp(void *o,int fd,int obj,uint32_t *h) { struct fake *f=o; (void)fd; assert(obj==4); event(f,"imp"); *h=++f->imports; return 0; }
static int wait_fake(void *o,int fd,uint32_t h,uint64_t p,int64_t t) { struct fake *f=o; struct fake_timeline *tl=object(f,h); (void)fd;(void)t; event(f,"wait"); f->waits++; if (f->missing) return -ETIMEDOUT; if (tl->current<p) tl->current=p; return 0; }
static int query_fake(void *o,int fd,uint32_t h,uint64_t *p) { struct fake *f=o; (void)fd; event(f,"query"); f->queries++; *p=object(f,h)->current; return 0; }
static int signal_fake(void *o,int fd,uint32_t h,uint64_t p) { struct fake *f=o; struct fake_timeline *tl=object(f,h); (void)fd; event(f,"signal"); f->signals++; assert(p>=tl->current); tl->current=p; return 0; }
static int destroy_fake(void *o,int fd,uint32_t h) { struct fake *f=o;(void)fd; (void)object(f,h); event(f,"destroy"); f->destroys++; return 0; }
static const struct pnt_ops ops={imp,wait_fake,query_fake,signal_fake,destroy_fake};
static int copy_fake(void *o,CUdeviceptr d,uint32_t pitch) { struct fake *f=o; (void)d;(void)pitch; event(f,"copy"); f->copies++;
    if (f->revoke_copy) { assert(!pna_revoke(f->session,1,1)); assert(!pnt_cancel(f->timeline)); portal_nvenc_experimental_quarantine(f->gpu); }
    return f->copy_fail ? -1 : 0;
}
static CUresult CUDAAPI sync_fake(void) { event(F,"sync"); F->syncs++; return F->sync_fail ? (CUresult)1 : CUDA_SUCCESS; }
static NVENCSTATUS NVENCAPI map_fake(void *e,NV_ENC_MAP_INPUT_RESOURCE *m) { (void)e; event(F,"map"); F->maps++; m->mappedResource=(void *)1; m->mappedBufferFmt=NV_ENC_BUFFER_FORMAT_ARGB; return NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI encode_fake(void *e,NV_ENC_PIC_PARAMS *p) { (void)e;(void)p; event(F,"encode"); F->encodes++; return F->encode_fail ? NV_ENC_ERR_GENERIC : NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI lock_fake(void *e,NV_ENC_LOCK_BITSTREAM *p) { static unsigned char bytes[]={1,2}; (void)e; event(F,"lock"); F->locks++; p->bitstreamBufferPtr=bytes; p->bitstreamSizeInBytes=sizeof(bytes); return NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI unlock_fake(void *e,NV_ENC_OUTPUT_PTR p) { (void)e;(void)p; event(F,"unlock"); return NV_ENC_SUCCESS; }
static NVENCSTATUS NVENCAPI unmap_fake(void *e,NV_ENC_INPUT_PTR p) { (void)e;(void)p; event(F,"unmap"); return NV_ENC_SUCCESS; }
/* This callback models only this file's handwritten orchestration, not a production queue. */
static void queue_fake(void *o) { struct fake *f=o; event(f,"queue"); f->queues++; }

enum scenario { SUCCESS, MISSING, HIGH, COPY_FAIL, SYNC_FAIL, ENCODE_FAIL, REVOKE, FORMAT, FORMAT_CHANGE };
static void run(enum scenario which) {
    struct fake f={0}; F=&f; f.missing=which==MISSING; f.timeline_object.current=which==HIGH ? 10 : 0; f.copy_fail=which==COPY_FAIL; f.sync_fail=which==SYNC_FAIL; f.encode_fail=which==ENCODE_FAIL; f.revoke_copy=which==REVOKE;
    struct spa_chunk chunk={.size=32,.stride=16};
    struct spa_data d[3]={{.type=SPA_DATA_DmaBuf,.fd=3,.maxsize=32,.chunk=&chunk},{.type=SPA_DATA_SyncObj,.fd=4},{.type=SPA_DATA_SyncObj,.fd=4}};
    struct spa_meta_sync_timeline meta={.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE,.acquire_point=9,.release_point=10};
    struct spa_meta m={.type=SPA_META_SyncTimeline,.size=sizeof(meta),.data=&meta};
    struct spa_buffer b={.n_datas=3,.datas=d,.n_metas=1,.metas=&m};
    struct spa_video_info_raw fmt={.format=SPA_VIDEO_FORMAT_BGRA,.flags=SPA_VIDEO_FLAG_MODIFIER,.size={4,2}};
    struct pnspa_snapshot snap; assert(pnspa_snapshot(&b,&fmt,0,&snap));
    struct pna_evidence ev={.explicit_opt_in=1,.negotiated=1,.mutter_same_gpu=1,.trusted_producer=1,.exclusive_timeline=1,.pinned_lease_and_metadata=1,.verified_fd_and_size=1,.stable_unique_buffer_timeline=1,.width=4,.height=2,.fourcc=0x34325241};
    struct pna_frame frame={.buffer_id=1,.generation=1,.acquire=snap.acquire_point,.release=snap.release_point,.modifier=snap.modifier,.width=snap.width,.height=snap.height,.fourcc=ev.fourcc,.planes=1,.stride=(uint32_t)snap.stride,.offset=snap.offset,.chunk_size=snap.size,.allocation_size=snap.maxsize,.fd=snap.pixel_fd,.dmabuf=1,.metadata_valid=1,.unscheduled_release=!!(snap.sync_flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE),.full_frame=1};
    struct pna_session s; assert(!pna_init(&s));
    if (which==FORMAT) { ev.width=5; assert(pna_admit(&s,&frame,&ev)==-EINVAL); assert(s.state==PNA_IDLE); assert(!pna_destroy(&s)); return; }
    assert(!pna_admit(&s,&frame,&ev));
    assert(pna_acquired(&s,2,1,1)==-EINVAL); assert(pna_acquired(&s,1,2,1)==-EINVAL); assert(s.state==PNA_LEASED);
    struct pnt_ctx *t=NULL; assert(!pnt_create(&t,7,snap.acquire_fd,snap.acquire_point,snap.release_fd,snap.release_point,&ops,&f));
    f.session=&s; f.timeline=t;
    int r=pnt_wait(t,1);
    if (which==HIGH) assert(r==-ESTALE && f.timeline_object.current==10);
    if (which==MISSING) assert(r==-ETIMEDOUT && f.timeline_object.current==0);
    if (r) { assert(which==MISSING || which==HIGH); assert(pna_acquired(&s,1,1,0)==-EPERM); }
    if (r) goto failed;
    assert(f.timeline_object.current==9); assert(!pna_acquired(&s,1,1,1));
    if (which==FORMAT_CHANGE) {
        fmt.size.width=3; assert(pnspa_snapshot(&b,&fmt,0,&snap));
        assert(snap.width!=ev.width); /* caller observes changed format and revokes */
        assert(!pna_revoke(&s,1,1)); assert(!pnt_cancel(t));
        assert(pnt_gpu_begin(t)==-EINVAL && pna_gpu_begin(&s,1,1,1)==-EINVAL);
        assert(!f.copies && !f.syncs && !f.encodes);
        goto failed;
    }
    assert(!pnt_gpu_begin(t)); assert(!pna_gpu_begin(&s,1,1,1));
    NV_ENCODE_API_FUNCTION_LIST funcs={0}; funcs.nvEncMapInputResource=map_fake; funcs.nvEncEncodePicture=encode_fake; funcs.nvEncLockBitstream=lock_fake; funcs.nvEncUnlockBitstream=unlock_fake; funcs.nvEncUnmapInputResource=unmap_fake;
    unsigned char output[16]; PortalNvencExperimental gpu={.fn=&funcs,.encoder=(void *)1,.surface=(void *)1,.bitstream=(void *)1,.dst=1,.width=4,.height=2,.sync=sync_fake,.output=output,.capacity=sizeof(output)}; f.gpu=&gpu;
    const uint8_t *data=NULL; uint32_t len=0;
    r=portal_nvenc_experimental_encode(&gpu,copy_fake,&f,&data,&len);
    if (r) {
        assert(gpu.quarantined && !data && !len);
        assert(s.gpu_pending && pnt_state(t)==PNT_GPU);
        assert(pna_release(&s,1,1,1)==-EINVAL && pnt_release(t,&meta)==-EINVAL);
        assert(!f.signals && !f.queues && (meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
        if (which!=REVOKE) { assert(!pna_revoke(&s,1,1)); assert(!pnt_cancel(t)); }
        assert(pna_abandon(&s,1,1,1)==-EBUSY && pnt_abandon_session(t)==-EBUSY);
        /* Only the synthetic test now asserts all fake work has quiesced. */
        assert(!pna_gpu_complete(&s,1,1,1)); assert(!pnt_gpu_complete(t));
        assert(gpu.source_complete==(which==ENCODE_FAIL));
        assert(f.copies==1 && f.syncs==(which==SYNC_FAIL || which==ENCODE_FAIL) && f.encodes==(which==ENCODE_FAIL));
        goto failed;
    }
    assert(which==SUCCESS && gpu.source_complete && len==2 && data==output);
    assert(!pnt_gpu_complete(t)); assert(!pna_gpu_complete(&s,1,1,1));
    assert(!pnt_release(t,&meta)); assert(!pna_release(&s,1,1,1)); queue_fake(&f);
    assert(f.timeline_object.current==10);
    assert(!strcmp(f.events,"imp,imp,wait,query,query,copy,sync,map,encode,lock,unlock,unmap,query,signal,queue"));
    assert(f.signals==1 && f.queues==1 && !(meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
    assert(!pnt_destroy(&t));
    assert(pna_admit(&s,&frame,&ev)==-ESTALE); frame.generation=2; assert(pna_admit(&s,&frame,&ev)==-ESTALE);
    frame.buffer_id=2; frame.generation=1; assert(!pna_admit(&s,&frame,&ev)); assert(!pna_revoke(&s,2,1)); assert(!pna_abandon(&s,2,1,1)); assert(!pna_destroy(&s));
    /* Reinitialization is legal only after the prior mutex was destroyed. */
    assert(!pna_init(&s)); assert(s.state==PNA_IDLE); assert(!pna_admit(&s,&frame,&ev));
    assert(!pna_revoke(&s,2,1)); assert(!pna_abandon(&s,2,1,1)); assert(!pna_destroy(&s));
    assert(!strcmp(f.events,"imp,imp,wait,query,query,copy,sync,map,encode,lock,unlock,unmap,query,signal,queue,destroy,destroy")); return;
failed:
    assert(!f.signals && !f.queues && (meta.flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
    assert(s.state==PNA_REVOKED && !s.gpu_pending); assert(pnt_state(t)==PNT_FAILED);
    /* Synthetic irrevocable termination, not a real producer teardown. */
    assert(!pna_abandon(&s,1,1,1)); assert(!pnt_abandon_session(t)); assert(!pnt_destroy(&t)); assert(!pna_destroy(&s)); assert(f.destroys==2);
    const char *expected=NULL;
    switch (which) {
    case MISSING: expected="imp,imp,wait,destroy,destroy"; break;
    case HIGH: expected="imp,imp,wait,query,destroy,destroy"; break;
    case FORMAT_CHANGE: expected="imp,imp,wait,query,destroy,destroy"; break;
    case COPY_FAIL: case REVOKE: expected="imp,imp,wait,query,query,copy,destroy,destroy"; break;
    case SYNC_FAIL: expected="imp,imp,wait,query,query,copy,sync,destroy,destroy"; break;
    case ENCODE_FAIL: expected="imp,imp,wait,query,query,copy,sync,map,encode,destroy,destroy"; break;
    default: assert(0);
    }
    assert(!strcmp(f.events,expected));
}
int main(void) { for (int i=SUCCESS;i<=FORMAT_CHANGE;i++) run((enum scenario)i); puts("PASS synthetic orchestration only; mock queue does not prove production queue safety or external contracts"); return 0; }
