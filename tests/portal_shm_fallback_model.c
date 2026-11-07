/* Standalone, no production linkage:
 * cc -std=c11 -Wall -Wextra -Werror -O1 -g -fsanitize=address,undefined \
 *   -fno-omit-frame-pointer tests/portal_shm_fallback_model.c \
 *   c_engine/portal_shm_fallback_model.c -o /tmp/shm-model && /tmp/shm-model
 */
#include "../c_engine/portal_shm_fallback_model.h"
#include <assert.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
    struct shm_model m;
    shm_init(&m);
    assert(!shm_request(&m, 0));
    assert(shm_request(&m, 1));
    assert(!shm_request(&m, 2));
    /* Reentrant callbacks cannot establish completion. */
    shm_format_event(&m, 1, 1, SHM_FORMAT_SHM);
    shm_buffer_event(&m, 1, 1, SHM_PIXELS_VALID);
    assert(m.state == SHM_REQUESTING);
    shm_request_result(&m, 1, 0);
    shm_request_result(&m, 1, 1);
    assert(m.state == SHM_FAILED && !shm_request(&m, 1));
    assert(shm_request(&m, 2));
    shm_request_result(&m, 1, 1);
    assert(m.state == SHM_REQUESTING);
    shm_request_result(&m, 2, 1);
    shm_buffer_event(&m, 2, 1, SHM_PIXELS_VALID);
    shm_format_event(&m, 1, 1, SHM_FORMAT_SHM);
    assert(m.state == SHM_WAIT_FORMAT);
    shm_format_event(&m, 2, 1, SHM_FORMAT_SHM);
    shm_buffer_event(&m, 0, 1, SHM_PIXELS_VALID);
    shm_buffer_event(&m, 2, 0, SHM_PIXELS_VALID);
    shm_buffer_event(&m, 1, 1, SHM_PIXELS_VALID);
    shm_buffer_event(&m, 2, 1, SHM_DMA);
    shm_buffer_event(&m, 2, 1, SHM_EMPTY);
    shm_buffer_event(&m, 2, 1, SHM_INVALID);
    assert(m.state == SHM_WAIT_PIXELS);
    shm_buffer_event(&m, 2, 1, SHM_PIXELS_VALID);
    assert(m.state == SHM_READY);
    shm_buffer_event(&m, 0, 1, SHM_PIXELS_VALID);
    assert(m.state == SHM_WAIT_PIXELS);
    shm_buffer_event(&m, 2, 1, SHM_PIXELS_VALID);
    assert(m.state == SHM_READY);
    /* A later format invalidates pixels of the earlier format in this epoch. */
    shm_format_event(&m, 2, 2, SHM_FORMAT_SHM);
    shm_buffer_event(&m, 2, 1, SHM_PIXELS_VALID);
    assert(m.state == SHM_WAIT_PIXELS);
    shm_buffer_event(&m, 2, 2, SHM_PIXELS_VALID);
    assert(m.state == SHM_READY);
    /* Replaying either current or older generation fails rather than reviving it. */
    shm_format_event(&m, 2, 2, SHM_FORMAT_SHM);
    shm_buffer_event(&m, 2, 2, SHM_PIXELS_VALID);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 3));
    shm_request_result(&m, 3, 1);
    shm_format_event(&m, 3, 1, SHM_FORMAT_SHM);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 4));
    shm_request_result(&m, 4, 1);
    shm_format_event(&m, 4, 0, SHM_FORMAT_SHM);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 5));
    shm_request_result(&m, 5, 1);
    shm_format_event(&m, 0, 3, SHM_FORMAT_SHM);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 6));
    shm_request_result(&m, 6, 1);
    shm_format_event(&m, 6, 3, SHM_FORMAT_ERROR);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 7));
    shm_request_result(&m, 7, 1);
    shm_format_event(&m, 7, 3, SHM_FORMAT_OTHER);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 8));
    shm_timeout(&m, 8);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 9));
    shm_request_result(&m, 9, 1);
    shm_timeout(&m, 9);
    assert(m.state == SHM_FAILED);
    assert(shm_request(&m, 10));
    shm_request_result(&m, 10, 1);
    shm_format_event(&m, 10, 3, SHM_FORMAT_SHM);
    shm_timeout(&m, 9);
    assert(m.state == SHM_WAIT_PIXELS);
    shm_timeout(&m, 10);
    shm_buffer_event(&m, 10, 3, SHM_PIXELS_VALID);
    assert(m.state == SHM_FAILED);
    /* Neither counter may wrap; the high-water mark survives a new epoch. */
    assert(shm_request(&m, UINT64_MAX));
    shm_request_result(&m, UINT64_MAX, 1);
    shm_format_event(&m, UINT64_MAX, UINT64_MAX, SHM_FORMAT_SHM);
    shm_buffer_event(&m, UINT64_MAX, UINT64_MAX, SHM_PIXELS_VALID);
    assert(m.state == SHM_READY);
    shm_format_event(&m, UINT64_MAX, 1, SHM_FORMAT_SHM);
    assert(m.state == SHM_FAILED && !shm_request(&m, 1));
    /* Generation exhaustion also persists independently across epochs. */
    struct shm_model n;
    shm_init(&n);
    assert(shm_request(&n, 1));
    shm_request_result(&n, 1, 1);
    shm_format_event(&n, 1, UINT64_MAX, SHM_FORMAT_SHM);
    shm_timeout(&n, 1);
    assert(shm_request(&n, 2));
    shm_request_result(&n, 2, 1);
    shm_format_event(&n, 2, 1, SHM_FORMAT_SHM);
    assert(n.state == SHM_FAILED);
    shm_close(&m);
    shm_request_result(&m, UINT64_MAX, 1);
    shm_format_event(&m, UINT64_MAX, UINT64_MAX, SHM_FORMAT_SHM);
    shm_buffer_event(&m, UINT64_MAX, UINT64_MAX, SHM_PIXELS_VALID);
    shm_timeout(&m, UINT64_MAX);
    assert(!shm_request(&m, UINT64_MAX) && m.state == SHM_CLOSED);
    puts("PASS: isolated SHM model (identities supplied by caller)");
}
