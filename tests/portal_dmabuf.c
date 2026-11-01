#define _GNU_SOURCE
#include <assert.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>

static struct pw_buffer *incoming[8], *returned[32];
static unsigned input_count, input_next, return_count, loop_depth, updates;
static int requested_dma;
static struct pw_buffer *test_dequeue(struct pw_stream *stream) {
    (void)stream;
    assert(loop_depth == 1);
    return input_next < input_count ? incoming[input_next++] : NULL;
}
static int test_queue(struct pw_stream *stream, struct pw_buffer *buffer) {
    (void)stream;
    assert(loop_depth == 1 && return_count < 32);
    returned[return_count++] = buffer;
    return 0;
}
static void test_loop_lock(struct pw_thread_loop *loop) { (void)loop; assert(loop_depth++ == 0); }
static void test_loop_unlock(struct pw_thread_loop *loop) { (void)loop; assert(loop_depth-- == 1); }
static int test_update(struct pw_stream *stream, const struct spa_pod **params, uint32_t count) {
    (void)stream;
    assert(loop_depth == 1);
    if (count == 1) { updates++; return 0; } /* Negotiated buffer requirements. */
    assert(count == 2 || count == 3);
    requested_dma = count == 3;
    assert(!!spa_pod_find_prop(params[0], NULL, SPA_FORMAT_VIDEO_modifier) == requested_dma);
    if (requested_dma) assert(!spa_pod_find_prop(params[1], NULL, SPA_FORMAT_VIDEO_modifier));
    updates++;
    return 0;
}
#define pw_stream_dequeue_buffer test_dequeue
#define pw_stream_queue_buffer test_queue
#define pw_stream_update_params test_update
#define pw_thread_loop_lock test_loop_lock
#define pw_thread_loop_unlock test_loop_unlock
#include "../c_engine/portal_engine.c"

