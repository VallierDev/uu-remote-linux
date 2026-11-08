#include "../c_engine/portal_nvenc_model.h"
#include <assert.h>

int main(void)
{
    struct portal_nvenc_model m;
    portal_nvenc_init(&m);
    assert(portal_nvenc_lease(&m, 1) == PN_ERROR);
    assert(portal_nvenc_enable_model(&m) == PN_OK);
    assert(portal_nvenc_lease(&m, 0) == PN_ERROR && m.state == PN_READY);
    assert(portal_nvenc_lease(&m, 1) == PN_OK);
    assert(portal_nvenc_close(&m) == PN_UNSAFE);
    assert(portal_nvenc_import(&m, 0, 1) == PN_ERROR && m.state == PN_LEASED);
    assert(portal_nvenc_submit(&m, 1) == PN_ERROR);
    assert(portal_nvenc_release(&m, 1) == PN_OK);
    assert(portal_nvenc_lease(&m, 1) == PN_OK);
    assert(portal_nvenc_import(&m, 1, 1) == PN_OK);
    assert(portal_nvenc_submit(&m, 1) == PN_OK);
    assert(portal_nvenc_release(&m, 1) == PN_UNSAFE);
    assert(portal_nvenc_close(&m) == PN_UNSAFE);
    assert(portal_nvenc_wait_gpu(&m, 1) == PN_OK);
    assert(portal_nvenc_release(&m, 1) == PN_OK);
    assert(portal_nvenc_close(&m) == PN_OK);
    assert(portal_nvenc_enable_model(&m) == PN_ERROR);

    /* Every uncertain failure permanently prevents return and clean exit. */
    for (int failure = 0; failure < 4; ++failure) {
        portal_nvenc_init(&m);
        assert(portal_nvenc_enable_model(&m) == PN_OK);
        assert(portal_nvenc_lease(&m, 1) == PN_OK);
        if (failure == 0) {
            assert(portal_nvenc_import(&m, 1, 0) == PN_UNSAFE);
        } else {
            assert(portal_nvenc_import(&m, 1, 1) == PN_OK);
            if (failure == 1) assert(portal_nvenc_submit(&m, 0) == PN_UNSAFE);
            else if (failure == 2) {
                assert(portal_nvenc_submit(&m, 1) == PN_OK);
                assert(portal_nvenc_wait_gpu(&m, 0) == PN_UNSAFE);
            } else assert(portal_nvenc_release(&m, 0) == PN_UNSAFE);
        }
        assert(portal_nvenc_release(&m, 1) == PN_UNSAFE);
        assert(portal_nvenc_close(&m) == PN_UNSAFE);
    }
    return 0;
}
