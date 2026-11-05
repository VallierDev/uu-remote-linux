#ifndef PORTAL_SHM_FALLBACK_MODEL_H
#define PORTAL_SHM_FALLBACK_MODEL_H
#include <stdint.h>

/* Isolated, serialized model, not a PipeWire interface. The caller must
 * independently authenticate BOTH epoch and format-generation ownership of
 * every format callback and buffer, not assign them from callback order or
 * buffer type. Zero means unknown. No such production proof is asserted.
 * Epoch tokens and format generations are separately strictly increasing,
 * never reused, across this model's lifetime; exhaustion is fail closed.
 * Pixel validity means independently checked nonempty, uncorrupted,
 * accessible SHM pixels. Replayed format generations fail the epoch. */
enum shm_state { SHM_IDLE, SHM_REQUESTING, SHM_WAIT_FORMAT, SHM_WAIT_PIXELS,
                 SHM_READY, SHM_FAILED, SHM_CLOSED };
enum shm_format { SHM_FORMAT_SHM, SHM_FORMAT_OTHER, SHM_FORMAT_ERROR };
enum shm_buffer { SHM_PIXELS_VALID, SHM_EMPTY, SHM_DMA, SHM_INVALID };
struct shm_model { enum shm_state state; uint64_t epoch, generation; };
void shm_init(struct shm_model *m);
/* Begins a new request only from IDLE, FAILED or READY with a larger token. */
int shm_request(struct shm_model *m, uint64_t epoch);
void shm_request_result(struct shm_model *m, uint64_t epoch, int success);
void shm_format_event(struct shm_model *m, uint64_t epoch, uint64_t generation, enum shm_format format);
void shm_buffer_event(struct shm_model *m, uint64_t epoch, uint64_t generation, enum shm_buffer buffer);
void shm_timeout(struct shm_model *m, uint64_t epoch);
void shm_close(struct shm_model *m);
#endif
