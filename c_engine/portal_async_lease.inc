static int describe_explicit_buffer(struct uu_portal *p, struct spa_buffer *b, UuPortalAsyncFrame *frame) {
#if !PW_CHECK_VERSION(1, 2, 0)
    (void)p; (void)b; (void)frame; return -ENOTSUP;
#else
    if (!b || b->n_datas != 3 || !b->datas || !p->width || !p->height || p->modifier == UINT64_C(0x00ffffffffffffff)) return -EINVAL;
    if (b->n_metas > 64 || (b->n_metas && !b->metas) || p->width > 8192 || p->height > 8192) return -EINVAL;
    unsigned sync_count = 0;
    for (uint32_t i = 0; i < b->n_metas; i++) {
        if (b->metas[i].type == SPA_META_SyncTimeline &&
            (++sync_count != 1 || b->metas[i].size != sizeof(struct spa_meta_sync_timeline) || !b->metas[i].data)) return -EINVAL;
    }
    struct spa_data *d = b->datas;
    struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(b, SPA_META_SyncTimeline, sizeof(*m));
    struct spa_meta_header *h = spa_buffer_find_meta_data(b, SPA_META_Header, sizeof(*h));
    struct spa_meta_region *crop = spa_buffer_find_meta_data(b, SPA_META_VideoCrop, sizeof(*crop));
    /* No automatic detection: flags=0 is admitted only under an independently
     * established flagless release-point producer contract selected up front. */
    uint32_t expected_flags = p->explicit_protocol == 1 ? 0 : SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE;
    if (m && m->flags != expected_flags) return -EPROTONOSUPPORT;
    if (!m || !m->acquire_point || m->acquire_point >= m->release_point ||
        m->flags != expected_flags || (h && h->flags) ||
        (crop && spa_meta_region_is_valid(crop) && (crop->region.position.x || crop->region.position.y ||
         crop->region.size.width != p->width || crop->region.size.height != p->height)) ||
        d[0].type != SPA_DATA_DmaBuf || d[1].type != SPA_DATA_SyncObj || d[2].type != SPA_DATA_SyncObj ||
        !d[0].chunk || d[0].chunk->flags || d[0].chunk->stride <= 0 || !d[0].chunk->size ||
        d[0].chunk->offset >= d[0].maxsize || d[0].chunk->size > d[0].maxsize - d[0].chunk->offset ||
        d[0].mapoffset > UINT32_MAX - d[0].maxsize) return -EINVAL;
    for (int i = 0; i < 3; i++)
        if (d[i].fd < 0 || d[i].fd > INT_MAX || d[i].flags != SPA_DATA_FLAG_READABLE) return -EINVAL;
    uint32_t fourcc;
    switch (p->format) {
    case SPA_VIDEO_FORMAT_BGRA: fourcc = 0x34325241; break;
    case SPA_VIDEO_FORMAT_BGRx: fourcc = 0x34325258; break;
    case SPA_VIDEO_FORMAT_RGBA: fourcc = 0x34324241; break;
    case SPA_VIDEO_FORMAT_RGBx: fourcc = 0x34324258; break;
    default: return -EINVAL;
    }
    if (!p->modifier && ((uint64_t)p->width * 4 > (uint32_t)d[0].chunk->stride ||
        (uint64_t)(p->height - 1) * d[0].chunk->stride + (uint64_t)p->width * 4 > d[0].chunk->size)) return -EINVAL;
    uint64_t epoch = uu_portal_capture_epoch(p);
    if (epoch == UINT64_MAX) return -ECANCELED;
    *frame = (UuPortalAsyncFrame){
        .pixels = { .fd = (int)d[0].fd, .fourcc = fourcc, .width = p->width, .height = p->height,
            .stride = d[0].chunk->stride, .offset = d[0].mapoffset + d[0].chunk->offset,
            .size = d[0].mapoffset + d[0].maxsize, .modifier = p->modifier },
        .acquire_fd = (int)d[1].fd, .release_fd = (int)d[2].fd,
        .acquire_point = m->acquire_point, .release_point = m->release_point, .epoch = epoch };
    return 0;
#endif
}
/* Explicit-sync storage. Producer admission remains the caller's responsibility.
 * All pending/current/async changes use loop lock then frame lock. */
