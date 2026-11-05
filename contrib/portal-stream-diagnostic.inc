/* Bounded, explicitly selected trusted flagless producer diagnostic, not host
 * admission. All PipeWire callbacks and release/queue run on this one thread.
 * No other consumer/signaler is permitted for this new private Portal session.
 * Snapshot equality and epoch checks cannot establish that external authority. */
static int same_source_frame(const struct pnspa_snapshot *a,const struct pnspa_snapshot *b) {
#define EQ(f) (a->f==b->f)
    return EQ(format)&&EQ(width)&&EQ(height)&&EQ(modifier)&&EQ(pixel_fd)&&EQ(acquire_fd)&&EQ(release_fd)&&
        EQ(mapoffset)&&EQ(maxsize)&&EQ(offset)&&EQ(size)&&EQ(stride)&&EQ(chunk_flags)&&
        EQ(acquire_point)&&EQ(release_point)&&EQ(sync_flags);
#undef EQ
}
static int live_stream_frame(struct probe *p) {
    if (p->stop_reasons || !p->held || p->done!=1 || p->epoch!=p->frame_epoch ||
        p->stream_state!=PW_STREAM_STATE_STREAMING || !p->dma_format) return 0;
    struct pnspa_snapshot live;
    return pnspa_snapshot(p->held->buffer,&p->fixed,p->fixed.modifier,&live) &&
        live.sync_flags==0 && same_source_frame(&live,&p->source_frame);
}
static void park_stream_diagnostic(const char *stage) {
    fprintf(stderr,"Stream diagnostic quarantined at %s; owners retained, no retry\n",stage);
    fflush(stderr);
    for (;;) g_usleep(G_USEC_PER_SEC);
}
static int dispatch_stream_control(struct probe *p) {
    struct pw_loop *loop=pw_main_loop_get_loop(p->loop);
    pw_loop_enter(loop);
    int result=pw_loop_iterate(loop,0);
    pw_loop_leave(loop);
    dispatch_portal_events(NULL,0);
    return result;
}
static int run_stream_diagnostic(struct probe *p,struct spa_source *timer) {
    struct pnegl_device *device=NULL;
    NvencEngine *encoder=NULL;
    struct pnegl_image image={0};
    struct { struct pw_buffer *buffer; uint64_t release; } seen[8]={0};
    unsigned seen_count=0,reused=0,returned=0,encoded_count=0,returned_after_stop=0;
    gint64 began=g_get_monotonic_time(),deadline=began+60*G_TIME_SPAN_SECOND;
    int measure=p->encode_check==4;
    unsigned limit=measure ? 330 : (p->encode_check==3 ? 600 : 8);
    gint64 measured_start=0,measured_end=0;
    struct timespec cpu_start={0},cpu_end={0};
    uint64_t measured_bytes=0;
    unsigned measured_frames=0;
    int render=-1,result=1; uint32_t width=0,height=0;
    for (unsigned frame=0;frame<limit;frame++) {
        if (p->stop_reasons) goto stopped;
        if (measure && frame==30) {
            if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID,&cpu_start)) goto out;
            measured_start=g_get_monotonic_time();
            fprintf(stderr,"Measurement begin: monotonic_us=%" G_GINT64_FORMAT " warmup=30 frames=300 encoder_fps=30 gop=30 cq=23 max_bps=2000000 decode_in_interval=0\n",measured_start);
        }
        p->done=0;
        gint64 remaining=deadline-g_get_monotonic_time();
        if (remaining<=0) { stop_probe(p,STOP_TIMEOUT); goto stopped; }
        struct timespec delay={.tv_sec=MIN(15,(remaining+G_TIME_SPAN_SECOND-1)/G_TIME_SPAN_SECOND)};
        if (pw_loop_update_timer(pw_main_loop_get_loop(p->loop),timer,&delay,NULL,false)<0) goto out;
        if (pw_main_loop_run(p->loop)<0) goto out;
        if (!live_stream_frame(p)) goto stopped;
        unsigned slot=0;
        for (;slot<seen_count;slot++) if (seen[slot].buffer==p->held) break;
        if (slot<seen_count) {
            if (p->source_frame.acquire_point<=seen[slot].release) goto out;
            reused++;
        } else {
            if (seen_count>=G_N_ELEMENTS(seen)) goto out;
            seen[slot].buffer=p->held; seen_count++;
        }
        if (!encoder) {
            width=p->image_frame.width; height=p->image_frame.height;
            if (p->image_frame.format!=SPA_VIDEO_FORMAT_BGRA || !width || !height || width%2 || height%2) goto out;
            render=open(p->acquire_device,O_RDWR|O_CLOEXEC); if (render<0) goto out;
            if (pnegl_device_open(&device,render)) goto out;
            NvencConfig config={.width=width,.height=height,.fps=30,.gop=30,.cq=23,
                .target_bps=2000000,.max_bps=2000000,.buffer_bits=2000000};
            encoder=nvenc_create(&config);
            if (!encoder || nvenc_check_device(encoder,pnegl_device_pci(device))) goto out;
        }
        if (p->image_frame.width!=width || p->image_frame.height!=height ||
            p->image_frame.format!=SPA_VIDEO_FORMAT_BGRA) goto out;
        if (observe_acquire(p->acquire_device,p->acquire_fd,p->acquire_point,p->release_fd,p->release_point,2000)) goto out;
        // Dispatch pending control events before image access. process does not
        // dequeue anything while a lease is held. Any generation change rejects it.
        if (dispatch_stream_control(p)<0 || !live_stream_frame(p)) goto stopped;
        if (pnegl_image_create(&image,device->display,&p->image_frame,device->create_image,device->destroy_image)) goto out;
        const uint8_t *packet=NULL; uint32_t length=0; int complete=0;
        int encoded=nvenc_encode_egl(encoder,image.image,pnegl_device_pci(device),!measure || frame==0,&packet,&length,&complete);
        if (encoded || !complete) park_stream_diagnostic("encode");
        encoded_count++;
        int decoded=(!measure || frame==0) ? decode_one_frame(packet,length,width,height) : 0;
        if (pnegl_image_destroy_completed(&image,1)) park_stream_diagnostic("image-destroy");
        if (decoded) goto out;
        if (dispatch_stream_control(p)<0 || !live_stream_frame(p)) goto stopped;
        uint32_t release=0; uint64_t point=0;
        if (drmSyncobjFDToHandle(render,p->release_fd,&release)) goto out;
        if (drmSyncobjQuery(render,&release,&point,1) || point>=p->release_point) {
            if (drmSyncobjDestroy(render,release)) park_stream_diagnostic("query-cleanup");
            goto out;
        }
        // Selected profile has flags==0: signal exactly once only after ALL
        // source views/work ended. No other actor may signal this frame's release.
        if (drmSyncobjTimelineSignal(render,&release,&p->release_point,1)) park_stream_diagnostic("release-signal");
        if (drmSyncobjQuery(render,&release,&point,1) || point!=p->release_point) park_stream_diagnostic("release-confirm");
        if (drmSyncobjDestroy(render,release)) park_stream_diagnostic("release-handle-destroy");
        if (pw_stream_queue_buffer(p->stream,p->held)<0) park_stream_diagnostic("queue-after-release");
        seen[slot].release=p->release_point;
        p->held=NULL; returned++;
        if (p->stop_at) returned_after_stop++;
        close(p->pixel_fd); p->pixel_fd=-1;
        close(p->acquire_fd); p->acquire_fd=-1;
        close(p->release_fd); p->release_fd=-1;
        if (measure && frame>=30) {
            measured_frames++; measured_bytes+=length;
            measured_end=g_get_monotonic_time();
            if (clock_gettime(CLOCK_PROCESS_CPUTIME_ID,&cpu_end)) { measured_end=0; goto out; }
        }
        if (!measure) fprintf(stderr,"Stream diagnostic: frame=%u decoded=1 released=1 queued=1 packet_bytes=%u\n",frame+1,length);
    }
    result=p->encode_check==3 ? 1 : 0;
    goto out;
