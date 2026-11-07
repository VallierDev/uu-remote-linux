#include "portal_nvenc_spa.h"
#include <limits.h>
#include <string.h>

#define PNSPA_MAX_METAS 64u

static int valid_fd(int64_t fd) { return fd >= 0 && fd <= INT_MAX; }

int pnspa_snapshot(const struct spa_buffer *b, const struct spa_video_info_raw *f,
                   uint64_t expected_modifier, struct pnspa_snapshot *out)
{
    struct pnspa_snapshot s = {0};
    struct spa_meta_sync_timeline timeline;
    const struct spa_meta *sync = NULL;
    const struct spa_data *d;
    const struct spa_chunk *c;
    if (!b || !f || !out || b->n_datas != 3 || !b->datas ||
        b->n_metas > PNSPA_MAX_METAS || (b->n_metas && !b->metas)) return 0;
    if (!(f->flags & SPA_VIDEO_FLAG_MODIFIER) || f->modifier != expected_modifier ||
        f->modifier == UINT64_C(0x00ffffffffffffff) || !f->size.width || !f->size.height)
        return 0;
    switch (f->format) {
    case SPA_VIDEO_FORMAT_BGRA: case SPA_VIDEO_FORMAT_BGRx:
    case SPA_VIDEO_FORMAT_RGBA: case SPA_VIDEO_FORMAT_RGBx: break;
    default: return 0;
    }
    d = &b->datas[0]; c = d->chunk;
    if (d->type != SPA_DATA_DmaBuf || !valid_fd(d->fd) || !c ||
        !d->maxsize || !c->size || c->stride <= 0 || c->flags != 0 ||
        c->offset >= d->maxsize || c->size > d->maxsize - c->offset ||
        d->mapoffset > UINT32_MAX - d->maxsize) return 0;
    /* Only linear layouts have a meaningful packed row-size calculation. */
    if (f->modifier == 0) {
        uint64_t row = (uint64_t)f->size.width * 4;
        uint64_t required = (uint64_t)(f->size.height - 1) * (uint32_t)c->stride + row;
        if (row > (uint32_t)c->stride || required > c->size) return 0;
    }
    for (unsigned i = 1; i < 3; i++)
        if (b->datas[i].type != SPA_DATA_SyncObj || !valid_fd(b->datas[i].fd)) return 0;
    for (uint32_t i = 0; i < b->n_metas; i++) {
        const struct spa_meta *m = &b->metas[i];
        if (m->type == SPA_META_SyncTimeline) {
            if (sync || !m->data || m->size != sizeof(timeline)) return 0;
            sync = m;
        }
    }
    if (!sync) return 0;
    memcpy(&timeline, sync->data, sizeof(timeline));
    if (!timeline.acquire_point || timeline.release_point <= timeline.acquire_point ||
        (timeline.flags & ~((uint32_t)SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE))) return 0;
    s.format = f->format; s.width = f->size.width; s.height = f->size.height;
    s.modifier = f->modifier; s.pixel_fd = (int)d->fd;
    s.acquire_fd = (int)b->datas[1].fd; s.release_fd = (int)b->datas[2].fd;
    s.mapoffset = d->mapoffset; s.maxsize = d->maxsize;
    s.offset = c->offset; s.size = c->size; s.stride = c->stride; s.chunk_flags = c->flags;
    s.acquire_point = timeline.acquire_point; s.release_point = timeline.release_point;
    s.sync_flags = timeline.flags;
    *out = s;
    return 1;
}