static void async_free(struct uu_portal_async_lease **lease) {
    struct uu_portal_async_lease *l = *lease;
    close(l->frame.pixels.fd); close(l->frame.acquire_fd); close(l->frame.release_fd);
    l->portal->dma_async = NULL;
    free(l); *lease = NULL;
}
int uu_portal_async_cancelled(const struct uu_portal_async_lease *l) {
    return !l || uu_portal_capture_cancelled(l->portal, l->frame.epoch);
}
int uu_portal_async_acquire(struct uu_portal *p, struct uu_portal_async_lease **out, UuPortalAsyncFrame *frame) {
    if (!p || !out || *out || !frame || !p->pw_loop || !p->stream) return -EINVAL;
#if !PW_CHECK_VERSION(1, 2, 0)
    return -ENOTSUP;
#else
    int r = -EAGAIN;
    pw_thread_loop_lock(p->pw_loop);
    if (p->explicit_requested) stream_process(p); /* Drain an already queued frame after backpressure. */
    pthread_mutex_lock(&p->lock);
    if (p->dma_async || p->dma_current) { r = -EBUSY; goto done; }
    if (atomic_load(&p->closed)) { r = -ECANCELED; goto done; }
    if (p->error || p->dma_blocked) { r = p->explicit_error ? p->explicit_error : -ECANCELED; goto done; }
    if (p->explicit_requested && !p->dma_seen && g_get_monotonic_time() - p->dma_requested_at > 2 * G_TIME_SPAN_SECOND) { r = -ETIMEDOUT; goto done; }
    if (!p->format_valid || !p->dma_format || !p->dma_pending) goto done;
    UuPortalAsyncFrame snapshot;
    r = describe_explicit_buffer(p, p->dma_pending->buffer, &snapshot);
    if (r) goto done;
    int borrowed[3] = {snapshot.pixels.fd, snapshot.acquire_fd, snapshot.release_fd};
    int owned[3] = {-1,-1,-1};
    for (int i = 0; i < 3; i++) {
        owned[i] = fcntl(borrowed[i], F_DUPFD_CLOEXEC, 0);
        if (owned[i] < 0) {
            r = -errno; for (int j = 0; j < i; j++) close(owned[j]); goto done;
        }
    }
    struct uu_portal_async_lease *l = calloc(1, sizeof(*l));
    if (!l) { for (int i = 0; i < 3; i++) close(owned[i]); r = -ENOMEM; goto done; }
    l->portal = p; l->buffer = p->dma_pending;
    l->frame = snapshot;
    l->frame.pixels.fd = owned[0]; l->frame.acquire_fd = owned[1]; l->frame.release_fd = owned[2];
    p->dma_async = l; p->dma_pending = NULL; *out = l; *frame = l->frame; r = 0;
done:
    pthread_mutex_unlock(&p->lock); pw_thread_loop_unlock(p->pw_loop);
    return r;
#endif
}
int uu_portal_async_finish(struct uu_portal_async_lease **lease, int gpu_complete,
    int (*release)(void *, struct spa_meta_sync_timeline *), void *opaque) {
    if (!lease || !*lease || !release) return -EINVAL;
    if (gpu_complete != 1) return -EBUSY;
#if !PW_CHECK_VERSION(1, 2, 0)
    (void)opaque; return -ENOTSUP;
#else
    struct uu_portal_async_lease *l = *lease; struct uu_portal *p = l->portal;
    pw_thread_loop_lock(p->pw_loop); pthread_mutex_lock(&p->lock);
    int r = -ECANCELED;
    if (l->attempted || uu_portal_async_cancelled(l) || !l->buffer) goto done;
    struct spa_meta_sync_timeline *m = spa_buffer_find_meta_data(l->buffer->buffer, SPA_META_SyncTimeline, sizeof(*m));
    if (!m || m->acquire_point != l->frame.acquire_point || m->release_point != l->frame.release_point ||
        m->flags != (p->explicit_protocol == 1 ? 0u : SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE)) goto done;
    l->attempted = 1;
    r = release(opaque, m);
    if (r) { if (r > 0) r = -EIO; goto done; }
    if (p->explicit_protocol == 1 ? m->flags != 0 : (m->flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE) != 0) { r = -EPROTO; goto done; }
    r = pw_stream_queue_buffer(p->stream, l->buffer);
    if (r < 0) goto done;
    async_free(lease); r = 0;
done:
    pthread_mutex_unlock(&p->lock); pw_thread_loop_unlock(p->pw_loop);
    return r;
#endif
}
int uu_portal_async_abandon(struct uu_portal_async_lease **lease, int gpu_complete) {
    if (!lease || !*lease) return -EINVAL;
    struct uu_portal_async_lease *l = *lease; struct uu_portal *p = l->portal;
    if (gpu_complete != 1 || !atomic_load(&p->closed)) return -EBUSY;
    pw_thread_loop_lock(p->pw_loop); pthread_mutex_lock(&p->lock);
    async_free(lease);
    pthread_mutex_unlock(&p->lock); pw_thread_loop_unlock(p->pw_loop);
    return 0;
}
