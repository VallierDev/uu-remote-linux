/* Offline guard checks: no D-Bus, real frames or GPU work. */
#define main portal_probe_main
#include "../contrib/portal-dmabuf-probe.c"
#undef main
static gboolean inject_closed(gpointer data) {
    portal_closed_cb(NULL,NULL,NULL,NULL,NULL,NULL,data);
    return G_SOURCE_REMOVE;
}
int main(void) {
    pw_init(NULL,NULL);
    struct spa_chunk chunk={.size=32,.stride=16};
    struct spa_data data[3]={{.type=SPA_DATA_DmaBuf,.fd=3,.maxsize=32,.chunk=&chunk},
        {.type=SPA_DATA_SyncObj,.fd=4},{.type=SPA_DATA_SyncObj,.fd=5}};
    struct spa_meta_sync_timeline timeline={.acquire_point=1,.release_point=2};
    struct spa_meta meta={.type=SPA_META_SyncTimeline,.size=sizeof(timeline),.data=&timeline};
    struct spa_buffer spa={.n_datas=3,.datas=data,.n_metas=1,.metas=&meta};
    struct pw_buffer buffer={.buffer=&spa};
    struct probe p={.done=1,.dma_format=1,.held=&buffer,.epoch=1,.frame_epoch=1,
        .stream_state=PW_STREAM_STATE_STREAMING,
        .fixed={.format=SPA_VIDEO_FORMAT_BGRA,.flags=SPA_VIDEO_FLAG_MODIFIER,.size={4,2},.modifier=0}};
    p.loop=pw_main_loop_new(NULL); assert(p.loop);
    assert(pnspa_snapshot(&spa,&p.fixed,0,&p.source_frame));
    assert(live_stream_frame(&p));
    // Holding a frame prevents another process callback from overwriting its owners.
    process(&p); assert(p.held==&buffer && live_stream_frame(&p));
    timeline.flags=1; assert(!live_stream_frame(&p)); timeline.flags=0;
    timeline.release_point=3; assert(!live_stream_frame(&p)); timeline.release_point=2;
    data[2].fd=6; assert(!live_stream_frame(&p)); data[2].fd=5;
    p.fixed.size.width=2; assert(!live_stream_frame(&p)); p.fixed.size.width=4;
    p.epoch++; assert(!live_stream_frame(&p)); p.epoch--;
    p.stream_state=PW_STREAM_STATE_PAUSED; assert(!live_stream_frame(&p)); p.stream_state=PW_STREAM_STATE_STREAMING;
    assert(live_stream_frame(&p));
    remove_buffer(&p,&buffer); assert(!p.held && !live_stream_frame(&p));
    p.held=&buffer; p.done=1; p.frame_epoch=p.epoch;
    timeout_cb(&p,1); assert(!live_stream_frame(&p));
    p.stop_reasons=0; p.stop_at=0; p.portal_closed=0; p.done=0;
    struct portal_watch watch={.probe=&p};
    struct pw_loop *loop=pw_main_loop_get_loop(p.loop);
    struct spa_source *control=pw_loop_add_timer(loop,dispatch_portal_events,&watch);
    struct spa_source *guard=pw_loop_add_timer(loop,timeout_cb,&p);
    assert(control && guard);
    struct timespec tick={.tv_nsec=20000000},limit={.tv_nsec=500000000};
    assert(!pw_loop_update_timer(loop,control,&tick,&tick,false));
    assert(!pw_loop_update_timer(loop,guard,&limit,NULL,false));
    g_idle_add(inject_closed,&watch);
    assert(!pw_main_loop_run(p.loop));
    assert(watch.closed && p.portal_closed && p.stop_reasons==STOP_PORTAL && p.done==-1);
    assert(!live_stream_frame(&p));
    pw_loop_destroy_source(loop,control); pw_loop_destroy_source(loop,guard);
    pw_main_loop_destroy(p.loop); pw_deinit();
    puts("PASS: stream guard rejects removal, timeout, reconfiguration, flag/point/fd change and stale epoch; held frame cannot be overwritten; Portal Closed dispatched during idle wait");
}
