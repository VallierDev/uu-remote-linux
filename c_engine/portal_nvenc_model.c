#include "portal_nvenc_model.h"

void portal_nvenc_init(struct portal_nvenc_model *m) { m->state = PN_DISABLED; }

enum portal_nvenc_result portal_nvenc_enable_model(struct portal_nvenc_model *m)
{
    if (m->state != PN_DISABLED) return PN_ERROR;
    m->state = PN_READY;
    return PN_OK;
}

enum portal_nvenc_result portal_nvenc_lease(struct portal_nvenc_model *m, int success)
{
    if (m->state != PN_READY) return PN_ERROR;
    if (!success) return PN_ERROR; /* no lease acquired */
    m->state = PN_LEASED;
    return PN_OK;
}

enum portal_nvenc_result portal_nvenc_import(struct portal_nvenc_model *m, int fence_wait_ok, int import_ok)
{
    if (m->state != PN_LEASED) return PN_ERROR;
    if (!fence_wait_ok) return PN_ERROR; /* no GPU access attempted */
    /* Failed import may have partially acquired resources; ownership is unknown. */
    if (!import_ok) { m->state = PN_QUARANTINED; return PN_UNSAFE; }
    m->state = PN_IMPORTED;
    return PN_OK;
}

enum portal_nvenc_result portal_nvenc_submit(struct portal_nvenc_model *m, int success)
{
    if (m->state != PN_IMPORTED) return PN_ERROR;
    /* Even an error may have queued GPU work. */
    m->state = success ? PN_IN_FLIGHT : PN_QUARANTINED;
    return success ? PN_OK : PN_UNSAFE;
}

enum portal_nvenc_result portal_nvenc_wait_gpu(struct portal_nvenc_model *m, int success)
{
    if (m->state != PN_IN_FLIGHT) return PN_ERROR;
    m->state = success ? PN_QUIESCED : PN_QUARANTINED;
    return success ? PN_OK : PN_UNSAFE;
}

enum portal_nvenc_result portal_nvenc_release(struct portal_nvenc_model *m, int success)
{
    if (m->state == PN_IN_FLIGHT || m->state == PN_QUARANTINED) return PN_UNSAFE;
    if (m->state != PN_LEASED && m->state != PN_IMPORTED && m->state != PN_QUIESCED) return PN_ERROR;
    /* Models successful import destruction AND lease return as one operation. */
    m->state = success ? PN_READY : PN_QUARANTINED;
    return success ? PN_OK : PN_UNSAFE;
}

enum portal_nvenc_result portal_nvenc_close(struct portal_nvenc_model *m)
{
    if (m->state == PN_CLOSED) return PN_ERROR;
    if (m->state != PN_READY && m->state != PN_DISABLED) return PN_UNSAFE;
    m->state = PN_CLOSED;
    return PN_OK;
}
