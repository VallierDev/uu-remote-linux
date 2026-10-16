#define _GNU_SOURCE
/* Protocol-independent C bounds/lifecycle checks; never requests consent. */
#include <assert.h>
#include "../c_engine/portal_engine.c"
static void clipboard_bounds(void) {
    int fds[2]; uint8_t bytes[8]={0};
    assert(pipe2(fds,O_NONBLOCK)==0);
    assert(write(fds[1],"abc",3)==3); close(fds[1]);
    assert(clipboard_io(fds[0],bytes,4,0)==3 && !memcmp(bytes,"abc",3)); close(fds[0]);
    assert(pipe2(fds,O_NONBLOCK)==0);
    assert(write(fds[1],"1234",4)==4); close(fds[1]);
    assert(clipboard_io(fds[0],bytes,4,0)==-1); close(fds[0]);
    assert(pipe2(fds,O_NONBLOCK)==0); close(fds[0]);
    assert(clipboard_io(fds[1],bytes,1,1)==-1); close(fds[1]);
    assert(clipboard_io(-1,bytes,0,1)==0);
}
static void capture_with_optional_geometry(int has_size) {
    struct uu_portal *p = calloc(1, sizeof(*p));
    assert(p);
    pthread_mutex_init(&p->lock, NULL);
    pthread_cond_init(&p->changed, NULL);
    p->width = p->height = 2;
    p->absolute_stream_pixels = 1;
    GVariantBuilder properties, streams, result;
    g_variant_builder_init(&properties, G_VARIANT_TYPE_VARDICT);
    if (has_size) g_variant_builder_add(&properties, "{sv}", "size", g_variant_new("(ii)", 1, 1));
    g_variant_builder_init(&streams, G_VARIANT_TYPE("a(ua{sv})"));
    g_variant_builder_add(&streams, "(u@a{sv})", 42u, g_variant_builder_end(&properties));
    g_variant_builder_init(&result, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&result, "{sv}", "devices", g_variant_new_uint32(3));
    g_variant_builder_add(&result, "{sv}", "streams", g_variant_builder_end(&streams));
    GVariant *reply = g_variant_ref_sink(g_variant_builder_end(&result));
    assert(read_stream_result(p, reply) == 0 && p->node == 42);
    g_variant_unref(reply);
    p->frame_length = 16;
    p->frame = calloc(1, p->frame_length);
    assert(p->frame);
    p->sequence = 1;
    uint8_t pixels[16];
    /* Optional compositor metadata must not gate capture/uinput. */
    uu_portal_input_dispatch(p);
    assert(!p->ei && uu_portal_alive(p));
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 0);
    uint32_t width, height;
    uu_portal_logical_size(p, &width, &height);
    assert(width == (has_size ? 1 : 2) && height == width);
    if (!has_size) assert(uu_portal_move(p, 1, 1, 0) == 11);
    assert(!p->ei && uu_portal_alive(p));
    uu_portal_close(p);
}
static void authorization_still_required(void) {
    const unsigned cases[][2] = {{0,1}, {1,1}, {2,1}, {3,0}, {3,2}};
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        GVariantBuilder streams, result;
        g_variant_builder_init(&streams, G_VARIANT_TYPE("a(ua{sv})"));
        for (unsigned n = 0; n < cases[i][1]; n++)
            g_variant_builder_add(&streams, "(u@a{sv})", n + 42, empty_options());
        g_variant_builder_init(&result, G_VARIANT_TYPE_VARDICT);
        g_variant_builder_add(&result, "{sv}", "devices", g_variant_new_uint32(cases[i][0]));
        g_variant_builder_add(&result, "{sv}", "streams", g_variant_builder_end(&streams));
        GVariant *reply = g_variant_ref_sink(g_variant_builder_end(&result));
        struct uu_portal p = {0};
        assert(read_stream_result(&p, reply) == 6);
        g_variant_unref(reply);
    }
}
int main(void) {
    clipboard_bounds();
    capture_with_optional_geometry(0);
    capture_with_optional_geometry(1);
    authorization_still_required();
    struct uu_portal *p = calloc(1, sizeof(*p));
    assert(p);
    pthread_mutex_init(&p->lock, NULL);
    pthread_cond_init(&p->changed, NULL);
    uint8_t storage[256];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *format = spa_format_video_raw_build(&builder, SPA_PARAM_Format,
        &SPA_VIDEO_INFO_RAW_INIT(.format=SPA_VIDEO_FORMAT_BGRA, .size=SPA_RECTANGLE(2,2), .framerate=SPA_FRACTION(60,1)));
    stream_format(p, SPA_PARAM_Format, format);
    assert(p->width == 2 && p->height == 2 && p->frame_length == 16 && !p->error);
    uint8_t pixels[16] = {0};
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 10);
    p->sequence = 1;
    assert(uu_portal_capture(p, pixels, sizeof(pixels)-1) == 11);
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 0);
    p->logical_width = p->logical_height = 2;
    assert(uu_portal_move(p, NAN, 1, 0) == 11);
    assert(uu_portal_move(p, 2, 1, 0) == 11);
    assert(uu_portal_alive(p));
    /* Synthetic RGBA with row padding validates both channel order and stride. */
    uint8_t source[24] = {255,0,0,255, 0,255,0,255, 99,99,99,99,
                         0,0,255,255, 255,255,255,255, 88,88,88,88};
    struct spa_chunk chunk = {.offset=0, .size=20, .stride=12};
    struct spa_data data = {.type=SPA_DATA_MemPtr, .data=source, .maxsize=24, .chunk=&chunk};
    struct spa_buffer buffer = {.n_datas=1, .datas=&data};
    p->format=SPA_VIDEO_FORMAT_RGBA;
    copy_video_buffer(p, &buffer);
    const uint8_t expected[16] = {0,0,255,255, 0,255,0,255, 255,0,0,255, 255,255,255,255};
    assert(!memcmp(p->frame, expected, 16) && p->sequence == 2);
    /* Capacity still contains old bytes, but zero/short chunks are not frames. */
    for (unsigned size=0; size<20; size++) {
        chunk.size=size; copy_video_buffer(p, &buffer);
        assert(p->sequence == 2 && !memcmp(p->frame, expected, 16) && !p->error);
    }
    chunk.size=20; chunk.flags=SPA_CHUNK_FLAG_CORRUPTED;
    copy_video_buffer(p, &buffer);
    assert(p->sequence==2 && !p->error);
    chunk.flags=0;
    struct spa_meta_header header={.flags=SPA_META_HEADER_FLAG_CORRUPTED};
    struct spa_meta metadata={.type=SPA_META_Header, .size=sizeof(header), .data=&header};
    buffer.n_metas=1; buffer.metas=&metadata;
    copy_video_buffer(p, &buffer); assert(p->sequence==2 && !p->error);
    buffer.n_metas=0; buffer.metas=NULL;
    /* Explicit EMPTY is opaque black rather than a stale desktop image. */
    chunk.size=0; chunk.flags=SPA_CHUNK_FLAG_EMPTY;
    copy_video_buffer(p, &buffer); assert(p->sequence==3);
    for (unsigned pixel=0; pixel<4; pixel++) {
        assert(p->frame[pixel*4]==0 && p->frame[pixel*4+1]==0 && p->frame[pixel*4+2]==0 && p->frame[pixel*4+3]==255);
    }
    /* A valid buffer after corruption remains usable; no permanent failure. */
    chunk.size=20; chunk.flags=0; copy_video_buffer(p, &buffer);
    assert(p->sequence==4 && !memcmp(p->frame, expected, 16));
    uu_portal_reset_frame(p);
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 10);
    copy_video_buffer(p, &buffer);
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 0);
    p->session=g_strdup("/uu_test_session");
    GVariantBuilder clip_options; g_variant_builder_init(&clip_options,G_VARIANT_TYPE_VARDICT);
    const char *types[]={"image/png","text/plain",NULL};
    g_variant_builder_add(&clip_options,"{sv}","mime_types",g_variant_new_strv(types,-1));
    GVariant *event=g_variant_ref_sink(g_variant_new("(o@a{sv})",p->session,g_variant_builder_end(&clip_options)));
    selection_owner(NULL,NULL,NULL,NULL,NULL,event,p); g_variant_unref(event);
    assert(p->clipboard_changed && !strcmp(p->clipboard_mime,"text/plain"));
    g_variant_builder_init(&clip_options,G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&clip_options,"{sv}","session_is_owner",g_variant_new_boolean(TRUE));
    event=g_variant_ref_sink(g_variant_new("(o@a{sv})",p->session,g_variant_builder_end(&clip_options)));
    selection_owner(NULL,NULL,NULL,NULL,NULL,event,p); g_variant_unref(event);
    assert(!p->clipboard_changed && !p->clipboard_mime);
    assert(uu_portal_clipboard_set(p,(const uint8_t *)"x",1)==-1);
    assert(uu_portal_clipboard_poll(p,NULL,CLIP_LIMIT)==-1);
    p->error=8;
    assert(!uu_portal_alive(p));
    assert(uu_portal_key(p, 'a', 30, 1)==9); /* no input into an unusable stream */
    p->error=0;
    p->clipboard_enabled=1;
    assert(uu_portal_clipboard_supported(p));
    assert(uu_portal_clipboard_ready(p));
    atomic_store(&p->closed, 1);
    /* Closing a granted session temporarily removes readiness, not capability. */
    assert(uu_portal_clipboard_supported(p));
    assert(!uu_portal_clipboard_ready(p));
    assert(!uu_portal_alive(p));
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 9);
    uu_portal_close(p);
    puts("Portal C bounds/lifecycle checks passed");
    return 0;
}
