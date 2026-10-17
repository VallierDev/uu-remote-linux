#define _GNU_SOURCE
/* Protocol-independent C bounds/lifecycle checks; never requests consent. */
#include <gio/gio.h>
#include <assert.h>
static gint64 test_monotonic_time = 1000000;
static gint64 portal_test_monotonic_time(void) { return test_monotonic_time; }
#define g_get_monotonic_time portal_test_monotonic_time
#include "../c_engine/portal_engine.c"
#undef g_get_monotonic_time

static struct uu_portal *test_portal(void) {
    struct uu_portal *p = calloc(1, sizeof(*p));
    assert(p);
    pthread_mutex_init(&p->lock, NULL);
    pthread_cond_init(&p->changed, NULL);
    return p;
}
static void test_format(struct uu_portal *p, uint32_t width, uint32_t height) {
    uint8_t storage[256];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *format = spa_format_video_raw_build(&builder, SPA_PARAM_Format,
        &SPA_VIDEO_INFO_RAW_INIT(.format=SPA_VIDEO_FORMAT_BGRA, .size=SPA_RECTANGLE(width,height),
            .framerate=SPA_FRACTION(60,1)));
    assert(format);
    stream_format(p, SPA_PARAM_Format, format);
}
static void test_frame(struct uu_portal *p) {
    uint8_t pixels[16] = {1,2,3,255, 4,5,6,255, 7,8,9,255, 10,11,12,255};
    struct spa_chunk chunk = {.offset=0, .size=sizeof(pixels), .stride=8};
    struct spa_data data = {.type=SPA_DATA_MemPtr, .data=pixels, .maxsize=sizeof(pixels), .chunk=&chunk};
    struct spa_buffer buffer = {.n_datas=1, .datas=&data};
    copy_video_buffer(p, &buffer);
}
static void assert_capture_ok(struct uu_portal *p, uint8_t *pixels) {
    uint64_t revision = p->revision + 1;
    assert(uu_portal_capture_since(p, pixels, p->frame_length, &revision) == 0);
}
static void portal_renegotiation_timeout(void) {
    uint8_t pixels[16];
    struct uu_portal *p = test_portal();
    test_monotonic_time = 1000000;
    test_format(p, 2, 2);
    assert(p->format_valid);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    assert(p->sequence == 1 && uu_portal_capture_alive(p));
    assert_capture_ok(p, pixels);

    gint64 paused_at = test_monotonic_time;
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    assert(!p->interrupted_at);
    stream_format(p, SPA_PARAM_Format, NULL);
    assert(!p->format_valid && p->interrupted_at == paused_at && uu_portal_capture_alive(p));
    uint64_t sequence = p->sequence;
    test_frame(p);
    assert(p->sequence == sequence); /* Buffers without a negotiated format are ignored. */

    test_monotonic_time = paused_at + 3 * G_TIME_SPAN_SECOND;
    stream_state(p, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_PAUSED, NULL);
    stream_format(p, SPA_PARAM_Format, NULL);
    test_monotonic_time = paused_at + 4 * G_TIME_SPAN_SECOND;
    stream_format(p, SPA_PARAM_Format, NULL);
    assert(p->interrupted_at == paused_at); /* Repeated NULL formats do not restart the deadline. */
    assert(uu_portal_capture_alive(p));
    assert_capture_ok(p, pixels);

    test_monotonic_time = paused_at + 5 * G_TIME_SPAN_SECOND - 1;
    assert(uu_portal_capture_alive(p));
    assert_capture_ok(p, pixels);
    test_monotonic_time = paused_at + 5 * G_TIME_SPAN_SECOND;
    assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
    uint64_t revision = 0;
    memset(pixels, 99, sizeof(pixels));
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), &revision) == 9);
    assert(!revision && pixels[0] == 99 && pixels[15] == 99);
    /* Late renegotiation can recover naturally before the worker replaces it. */
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    assert(uu_portal_capture_alive(p) && p->revision == 2);
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), &revision) == 0);
    assert(revision == 2 && pixels[0] == 1);
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), &revision) == 12);
    uu_portal_close(p);
}
static void portal_renegotiation_recovery(void) {
    uint8_t pixels[16];
    struct uu_portal *p = test_portal();
    test_monotonic_time = 2000000;
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    gint64 paused_at = test_monotonic_time;
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    stream_format(p, SPA_PARAM_Format, NULL);

    test_monotonic_time = paused_at + 2 * G_TIME_SPAN_SECOND;
    test_format(p, 2, 2);
    assert(p->format_valid && !p->interrupted_at);
    stream_state(p, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_STREAMING, NULL);
    assert(p->interrupted_at == 0 && uu_portal_capture_alive(p));
    test_monotonic_time = paused_at + 6 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p));
    assert_capture_ok(p, pixels);
    uu_portal_close(p);
}
static void portal_renegotiation_event_order_and_healthy_states(void) {
    uint8_t pixels[16];
    struct uu_portal *p = test_portal();
    test_monotonic_time = 3000000;
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    stream_format(p, SPA_PARAM_Format, NULL); /* Format can disappear before the state callback. */
    assert(!p->format_valid && !p->interrupted_at && uu_portal_capture_alive(p));
    test_monotonic_time += 20 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p));
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    gint64 paused_at = test_monotonic_time;
    assert(p->interrupted_at == paused_at);
    test_monotonic_time = paused_at + 5 * G_TIME_SPAN_SECOND;
    assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), NULL) == 9);
    uu_portal_close(p);

    p = test_portal();
    test_monotonic_time = 4000000;
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_PAUSED, NULL);
    assert(!p->interrupted_at);
    test_monotonic_time += 60 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p)); /* A stream that never reached STREAMING has no recovery deadline. */
    assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 10);
    uu_portal_close(p);

    p = test_portal();
    test_monotonic_time = 5000000;
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    uint64_t revision = 0;
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), &revision) == 0);
    test_monotonic_time += 60 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p)); /* A static screen can stream without producing a new frame. */
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), &revision) == 12);
    uu_portal_close(p);

    p = test_portal();
    test_monotonic_time = 6000000;
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    test_monotonic_time += 6 * G_TIME_SPAN_SECOND;
    assert(p->format_valid && uu_portal_capture_alive(p)); /* PAUSED alone is not a format failure. */
    assert_capture_ok(p, pixels);
    assert(!p->interrupted_at);
    /* Healthy paused time must not consume a later negotiation's allowance. */
    stream_format(p, SPA_PARAM_Format, NULL);
    paused_at = test_monotonic_time;
    assert(p->interrupted_at == paused_at && uu_portal_capture_alive(p));
    test_monotonic_time += 4 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p));
    test_format(p, 2, 2);
    assert(!p->interrupted_at && uu_portal_capture_alive(p));
    /* A second format removal while still paused gets a new full deadline. */
    test_monotonic_time += 20 * G_TIME_SPAN_SECOND;
    stream_format(p, SPA_PARAM_Format, NULL);
    paused_at = test_monotonic_time;
    assert(p->interrupted_at == paused_at && uu_portal_capture_alive(p));
    test_monotonic_time += 5 * G_TIME_SPAN_SECOND - 1;
    assert(uu_portal_capture_alive(p));
    test_monotonic_time++;
    assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
    uu_portal_close(p);
}
static void portal_changed_physical_format_still_fails(void) {
    struct uu_portal *p = test_portal();
    uint8_t pixels[24];
    test_monotonic_time = 7000000;
    test_format(p, 2, 2);
    assert(p->format_valid && !p->error);
    test_format(p, 3, 2);
    assert(p->error == 8 && !uu_portal_capture_alive(p));
    assert(uu_portal_capture_since(p, pixels, sizeof(pixels), NULL) == 8);
    uu_portal_close(p);
}
static void display_change(struct uu_portal *p) {
    monitors_changed(NULL, NULL, NULL, NULL, NULL, NULL, p);
}
static struct uu_portal *streaming_portal(void) {
    struct uu_portal *p = test_portal();
    test_format(p, 2, 2);
    stream_state(p, PW_STREAM_STATE_UNCONNECTED, PW_STREAM_STATE_STREAMING, NULL);
    test_frame(p);
    return p;
}
static void portal_display_change_recovery(void) {
    /* Notification, PAUSED and NULL can arrive in any order. */
    const unsigned orders[][3] = {{0,1,2}, {0,2,1}, {1,0,2}, {1,2,0}, {2,0,1}, {2,1,0}};
    for (size_t order = 0; order < G_N_ELEMENTS(orders); order++) {
        test_monotonic_time = 10000000;
        struct uu_portal *p = streaming_portal();
        for (unsigned n = 0; n < 3; n++) {
            test_monotonic_time += 20 * G_TIME_SPAN_MILLISECOND;
            switch (orders[order][n]) {
                case 0: display_change(p); break;
                case 1: stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL); break;
                case 2: stream_format(p, SPA_PARAM_Format, NULL); break;
            }
        }
        gint64 deadline = MAX(p->display_changed_at, p->interrupted_at) + PORTAL_DISPLAY_GRACE;
        test_monotonic_time = deadline - 1;
        assert(uu_portal_capture_alive(p));
        test_monotonic_time++;
        assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
        p->clipboard_enabled = 1;
        assert(uu_portal_clipboard_ready(p));
        uint8_t pixels[16];
        assert(uu_portal_capture(p, pixels, sizeof(pixels)) == 9);
        uu_portal_close(p);
    }
    /* A delayed notification still accelerates an existing invalid pause. */
    test_monotonic_time = 20000000;
    struct uu_portal *p = streaming_portal();
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    stream_format(p, SPA_PARAM_Format, NULL);
    test_monotonic_time += G_TIME_SPAN_SECOND;
    display_change(p);
    gint64 notified_at = p->display_changed_at;
    test_monotonic_time += 100 * G_TIME_SPAN_MILLISECOND;
    display_change(p);
    assert(p->display_changed_at == notified_at); /* Repeated signals cannot postpone recovery. */
    test_monotonic_time = notified_at + PORTAL_DISPLAY_GRACE;
    assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
    uu_portal_close(p);
}
static void portal_display_change_healthy_and_stale(void) {
    test_monotonic_time = 30000000;
    struct uu_portal *p = test_portal();
    display_change(p);
    assert(!p->display_changed_at); /* Ignore notifications during startup. */
    uu_portal_close(p);

    p = streaming_portal();
    display_change(p);
    test_monotonic_time += 60 * G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p)); /* No new frames on a static screen is healthy. */
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    assert(uu_portal_capture_alive(p)); /* A valid format must not trigger recovery. */
    stream_format(p, SPA_PARAM_Format, NULL);
    gint64 invalid_at = test_monotonic_time;
    test_monotonic_time += PORTAL_DISPLAY_GRACE;
    assert(uu_portal_capture_alive(p)); /* The notification from a minute ago is unrelated. */
    test_monotonic_time = invalid_at + PORTAL_RENEGOTIATION_TIMEOUT;
    assert(!uu_portal_capture_alive(p));
    uu_portal_close(p);

    test_monotonic_time = 100000000;
    p = streaming_portal();
    display_change(p);
    stream_state(p, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    stream_format(p, SPA_PARAM_Format, NULL);
    test_monotonic_time += PORTAL_DISPLAY_GRACE - 1;
    test_format(p, 2, 2); /* Natural renegotiation wins before the early check. */
    assert(!p->display_changed_at && !p->interrupted_at);
    test_monotonic_time += G_TIME_SPAN_SECOND;
    assert(uu_portal_capture_alive(p));
    stream_format(p, SPA_PARAM_Format, NULL);
    test_monotonic_time += PORTAL_DISPLAY_GRACE;
    assert(uu_portal_capture_alive(p)); /* Recovered signal cannot accelerate another failure. */
    display_change(p); /* A later display change correlates with the retained invalid state. */
    test_monotonic_time += PORTAL_DISPLAY_GRACE;
    assert(!uu_portal_capture_alive(p) && uu_portal_alive(p));
    /* Even after the fast deadline, recovery can still win before replacement. */
    test_format(p, 2, 2);
    assert(uu_portal_capture_alive(p) && !p->display_changed_at);
    display_change(p);
    test_format(p, 2, 2); /* Successful format without an invalid pause discards its hint. */
    assert(!p->display_changed_at);
    display_change(p);
    stream_state(p, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_STREAMING, NULL);
    assert(!p->display_changed_at);
    uu_portal_close(p);
}
static void portal_display_subscription_lifecycle(void) {
    GTestDBus *test_bus = g_test_dbus_new(G_TEST_DBUS_NONE);
    g_test_dbus_up(test_bus);
    GDBusConnection *bus = g_dbus_connection_new_for_address_sync(g_test_dbus_get_bus_address(test_bus),
        G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT | G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION,
        NULL, NULL, NULL);
    assert(bus);
    const char *sender = g_dbus_connection_get_unique_name(bus);
    for (unsigned threaded = 0; threaded < 2; threaded++) {
        struct uu_portal *p = streaming_portal();
        p->bus = g_object_ref(bus);
        p->context = g_main_context_new();
        p->main_loop = g_main_loop_new(p->context, FALSE);
        g_main_context_push_thread_default(p->context);
        p->display_subscription = g_dbus_connection_signal_subscribe(bus, sender, MUTTER_DISPLAY,
            "MonitorsChanged", MUTTER_DISPLAY_PATH, NULL, G_DBUS_SIGNAL_FLAGS_NONE, monitors_changed, p, NULL);
        g_main_context_pop_thread_default(p->context);
        /* A synchronous roundtrip places the match rule before the signal. */
        GVariant *reply = g_dbus_connection_call_sync(bus, "org.freedesktop.DBus", "/org/freedesktop/DBus",
            "org.freedesktop.DBus", "GetId", NULL, NULL, G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
        assert(reply); g_variant_unref(reply);
        assert(g_dbus_connection_emit_signal(bus, NULL, MUTTER_DISPLAY_PATH, MUTTER_DISPLAY,
            "MonitorsChanged", g_variant_new("()"), NULL));
        assert(g_dbus_connection_flush_sync(bus, NULL, NULL));
        /* The test clock is fixed; use a bounded iteration count for delivery. */
        for (unsigned attempt = 0; attempt < 1000 && !p->display_changed_at; attempt++) {
            while (g_main_context_iteration(p->context, FALSE)) {}
            if (!p->display_changed_at) g_usleep(1000);
        }
        assert(p->display_changed_at);
        if (threaded) {
            assert(!pthread_create(&p->dbus_thread, NULL, run_dbus, p));
            p->dbus_started = 1;
        }
        assert(g_dbus_connection_emit_signal(bus, NULL, MUTTER_DISPLAY_PATH, MUTTER_DISPLAY,
            "MonitorsChanged", g_variant_new("()"), NULL));
        assert(g_dbus_connection_flush_sync(bus, NULL, NULL));
        uu_portal_close(p); /* Queued signals and immediate loop shutdown must be safe. */
    }
    g_dbus_connection_close_sync(bus, NULL, NULL);
    g_object_unref(bus);
    g_test_dbus_down(test_bus);
    g_object_unref(test_bus);
}

static int cancel_immediately(void *data) { return *(const int *)data; }
struct cancel_later { int *cancelled; int polls; };
static int cancel_after_polls(void *data) {
    struct cancel_later *state = data;
    if (state->polls++ >= 2) *state->cancelled = 1;
    return *state->cancelled;
}
static void cancellable_waits(void) {
    int cancelled = 1, error = 0;
    struct uu_portal p = {.cancelled=cancel_immediately, .cancel_data=&cancelled};
    pthread_mutex_init(&p.lock, NULL);
    pthread_cond_init(&p.changed, NULL);
    assert(wait_for_first_frame(&p) == 13);
    pthread_cond_destroy(&p.changed);
    pthread_mutex_destroy(&p.lock);

    GVariantBuilder options;
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    assert(!request(&p, REMOTE, "Start", &options, 2, &error));
    assert(error == 13);
    g_variant_builder_clear(&options);

    /* A cancel arriving during the consent wait must stop it without the full timeout. */
    int waiting_cancelled = 0;
    struct cancel_later state = {.cancelled=&waiting_cancelled};
    struct response response = {0};
    struct uu_portal waiting = {.cancelled=cancel_after_polls, .cancel_data=&state};
    assert(wait_for_response(&waiting, &response, 120) == 13);
    assert(waiting_cancelled);
}
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
    portal_renegotiation_timeout();
    portal_renegotiation_recovery();
    portal_renegotiation_event_order_and_healthy_states();
    portal_changed_physical_format_still_fails();
    portal_display_change_recovery();
    portal_display_change_healthy_and_stale();
    portal_display_subscription_lifecycle();
    cancellable_waits();
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
