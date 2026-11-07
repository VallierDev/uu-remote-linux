#include "portal_nvenc_admission.h"
#include <errno.h>
#include <string.h>

static int valid(const struct pna_frame *f, const struct pna_evidence *e) {
    uint64_t row, need;
    if (!f || !e || e->explicit_opt_in != 1 || e->negotiated != 1 ||
        e->mutter_same_gpu != 1 || e->trusted_producer != 1 ||
        e->exclusive_timeline != 1 || e->pinned_lease_and_metadata != 1 ||
        e->verified_fd_and_size != 1 || e->stable_unique_buffer_timeline != 1 ||
        !f->buffer_id || f->dmabuf != 1 || f->metadata_valid != 1 ||
        f->unscheduled_release != 1 || f->full_frame != 1 || f->fd < 0 ||
        f->planes != 1 || !f->width || !f->height || f->width > 8192 || f->height > 8192 ||
        f->width != e->width || f->height != e->height || f->fourcc != e->fourcc ||
        f->modifier == UINT64_C(0x00ffffffffffffff) || f->modifier != e->modifier || !f->generation ||
        !f->acquire || f->acquire >= f->release) return 0;
    switch (f->fourcc) {
    case 0x34325258: case 0x34325241: case 0x34324258: case 0x34324241: break;
    default: return 0;
    }
    if (f->modifier != 0) {
        return e->verified_tiled_layout_fd_size_and_device == 1 && f->stride &&
            f->chunk_size && f->offset <= f->allocation_size &&
            f->chunk_size <= f->allocation_size - f->offset;
    }
    row = (uint64_t)f->width * 4;
    if (f->stride < row || f->offset > f->allocation_size) return 0;
    need = (uint64_t)(f->height - 1) * f->stride + row;
    return need <= f->chunk_size && f->chunk_size <= f->allocation_size - f->offset;
}
int pna_init(struct pna_session *s) {
    int r; if (!s) return -EINVAL;
    memset(s, 0, sizeof(*s)); r = pthread_mutex_init(&s->lock, NULL);
    return r ? -r : 0;
}
int pna_admit(struct pna_session *s, const struct pna_frame *f, const struct pna_evidence *e) {
    int r = 0; size_t i; struct pna_history *h = NULL, *empty = NULL;
    if (!s || !valid(f, e)) return -EINVAL;
    pthread_mutex_lock(&s->lock);
    if (s->state != PNA_IDLE && s->state != PNA_RELEASED) r = -EBUSY;
    else {
        for (i = 0; i < PNA_MAX_BUFFERS; i++) {
            if (s->history[i].id == f->buffer_id) { h = &s->history[i]; break; }
            if (!s->history[i].id && !empty) empty = &s->history[i];
        }
        if (!h) h = empty;
        if (!h) r = -ENOSPC;
        else if (f->generation <= h->generation || f->acquire <= h->last_release) r = -ESTALE;
        else {
            h->id = f->buffer_id; h->generation = f->generation; h->last_release = f->release;
            s->active_id = f->buffer_id; s->generation = f->generation; s->state = PNA_LEASED;
        }
    }
    pthread_mutex_unlock(&s->lock); return r;
}
static int matches(struct pna_session *s, uint64_t id, uint64_t g) {
    return id && id == s->active_id && g && g == s->generation;
}
int pna_acquired(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state != PNA_LEASED) r = -EINVAL;
    else if (ok != 1) { s->state = PNA_REVOKED; r = -EPERM; }
    else s->state = PNA_ACQUIRED;
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_gpu_begin(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state != PNA_ACQUIRED) r = -EINVAL;
    else if (ok != 1) { s->state = PNA_REVOKED; r = -EPERM; }
    else { s->state = PNA_GPU; s->gpu_pending = 1; }
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_gpu_complete(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || !s->gpu_pending || (s->state != PNA_GPU && s->state != PNA_REVOKED)) r = -EINVAL;
    else if (ok != 1) { s->state = PNA_REVOKED; r = -EPERM; }
    else { s->gpu_pending = 0; if (s->state == PNA_GPU) s->state = PNA_READY; }
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_no_gpu(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state != PNA_ACQUIRED) r = -EINVAL;
    else if (ok != 1) { s->state = PNA_REVOKED; r = -EPERM; }
    else s->state = PNA_READY;
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_release(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state != PNA_READY) r = -EINVAL;
    else if (ok != 1) { s->state = PNA_REVOKED; r = -EPERM; }
    else s->state = PNA_RELEASED;
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_revoke(struct pna_session *s, uint64_t id, uint64_t g) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state == PNA_RELEASED || s->state == PNA_ABANDONED || s->state == PNA_IDLE) r = -EINVAL;
    else s->state = PNA_REVOKED;
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_abandon(struct pna_session *s, uint64_t id, uint64_t g, int ok) {
    int r = 0; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (!matches(s,id,g) || s->state != PNA_REVOKED) r = -EINVAL;
    else if (ok != 1 || s->gpu_pending) r = -EBUSY;
    else s->state = PNA_ABANDONED;
    pthread_mutex_unlock(&s->lock); return r;
}
int pna_destroy(struct pna_session *s) {
    int r; if (!s) return -EINVAL; pthread_mutex_lock(&s->lock);
    if (s->state != PNA_IDLE && s->state != PNA_RELEASED && s->state != PNA_ABANDONED) { pthread_mutex_unlock(&s->lock); return -EBUSY; }
    pthread_mutex_unlock(&s->lock); r = pthread_mutex_destroy(&s->lock); return r ? -r : 0;
}
