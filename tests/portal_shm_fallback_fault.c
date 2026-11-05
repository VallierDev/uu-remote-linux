#define _GNU_SOURCE
/* Isolated characterization, no Portal, server, GPU or PipeWire connection:
 * cc -O1 -g -fsanitize=address,undefined -fno-omit-frame-pointer \
 * tests/portal_shm_fallback_fault.c -o /tmp/portal-shm-fault \
 * $(pkg-config --cflags --libs gio-2.0 gio-unix-2.0 libpipewire-0.3 libei-1.0) -pthread -lm
 * /tmp/portal-shm-fault
 * Assertions characterize injected states, not successful fallback or proof
 * that a real PipeWire producer emits the same events in this order. */
#include <assert.h>
#include <errno.h>
#include <pipewire/pipewire.h>
static struct pw_buffer *incoming;
static int update_result, updates, depth;
static void lock_loop(struct pw_thread_loop *p) { (void)p; assert(depth++ == 0); }
static void unlock_loop(struct pw_thread_loop *p) { (void)p; assert(depth-- == 1); }
static struct pw_buffer *dequeue(struct pw_stream *p) { (void)p; struct pw_buffer *b = incoming; incoming = NULL; return b; }
static int queue(struct pw_stream *p, struct pw_buffer *b) { (void)p; assert(b && depth); return 0; }
static int update(struct pw_stream *p, const struct spa_pod **v, uint32_t n) {
    (void)p; (void)v; assert(depth && n == 2); updates++; return update_result;
}
#define pw_thread_loop_lock lock_loop
#define pw_thread_loop_unlock unlock_loop
#define pw_stream_dequeue_buffer dequeue
#define pw_stream_queue_buffer queue
#define pw_stream_update_params update
#include "../c_engine/portal_engine.c"

static struct uu_portal *make_portal(void) {
    struct uu_portal *p = calloc(1, sizeof(*p)); assert(p);
    assert(!pthread_mutex_init(&p->lock, NULL)); assert(!pthread_cond_init(&p->changed, NULL));
    p->pw_loop = (void *)1; p->stream = (void *)1;
    p->width = p->height = 2; p->format = SPA_VIDEO_FORMAT_BGRA; p->format_valid = 1;
    p->frame_length = 16; p->frame = malloc(16); assert(p->frame); memset(p->frame, 0x55, 16);
    p->dma_requested = p->dma_format = 1;
    updates = depth = 0; incoming = NULL;
    return p;
}
static void finish(struct uu_portal *p) {
    assert(!depth); p->pw_loop = NULL; p->stream = NULL; uu_portal_close(p);
}
static void process(struct uu_portal *p, struct pw_buffer *b) {
    incoming = b; lock_loop(p->pw_loop); stream_process(p); unlock_loop(p->pw_loop);
}
int main(void) {
    uint8_t out[16]; uint64_t rev = 0;
    struct uu_portal *p = make_portal();
    /* Initial unsupported DMA frame; SHM request fails synchronously. */
    p->dma_bad = 18; assert(wait_for_first_frame(p) == 14);
    update_result = -EIO; uu_portal_dmabuf_disable(p);
    assert(updates == 1 && !p->dma_requested && !p->error);
    assert(uu_portal_capture_since(p, out, sizeof(out), &rev) == 10);
    assert(updates == 1); /* Failure forgotten; no retry. */
    /* Inject a late EMPTY DMA buffer: current code counts it as a frame even
     * though no SHM arrived. Its real-world reachability is not established. */
    struct spa_chunk chunk = { .flags = SPA_CHUNK_FLAG_EMPTY };
    struct spa_data data = { .type = SPA_DATA_DmaBuf, .fd = 5, .chunk = &chunk };
    struct spa_buffer spa = { .n_datas = 1, .datas = &data };
    struct pw_buffer buffer = { .buffer = &spa };
    process(p, &buffer);
    assert(wait_for_first_frame(p) == 0); /* False startup success. */
    assert(uu_portal_capture_since(p, out, sizeof(out), &rev) == 0 && out[0] == 0x55);
    finish(p);

    /* Runtime: a DMA request can fail while the previous SHM format is still
     * current. Disable does not clear its old sequence or report failure. */
    p = make_portal(); p->dma_format = 0; p->sequence = 1; p->revision = 2;
    update_result = -EIO; rev = 0;
    assert(uu_portal_capture_since(p, out, sizeof(out), &rev) == 0);
    assert(updates == 1 && out[0] == 0x55 && !p->error);
    finish(p);

    /* Accepted request is not completion. The format switch below is injected,
     * not a real format callback or proof of negotiated SHM fallback. */
    p = make_portal(); update_result = 0; rev = 0;
    uu_portal_dmabuf_disable(p);
    assert(uu_portal_capture_since(p, out, sizeof(out), &rev) == 10);
    p->dma_format = 0; /* Simulate a negotiated SHM format callback. */
    uint8_t pixels[16]; memset(pixels, 0x42, sizeof(pixels));
    chunk = (struct spa_chunk){ .size = sizeof(pixels), .stride = 8 };
    data = (struct spa_data){ .type = SPA_DATA_MemPtr, .data = pixels, .maxsize = sizeof(pixels), .chunk = &chunk };
    process(p, &buffer);
    assert(uu_portal_capture_since(p, out, sizeof(out), &rev) == 0 && out[0] == 0x42);
    finish(p);
    puts("PASS: injected-state characterization only; SHM fallback is not verified");
    return 0;
}
