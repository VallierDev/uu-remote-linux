/* Hardware-free regression of the real screencopy lease/cancellation code.
 * Build with run-wayland-dmabuf-tests.sh; no desktop or input injection. */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <time.h>
#include <poll.h>
#include <wayland-client.h>
#include "wlr-screencopy-unstable-v1-client-protocol.h"
#include "linux-dmabuf-unstable-v1-client-protocol.h"
static int prepare(struct wl_display *d);
static int dispatch(struct wl_display *d);
static int flush(struct wl_display *d);
static void cancel(struct wl_display *d);
static int display_fd(struct wl_display *d);
static int read_events(struct wl_display *d);
static int test_poll(struct pollfd *fd,nfds_t n,int ms);
static int test_clock(clockid_t id,struct timespec *out);
static void destroy_frame(struct zwlr_screencopy_frame_v1 *f);
static uint32_t frame_version(struct zwlr_screencopy_frame_v1 *f);
static void copy_frame(struct zwlr_screencopy_frame_v1 *f,struct wl_buffer *b);
static void damage_frame(struct zwlr_screencopy_frame_v1 *f,struct wl_buffer *b);
static struct zwlr_screencopy_frame_v1 *capture_output(struct zwlr_screencopy_manager_v1 *m,int32_t cursor,struct wl_output *o);
static int add_listener(struct zwlr_screencopy_frame_v1 *f,const struct zwlr_screencopy_frame_v1_listener *l,void *data);
#define wl_display_prepare_read prepare
#define wl_display_dispatch_pending dispatch
#define wl_display_flush flush
#define wl_display_cancel_read cancel
#define wl_display_get_fd display_fd
#define wl_display_read_events read_events
#define poll test_poll
#define clock_gettime test_clock
#define zwlr_screencopy_frame_v1_destroy destroy_frame
#define zwlr_screencopy_frame_v1_get_version frame_version
#define zwlr_screencopy_frame_v1_copy copy_frame
#define zwlr_screencopy_frame_v1_copy_with_damage damage_frame
#define zwlr_screencopy_manager_v1_capture_output capture_output
#define zwlr_screencopy_frame_v1_add_listener add_listener
#include "../c_engine/wayland_engine.c"
static uint64_t now_ms;
static int fail_flush, map_calls, destroy_calls, cancel_calls, consume_calls;
static int consume_code, copies, damaged_copies, pending_event;
static uint32_t protocol_version=3,test_stride=16,test_offset;
static int test_planes=1;
static uint64_t test_modifier;
static int prepare(struct wl_display *d) { (void)d; return 0; }
static int flush(struct wl_display *d) { (void)d; return fail_flush ? -1:0; }
static void cancel(struct wl_display *d) { (void)d; cancel_calls++; }
static int display_fd(struct wl_display *d) { (void)d; return 0; }
static int read_events(struct wl_display *d) { (void)d; return 0; }
static int dispatch(struct wl_display *d) {
    (void)d;
    if(pending_event) { pending_event=0; copy_ready(&frames[pending],NULL,0,0,0); }
    return 0;
}
static int test_poll(struct pollfd *fd,nfds_t n,int ms) {
    assert(n==1 && ms==0); /* Native capture must never block the input mutex. */ now_ms+=(unsigned)ms;
    fd->revents=pending_event ? POLLIN:0; return pending_event ? 1:0;
}
static int test_clock(clockid_t id,struct timespec *out) {
    (void)id; out->tv_sec=now_ms/1000; out->tv_nsec=(now_ms%1000)*1000000; return 0;
}
static void destroy_frame(struct zwlr_screencopy_frame_v1 *f) { assert(f); destroy_calls++; }
static uint32_t frame_version(struct zwlr_screencopy_frame_v1 *f) { (void)f; return protocol_version; }
static void copy_frame(struct zwlr_screencopy_frame_v1 *f,struct wl_buffer *b) { (void)f; (void)b; copies++; }
static void damage_frame(struct zwlr_screencopy_frame_v1 *f,struct wl_buffer *b) { (void)f; (void)b; damaged_copies++; }
static void ready_frame(struct frame *f,int done) {
    f->handle=(void*)1; f->bo=(void*)2; f->bo_fd=7; f->dmabuf_used=1;
    f->direct_dma=1; f->done=done; f->width=4; f->height=2; f->dma_format=0x34325258;
}
static struct zwlr_screencopy_frame_v1 *capture_output(struct zwlr_screencopy_manager_v1 *m,int32_t cursor,struct wl_output *o) {
    (void)m; (void)cursor; (void)o; ready_frame(&frames[pending],1); return (void*)1;
}
static int add_listener(struct zwlr_screencopy_frame_v1 *f,const struct zwlr_screencopy_frame_v1_listener *l,void *data) {
    (void)f; (void)l; (void)data; return 0;
}
static uint32_t stride(struct gbm_bo *b) { (void)b; return test_stride; }
static uint32_t offset(struct gbm_bo *b,int plane) { (void)b; assert(plane==0); return test_offset; }
static uint64_t modifier(struct gbm_bo *b) { (void)b; return test_modifier; }
static int planes(struct gbm_bo *b) { (void)b; return test_planes; }
static void *map(struct gbm_bo *b,uint32_t x,uint32_t y,uint32_t w,uint32_t h,uint32_t f,uint32_t *s,void **d) {
    (void)b;(void)x;(void)y;(void)w;(void)h;(void)f;(void)s;(void)d;map_calls++;return NULL;
}
static int consume(const UuDmaFrame *f,void *data) {
    assert(data==(void*)9);
    assert(pthread_mutex_trylock(&lock)==EBUSY); /* Lease covers GPU consumption. */
    assert(frames[pending].handle && frames[pending].bo && !frames[pending].pixels);
    assert(f->fd==7 && f->width==4 && f->height==2 && f->stride==test_stride);
    assert(f->offset==test_offset && f->size==test_offset+test_stride*2 && f->modifier==0);
    consume_calls++;return consume_code;
}
static void setup(void) {
    memset(frames,0,sizeof(frames)); pending=0;
    frames[0].shm_fd=frames[1].shm_fd=-1; frames[0].bo_fd=frames[1].bo_fd=-1;
    selected=0;outputs[0].transform=WL_OUTPUT_TRANSFORM_NORMAL;
    initialized=dmabuf_requested=1;dmabuf_disabled=dma_direct_capture=0;
    now_ms=0;fail_flush=map_calls=destroy_calls=cancel_calls=consume_calls=consume_code=0;
    copies=damaged_copies=pending_event=0;test_stride=16;test_offset=0;test_planes=1;test_modifier=0;protocol_version=3;
    gbm.bo_get_stride=stride;gbm.bo_get_offset=offset;gbm.bo_get_modifier=modifier;
    gbm.bo_get_plane_count=planes;gbm.bo_map=map;
}
static void unlocked(void) { assert(pthread_mutex_trylock(&lock)==0);pthread_mutex_unlock(&lock); }
static void lease_success_and_consumer_failure(void) {
    for(int error=0;error<2;error++) {
        setup();ready_frame(&frames[0],1);consume_code=error ? -7:0;test_offset=16;
        assert(uu_wl_capture_dmabuf(consume,(void*)9,0)==consume_code);
        assert(consume_calls==1 && destroy_calls==1 && pending==1 && map_calls==0);
        assert(!frames[0].handle && !frames[0].direct_dma && !frames[0].dmabuf_used);
        unlocked();
    }
}
static void damage_timeout_then_ready_and_cancel(void) {
    setup();ready_frame(&frames[0],0);frames[0].damage_requested=1;
    for(int i=0;i<2;i++) {
        assert(uu_wl_capture_dmabuf(consume,(void*)9,1)==1);
        assert(frames[0].handle && pending==0 && !consume_calls && !destroy_calls);
        unlocked();
    }
    assert(now_ms==0 && cancel_calls==2); /* No damage wait under the input mutex. */
    // Another engine call can dispatch readiness after the direct call timed out.
    assert(!dma_direct_capture);copy_ready(&frames[0],NULL,0,0,0);
    assert(!map_calls && !frames[0].pixels && frames[0].done);
    assert(uu_wl_capture_dmabuf(consume,(void*)9,1)==0 && consume_calls==1);
    setup();ready_frame(&frames[0],0);frames[0].damage_requested=1;
    assert(uu_wl_capture_dmabuf(consume,(void*)9,1)==1);
    assert(uu_wl_capture_dmabuf_cancel()==0 && destroy_calls==1 && !frames[0].handle);
    setup();ready_frame(&frames[0],0);frames[0].damage_requested=1;pending_event=1;
    assert(uu_wl_capture_dmabuf(consume,(void*)9,1)==0 && consume_calls==1);
    assert(!pending_event && !map_calls);
    unlocked();
}
static void restart_and_capture_request_policy(void) {
    setup();ready_frame(&frames[0],0);frames[0].damage_requested=1;
    assert(uu_wl_capture_dmabuf(consume,(void*)9,0)==0);
    assert(destroy_calls==2 && consume_calls==1); // Cancel old wait, then consume new first frame.
    setup();ready_frame(&frames[0],0);frames[0].direct_dma=0;
    assert(uu_wl_capture_dmabuf(consume,(void*)9,0)==0 && destroy_calls==2);
    struct frame f={.handle=(void*)1,.damage_requested=1};
    frame_copy(&f,(void*)3);assert(damaged_copies==1 && !copies);
    protocol_version=1;frame_copy(&f,(void*)3);assert(copies==1);
    protocol_version=3;f.damage_requested=0;frame_copy(&f,(void*)3);assert(copies==2);
}
static void failures_do_not_consume_or_hold_the_lock(void) {
    for(int failure=0;failure<8;failure++) {
        setup();ready_frame(&frames[0],1);
        switch(failure) {
            case 0: initialized=0;break;
            case 1: outputs[0].transform=WL_OUTPUT_TRANSFORM_90;break;
            case 2: frames[0].flags=ZWLR_SCREENCOPY_FRAME_V1_FLAGS_Y_INVERT;break;
            case 3: test_stride=8;break;
            case 4: test_offset=UINT32_MAX;break;
            case 5: test_planes=2;break;
            case 6: test_modifier=1;break;
            case 7: frames[0].done=0;fail_flush=1;break;
        }
        assert(uu_wl_capture_dmabuf(consume,(void*)9,0)<0);
        assert(!consume_calls && !map_calls);unlocked();
    }
}
int main(void) {
    lease_success_and_consumer_failure();damage_timeout_then_ready_and_cancel();
    restart_and_capture_request_policy();failures_do_not_consume_or_hold_the_lock();
    puts("PASS: DMA-BUF leases, idle timeout/resume, restart/cancel and unsupported-layout failures");
    return 0;
}
