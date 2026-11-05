#define _GNU_SOURCE
/* Actual Portal negotiation/process callbacks, fake PipeWire operations and
 * memfd snapshots. No real Portal session, GPU or compositor contract proof. */
#include <pipewire/pipewire.h>
static int capture_update(struct pw_stream *, const struct spa_pod **, uint32_t);
static struct pw_buffer *capture_dequeue(struct pw_stream *);
#define pw_stream_update_params capture_update
#define pw_stream_dequeue_buffer capture_dequeue
#define main baseline_lease_main
#include "portal_async_lease.c"
#undef main
static struct pw_buffer *incoming[2];
static unsigned count, next, updates, offers;
static int update_error;
static const uint64_t target_modifier = UINT64_C(0x0300000000606010);
static struct pw_buffer *capture_dequeue(struct pw_stream *stream) {
    (void)stream; return next < count ? incoming[next++] : NULL;
}
static int capture_update(struct pw_stream *stream, const struct spa_pod **params, uint32_t n) {
    (void)stream; assert(n == 2 || n == 3); updates++;
    unsigned index = 0;
    if (n == 3) {
        offers++;
        struct spa_video_info_raw info = {0};
        assert(spa_format_video_raw_parse(params[0], &info) >= 0);
        assert(info.format == SPA_VIDEO_FORMAT_BGRA && info.modifier == target_modifier);
        const struct spa_pod_prop *p = spa_pod_find_prop(params[0], NULL, SPA_FORMAT_VIDEO_modifier);
        assert(p && (p->flags & SPA_POD_PROP_FLAG_MANDATORY)); index++;
    }
    const struct spa_pod_prop *p = spa_pod_find_prop(params[index], NULL, SPA_PARAM_BUFFERS_blocks);
    int32_t blocks; assert(p && !spa_pod_get_int(&p->value, &blocks) && blocks == 3);
    p = spa_pod_find_prop(params[index], NULL, SPA_PARAM_BUFFERS_metaType);
    int32_t types; assert(p && (p->flags & SPA_POD_PROP_FLAG_MANDATORY));
    assert(!spa_pod_get_int(&p->value, &types) && types == (1 << SPA_META_SyncTimeline));
    p = spa_pod_find_prop(params[index+1], NULL, SPA_PARAM_META_size);
    int32_t size; assert(p && !spa_pod_get_int(&p->value, &size) && size == sizeof(struct spa_meta_sync_timeline));
    return update_error;
}
static void start(struct fixture *f) {
    setup(f); incoming[0] = incoming[1] = NULL;
    count = next = updates = offers = 0; update_error = 0;
    f->p->dma_pending = NULL; f->p->dma_requested = f->p->dma_format = 0;
}
static void format(struct fixture *f, int modifier) {
    uint8_t storage[512]; struct spa_pod_builder b = SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    struct spa_video_info_raw info = { .format=SPA_VIDEO_FORMAT_BGRA, .size={2,2},
        .flags = modifier ? SPA_VIDEO_FLAG_MODIFIER : 0, .modifier=modifier ? target_modifier : 0 };
    stream_format(f->p, SPA_PARAM_Format, spa_format_video_raw_build(&b, SPA_PARAM_Format, &info));
}
static int release_flagless(void *unused, struct spa_meta_sync_timeline *m) {
    (void)unused; assert(m->flags == 0 && m->acquire_point == 1 && m->release_point == 2);
    releases++; return 0;
}
int main(void) {
    alarm(20);
    struct fixture f, second; start(&f); setup(&second);
    assert(!uu_portal_request_explicit(f.p,target_modifier)); assert(updates == 1 && offers == 1);
    UuPortalAsyncFrame snapshot; struct uu_portal_async_lease *lease = NULL;
    assert(uu_portal_async_acquire(f.p,&lease,&snapshot) == -EAGAIN);
    assert(uu_portal_request_explicit(f.p,target_modifier) == -EBUSY);
    format(&f,1); assert(updates == 2 && !f.p->error);
    incoming[0]=&f.buffer; incoming[1]=&second.buffer; count=2;
    stream_process(f.p); assert(next==1 && f.p->dma_pending==&f.buffer && !queues);
    stream_process(f.p); assert(next==1 && !queues); /* Never drop the pending frame. */
    assert(!uu_portal_async_acquire(f.p,&lease,&snapshot) && snapshot.pixels.modifier==target_modifier);
    stream_process(f.p); assert(next==2 && f.p->dma_pending==&second.buffer && !queues);
    assert(uu_portal_request_explicit(f.p,target_modifier)==-EBUSY); /* live lease */
    UuDmaFrame legacy; uint64_t revision=0;
    assert(uu_portal_dmabuf_acquire(f.p,&legacy,&revision)==14);
    assert(!uu_portal_async_finish(&lease,1,release_fake,NULL) && queues==1);
    uint64_t epoch=uu_portal_capture_epoch(f.p);
    assert(!uu_portal_request_explicit(f.p,target_modifier));
    assert(updates==2 && offers==1 && queues==1 && next==2);
    assert(f.p->dma_pending==&second.buffer && uu_portal_capture_epoch(f.p)==epoch);
    assert(uu_portal_request_explicit(f.p,target_modifier+1)==-EBUSY);
    assert(uu_portal_request_explicit_protocol(f.p,target_modifier,1)==-EBUSY);
    assert(!uu_portal_async_acquire(f.p,&lease,&snapshot));
    assert(!uu_portal_async_finish(&lease,1,release_fake,NULL) && queues==2);
    assert(!uu_portal_request_explicit(f.p,target_modifier));
    f.p->dma_blocked=1;
    assert(uu_portal_request_explicit(f.p,target_modifier)==-ECANCELED);
    f.p->dma_blocked=0; uu_portal_end(f.p);
    assert(uu_portal_request_explicit(f.p,target_modifier)==-ECANCELED);
    cleanup(&f); cleanup(&second);
    for (int action=0;action<6;action++) {
        start(&f); assert(!uu_portal_request_explicit(f.p,target_modifier));
        if (action==0) { format(&f,0); assert(f.p->error && !queues); }
        else {
            format(&f,1); incoming[0]=&f.buffer; count=1;
            if (action==1) f.timeline.flags=0;
            stream_process(f.p); assert(f.p->dma_pending==&f.buffer && !queues);
            if (action==1) {
                assert(f.p->error && uu_portal_async_acquire(f.p,&lease,&snapshot)==-EPROTONOSUPPORT);
                assert(!lease && !queues && !releases);
                /* The retained diagnostic survives polling; closure takes precedence. */
                assert(uu_portal_async_acquire(f.p,&lease,&snapshot)==-EPROTONOSUPPORT);
                uu_portal_end(f.p);
                assert(uu_portal_async_acquire(f.p,&lease,&snapshot)==-ECANCELED);
            }
            else if (action==2) stream_format(f.p,SPA_PARAM_Format,NULL);
            else if (action==3) uu_portal_dmabuf_disable(f.p);
            else if (action==4) uu_portal_reset_frame(f.p);
            else {
                assert(!uu_portal_async_acquire(f.p,&lease,&snapshot));
                stream_format(f.p,SPA_PARAM_Format,NULL);
                assert(uu_portal_async_cancelled(lease));
                assert(uu_portal_async_finish(&lease,1,release_fake,NULL)==-ECANCELED);
                uu_portal_end(f.p); assert(!uu_portal_async_abandon(&lease,1));
            }
            assert(f.p->error && !queues);
        }
        cleanup(&f);
    }
    start(&f); update_error=-EIO;
    assert(uu_portal_request_explicit(f.p,target_modifier)==-EIO && f.p->error && !queues);
    cleanup(&f);
    start(&f); assert(!uu_portal_request_explicit(f.p,target_modifier));
    f.p->dma_requested_at=g_get_monotonic_time()-3*G_TIME_SPAN_SECOND;
    assert(uu_portal_async_acquire(f.p,&lease,&snapshot)==-ETIMEDOUT); cleanup(&f);
    start(&f);
    assert(uu_portal_request_explicit_protocol(f.p,target_modifier,2)==-EINVAL && !updates);
    assert(!uu_portal_request_explicit_protocol(f.p,target_modifier,1));
    assert(uu_portal_request_explicit_protocol(f.p,target_modifier,0)==-EBUSY);
    format(&f,1); f.timeline.flags=0; incoming[0]=&f.buffer; count=1;
    assert(!uu_portal_async_acquire(f.p,&lease,&snapshot));
    f.timeline.flags=1; /* Changing the contract during a lease must not signal. */
    assert(uu_portal_async_finish(&lease,1,release_flagless,NULL)==-ECANCELED && !releases && !queues);
    uu_portal_end(f.p); assert(!uu_portal_async_abandon(&lease,1)); cleanup(&f);
    start(&f); assert(!uu_portal_request_explicit_protocol(f.p,target_modifier,1));
    format(&f,1); f.timeline.flags=0; incoming[0]=&f.buffer; count=1;
    assert(!uu_portal_async_acquire(f.p,&lease,&snapshot));
    assert(!uu_portal_async_finish(&lease,1,release_flagless,NULL));
    assert(queues==1 && releases==1 && f.timeline.flags==0);
    assert(!uu_portal_request_explicit_protocol(f.p,target_modifier,1));
    assert(updates==2 && offers==1 && queues==1 && releases==1);
    cleanup(&f);
    puts("PASS: explicit format/buffer/meta negotiation, process-to-lease FIFO delivery, backpressure, rejection and no premature queue on reconfigure/disable/reset");
    return 0;
}