static struct uu_portal *make_portal(void) {
    struct uu_portal *p = calloc(1, sizeof(*p));
    assert(p);
    pthread_mutex_init(&p->lock, NULL);
    pthread_cond_init(&p->changed, NULL);
    p->width=2; p->height=2; p->format=SPA_VIDEO_FORMAT_BGRA; p->format_valid=1;
    p->frame_length=16; p->frame=malloc(16); assert(p->frame); memset(p->frame, 99, 16);
    p->pw_loop=(struct pw_thread_loop *)1; p->stream=(struct pw_stream *)1;
    input_count=input_next=return_count=updates=loop_depth=0;
    return p;
}
static void close_portal(struct uu_portal *p) {
    assert(!loop_depth);
    p->stream=NULL; p->pw_loop=NULL;
    uu_portal_close(p);
}
static void process(struct uu_portal *p, struct pw_buffer *buffer) {
    input_count=1; input_next=0; incoming[0]=buffer;
    test_loop_lock(p->pw_loop); stream_process(p); test_loop_unlock(p->pw_loop);
}
static void descriptor_bounds(void) {
    struct uu_portal *p=make_portal();
    struct spa_chunk chunk={.offset=8, .size=24, .stride=16};
    struct spa_data data={.type=SPA_DATA_DmaBuf, .fd=5, .mapoffset=4, .maxsize=64, .chunk=&chunk};
    struct spa_buffer buffer={.n_datas=1, .datas=&data};
    UuDmaFrame frame;
    assert(describe_dma_buffer(p,&buffer,&frame)==0);
    assert(frame.offset==12 && frame.size==68 && frame.stride==16 && frame.fourcc==0x34325241);
    chunk.size=23; assert(describe_dma_buffer(p,&buffer,&frame)==10); chunk.size=24;
    data.maxsize=31; assert(describe_dma_buffer(p,&buffer,&frame)==14); data.maxsize=64;
    data.mapoffset=UINT32_MAX; assert(describe_dma_buffer(p,&buffer,&frame)==14); data.mapoffset=4;
    chunk.stride=7; assert(describe_dma_buffer(p,&buffer,&frame)==14); chunk.stride=16;
    data.fd=-1; assert(describe_dma_buffer(p,&buffer,&frame)==14); data.fd=5;
    data.type=SPA_DATA_MemFd; assert(describe_dma_buffer(p,&buffer,&frame)==14); data.type=SPA_DATA_DmaBuf;
    buffer.n_datas=2; assert(describe_dma_buffer(p,&buffer,&frame)==14); buffer.n_datas=1;
    p->modifier=1; assert(describe_dma_buffer(p,&buffer,&frame)==19); p->modifier=0;
    chunk.flags=SPA_CHUNK_FLAG_CORRUPTED; assert(describe_dma_buffer(p,&buffer,&frame)==10);
    chunk.flags=SPA_CHUNK_FLAG_EMPTY; assert(describe_dma_buffer(p,&buffer,&frame)==15); chunk.flags=0;
    struct spa_meta_header header={.flags=SPA_META_HEADER_FLAG_CORRUPTED};
    struct spa_meta meta={.type=SPA_META_Header,.size=sizeof(header),.data=&header};
    buffer.n_metas=1; buffer.metas=&meta; assert(describe_dma_buffer(p,&buffer,&frame)==10);
    struct spa_meta_region crop={.region=SPA_REGION(1,0,2,2)};
    meta=(struct spa_meta){.type=SPA_META_VideoCrop,.size=sizeof(crop),.data=&crop};
    assert(describe_dma_buffer(p,&buffer,&frame)==14);
#if PW_CHECK_VERSION(1, 2, 0)
    struct spa_meta_sync_timeline sync={0};
    meta=(struct spa_meta){.type=SPA_META_SyncTimeline,.size=sizeof(sync),.data=&sync};
    assert(describe_dma_buffer(p,&buffer,&frame)==18);
#endif
    close_portal(p);
}
static void ownership_and_reset(void) {
    struct uu_portal *p=make_portal();
    p->dma_requested=p->dma_negotiated=p->dma_format=1;
    struct spa_chunk chunk={.size=16,.stride=8};
    /* Invalid CPU address: the DMA path must never dereference or copy pixels. */
    struct spa_data data={.type=SPA_DATA_DmaBuf,.fd=5,.maxsize=16,.data=(void *)1,.chunk=&chunk};
    struct spa_buffer spa={.n_datas=1,.datas=&data};
    struct pw_buffer a={.buffer=&spa}, b={.buffer=&spa}, c={.buffer=&spa};
    UuDmaFrame frame; uint64_t revision=0;
    process(p,&a);
    assert(p->dma_pending==&a && !return_count && p->frame[0]==99);
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==0 && loop_depth==1);
    assert(p->dma_current==&a && !p->dma_pending && !return_count);
    uu_portal_dmabuf_release(p); assert(!loop_depth && !return_count);
    process(p,&b); process(p,&c);
    assert(return_count==1 && returned[0]==&b && p->dma_current==&a && p->dma_pending==&c);
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==0);
    assert(return_count==2 && returned[1]==&a && p->dma_current==&c);
    uu_portal_dmabuf_release(p);
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==12);
    uu_portal_dmabuf_release(p);
    assert(return_count==2);
    uu_portal_reset_frame(p);
    assert(return_count==3 && returned[2]==&c && !p->dma_current && !p->dma_pending && !p->sequence);
    for (unsigned i=0;i<16;i++) assert(!p->frame[i]);
    process(p,&a);
    stream_remove_buffer(p,&a);
    assert(!p->dma_pending);
    process(p,&b);
    uu_portal_dmabuf_disable(p);
    assert(p->dma_blocked && !requested_dma && !p->dma_pending && !p->dma_current);
    assert(return_count==4 && returned[3]==&b);
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==14 && !loop_depth);
    close_portal(p);
}
static void negotiation_and_black(void) {
    struct uu_portal *p=make_portal();
    UuDmaFrame frame; uint64_t revision=0;
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==10 && !loop_depth);
    assert(requested_dma && updates==1);
    p->dma_negotiated=1; p->dma_format=1;
    p->dma_requested_at=g_get_monotonic_time()-3*G_TIME_SPAN_SECOND;
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==17 && !loop_depth);
    struct spa_chunk chunk={.flags=SPA_CHUNK_FLAG_EMPTY};
    struct spa_data data={.type=SPA_DATA_DmaBuf,.fd=5,.chunk=&chunk};
    struct spa_buffer spa={.n_datas=1,.datas=&data};
    struct pw_buffer buffer={.buffer=&spa};
    process(p,&buffer);
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==15 && !loop_depth);
    p->dma_neutral=0; p->dma_format=0;
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==16 && !loop_depth);
    uu_portal_dmabuf_disable(p);
    assert(!requested_dma && updates==2);
    close_portal(p);
}
static void initial_dma_transport(void) {
    struct uu_portal *p=make_portal();
    uint8_t storage[2048];
    const struct spa_pod *params[3];
    struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    p->width=p->height=0;
    assert(capture_formats(p,&builder,params,0)==2);
    assert(!spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_modifier));
    builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    assert(capture_formats(p,&builder,params,1)==3);
    assert(spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_modifier));
    assert(!spa_pod_find_prop(params[1],NULL,SPA_FORMAT_VIDEO_modifier));
    /* Recovery must request DMA before the first buffer: opening in CPU mode and
     * switching later drops a compositor's already-negotiated DMA first frame. */
    p->dma_requested=1;
    p->dma_bad=18;
    assert(wait_for_first_frame(p)==14); /* Reject unsupported startup buffers without a 10 s stall. */
    p->dma_bad=0;
    builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    const struct spa_pod *format=spa_format_video_raw_build(&builder,SPA_PARAM_Format,
        &SPA_VIDEO_INFO_RAW_INIT(.format=SPA_VIDEO_FORMAT_BGRA,.size=SPA_RECTANGLE(2,2),
            .flags=SPA_VIDEO_FLAG_MODIFIER,.modifier=0,.framerate=SPA_FRACTION(60,1)));
    test_loop_lock(p->pw_loop);
    stream_format(p,SPA_PARAM_Format,format);
    test_loop_unlock(p->pw_loop);
    struct spa_chunk chunk={.size=16,.stride=8};
    struct spa_data data={.type=SPA_DATA_DmaBuf,.fd=5,.maxsize=16,.data=(void *)1,.chunk=&chunk};
    struct spa_buffer spa={.n_datas=1,.datas=&data};
    struct pw_buffer buffer={.buffer=&spa};
    process(p,&buffer);
    assert(wait_for_first_frame(p)==0 && p->dma_pending==&buffer && p->frame[0]==99);
    UuDmaFrame frame; uint64_t revision=0;
    assert(uu_portal_dmabuf_acquire(p,&frame,&revision)==0);
    uu_portal_dmabuf_release(p);
    uu_portal_reset_frame(p);
    close_portal(p);
}
int main(void) {
    descriptor_bounds();
    ownership_and_reset();
    negotiation_and_black();
    initial_dma_transport();
    return 0;
}