stopped:
    if (p->encode_check==3 && p->stop_reasons) {
        // Correlate stream loss with the actual Portal Closed notification.
        // No frame access/release occurs while waiting for this control event.
        gint64 until=g_get_monotonic_time()+500000;
        while (!p->portal_closed && g_get_monotonic_time()<until) {
            dispatch_portal_events(NULL,0);
            if (!p->portal_closed) g_usleep(5000);
        }
        if (p->portal_closed && returned>0 && !returned_after_stop) result=0;
    }
out:
    if (measure && measured_start && measured_end>measured_start) {
        double seconds=(measured_end-measured_start)/1000000.0;
        double cpu=(cpu_end.tv_sec-cpu_start.tv_sec)+(cpu_end.tv_nsec-cpu_start.tv_nsec)/1e9;
        fprintf(stderr,"Measurement: complete=%d monotonic_us=%" G_GINT64_FORMAT " frames=%u seconds=%.3f fps=%.3f process_cpu_percent=%.3f cpu_ms_per_frame=%.3f packet_bytes=%" PRIu64 " width=%u height=%u\n",
            result==0 && measured_frames==300,measured_end,measured_frames,seconds,measured_frames/seconds,
            cpu/seconds*100,cpu*1000/measured_frames,measured_bytes,width,height);
    }
    // Only known-completed/no-submit exits reach here. No packet is published.
    if (image.image && pnegl_image_destroy_completed(&image,1)) park_stream_diagnostic("final-image-destroy");
    if (encoder) nvenc_destroy(encoder);
    if (device && pnegl_device_close_idle(&device,1)) park_stream_diagnostic("device-close");
    if (render>=0) close(render);
    fprintf(stderr,"Stream diagnostic: returned=%u distinct_buffers=%u reused_deliveries=%u result=%d\n",
        returned,seen_count,reused,result);
    fprintf(stderr,"Stream lifecycle: encoded=%u portal_closed=%d stop_reasons=%d returned_after_stop=%u elapsed_ms=%" G_GINT64_FORMAT "\n",
        encoded_count,p->portal_closed,p->stop_reasons,returned_after_stop,(g_get_monotonic_time()-began)/1000);
    return result;
}
