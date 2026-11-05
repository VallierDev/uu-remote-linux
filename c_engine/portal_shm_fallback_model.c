#include "portal_shm_fallback_model.h"

void shm_init(struct shm_model *m) { *m = (struct shm_model){ SHM_IDLE, 0, 0 }; }
int shm_request(struct shm_model *m, uint64_t epoch) {
    if ((m->state != SHM_IDLE && m->state != SHM_FAILED && m->state != SHM_READY) ||
        !epoch || epoch <= m->epoch) return 0;
    m->epoch = epoch;
    m->state = SHM_REQUESTING;
    return 1;
}
static int current(const struct shm_model *m, uint64_t epoch) {
    return epoch && epoch == m->epoch;
}
void shm_request_result(struct shm_model *m, uint64_t epoch, int success) {
    if (current(m, epoch) && m->state == SHM_REQUESTING)
        m->state = success ? SHM_WAIT_FORMAT : SHM_FAILED;
}
void shm_format_event(struct shm_model *m, uint64_t epoch, uint64_t generation, enum shm_format format) {
    if (m->state != SHM_WAIT_FORMAT && m->state != SHM_WAIT_PIXELS && m->state != SHM_READY) return;
    if (!epoch) { m->state = SHM_FAILED; return; }
    if (!current(m, epoch)) return;
    if (format != SHM_FORMAT_SHM || !generation || generation <= m->generation) {
        m->state = SHM_FAILED;
        return;
    }
    m->generation = generation;
    m->state = SHM_WAIT_PIXELS;
}
void shm_buffer_event(struct shm_model *m, uint64_t epoch, uint64_t generation, enum shm_buffer buffer) {
    if (m->state != SHM_WAIT_PIXELS && m->state != SHM_READY) return;
    /* Unknown identity cannot substantiate even a previously ready image. */
    if (!epoch || !generation) { m->state = SHM_WAIT_PIXELS; return; }
    if (current(m, epoch) && generation == m->generation &&
        m->state == SHM_WAIT_PIXELS && buffer == SHM_PIXELS_VALID)
        m->state = SHM_READY;
}
void shm_timeout(struct shm_model *m, uint64_t epoch) {
    if (current(m, epoch) && (m->state == SHM_REQUESTING || m->state == SHM_WAIT_FORMAT || m->state == SHM_WAIT_PIXELS))
        m->state = SHM_FAILED;
}
void shm_close(struct shm_model *m) { m->state = SHM_CLOSED; }
