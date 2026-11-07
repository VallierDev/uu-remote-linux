#ifndef PORTAL_NVENC_MODEL_H
#define PORTAL_NVENC_MODEL_H

/* Synthetic lifecycle model only: no descriptors, frames, driver calls or integration.
 * Real Portal modifier compatibility and producer/consumer synchronization are not
 * established. This model cannot authorize or perform a real DMA-BUF import. */
enum portal_nvenc_state {
    PN_DISABLED, PN_READY, PN_LEASED, PN_IMPORTED, PN_IN_FLIGHT,
    PN_QUIESCED, PN_QUARANTINED, PN_CLOSED
};
enum portal_nvenc_result { PN_OK, PN_ERROR, PN_UNSAFE };
struct portal_nvenc_model { enum portal_nvenc_state state; };

void portal_nvenc_init(struct portal_nvenc_model *m);
/* Opt-in enables the synthetic model, never a real capture path. */
enum portal_nvenc_result portal_nvenc_enable_model(struct portal_nvenc_model *m);
enum portal_nvenc_result portal_nvenc_lease(struct portal_nvenc_model *m, int success);
enum portal_nvenc_result portal_nvenc_import(struct portal_nvenc_model *m, int fence_wait_ok, int import_ok);
enum portal_nvenc_result portal_nvenc_submit(struct portal_nvenc_model *m, int success);
enum portal_nvenc_result portal_nvenc_wait_gpu(struct portal_nvenc_model *m, int success);
enum portal_nvenc_result portal_nvenc_release(struct portal_nvenc_model *m, int success);
enum portal_nvenc_result portal_nvenc_close(struct portal_nvenc_model *m);
#endif
