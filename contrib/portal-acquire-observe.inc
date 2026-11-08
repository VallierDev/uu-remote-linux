/* Read-only observation, not admission: never signal/transfer a timeline, map
 * pixels or submit GPU work. Caller holds the original unqueued buffer and
 * duplicated syncobj fds. A successful observation says nothing about pending
 * higher signals, producer authority, future writes or session termination. */
static int observe_acquire(const char *device, int acquire_fd, uint64_t acquire_point,
                          int release_fd, uint64_t release_point, unsigned timeout_ms) {
    if (!device || acquire_fd<0 || release_fd<0 || !acquire_point || release_point<=acquire_point || timeout_ms>2000)
        return -EINVAL;
    int drm=open(device,O_RDWR|O_CLOEXEC); if (drm<0) return -errno;
    uint32_t acquire=0,release=0; int result=-EIO;
    if (drmSyncobjFDToHandle(drm,acquire_fd,&acquire)) goto out;
    if (drmSyncobjFDToHandle(drm,release_fd,&release)) goto out;
    uint64_t point=0;
    if (drmSyncobjQuery(drm,&release,&point,1)) goto out;
    if (point>=release_point) { result=-ESTALE; goto out; }
    int64_t deadline=g_get_monotonic_time()*1000+(int64_t)timeout_ms*1000000;
    int waited;
    do {
        waited=drmSyncobjTimelineWait(drm,&acquire,&acquire_point,1,deadline,
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,NULL);
    } while (waited && errno==EINTR && g_get_monotonic_time()*1000<deadline);
    if (waited) { result=-(errno ? errno : EIO); goto out; }
    if (drmSyncobjQuery(drm,&release,&point,1)) goto out;
    result=point<release_point ? 0 : -ESTALE;
out:
    if (release && release!=acquire && drmSyncobjDestroy(drm,release)) result=-EIO;
    if (acquire && drmSyncobjDestroy(drm,acquire)) result=-EIO;
    close(drm); return result;
}
