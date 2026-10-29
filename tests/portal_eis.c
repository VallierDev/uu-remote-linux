#define _GNU_SOURCE
#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <assert.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <unistd.h>

struct harness;
static struct harness *active_harness;
static void *pump_server_during_open(void *data);
static GVariant *test_call_with_unix_fd_list_sync(GDBusConnection *connection,
    const gchar *bus_name, const gchar *object_path, const gchar *interface_name,
    const gchar *method_name, GVariant *parameters, const GVariantType *reply_type,
    GDBusCallFlags flags, gint timeout_msec, GUnixFDList *fd_list,
    GUnixFDList **out_fd_list, GCancellable *cancellable, GError **error);
#define g_dbus_connection_call_with_unix_fd_list_sync test_call_with_unix_fd_list_sync
#include "../c_engine/portal_engine.c"
#undef g_dbus_connection_call_with_unix_fd_list_sync
#include <libeis.h>

#define CAP_MASK (EI_DEVICE_CAP_POINTER | EI_DEVICE_CAP_POINTER_ABSOLUTE | \
    EI_DEVICE_CAP_BUTTON | EI_DEVICE_CAP_SCROLL | EI_DEVICE_CAP_KEYBOARD)
#define EVENT_CAP_MASK (EIS_DEVICE_CAP_POINTER | EIS_DEVICE_CAP_POINTER_ABSOLUTE | \
    EIS_DEVICE_CAP_BUTTON | EIS_DEVICE_CAP_SCROLL | EIS_DEVICE_CAP_KEYBOARD | EIS_DEVICE_CAP_TOUCH)

static const unsigned capabilities[] = {
    EIS_DEVICE_CAP_POINTER,
    EIS_DEVICE_CAP_POINTER_ABSOLUTE,
    EIS_DEVICE_CAP_BUTTON,
    EIS_DEVICE_CAP_SCROLL,
    EIS_DEVICE_CAP_KEYBOARD,
    EIS_DEVICE_CAP_TOUCH,
};

struct received {
    enum eis_event_type type;
    double x, y;
    uint32_t code;
    int pressed, dx, dy;
};

struct geometry {
    uint32_t width, height, logical_width, logical_height, x, y;
};

struct harness {
    const struct geometry *geometry;
    struct uu_portal *portal;
    struct eis *server;
    struct eis_client *client;
    struct eis_seat *seat;
    struct eis_device *device;
    struct received events[64];
    size_t n_events;
    unsigned frames, starts, stops, diagnostics;
    unsigned connect_calls;
    atomic_int stop_server_pump;
    pthread_t server_pump_thread;
};

static GVariant *test_call_with_unix_fd_list_sync(GDBusConnection *connection,
    const gchar *bus_name, const gchar *object_path, const gchar *interface_name,
    const gchar *method_name, GVariant *parameters, const GVariantType *reply_type,
    GDBusCallFlags flags, gint timeout_msec, GUnixFDList *fd_list,
    GUnixFDList **out_fd_list, GCancellable *cancellable, GError **error) {
    (void)cancellable;
    (void)error;
    assert(active_harness);
    assert(connection == active_harness->portal->bus);
    assert(!strcmp(bus_name, "org.freedesktop.portal.Desktop"));
    assert(!strcmp(object_path, "/org/freedesktop/portal/desktop"));
    assert(!strcmp(interface_name, "org.freedesktop.portal.RemoteDesktop"));
    assert(!strcmp(method_name, "ConnectToEIS"));
    assert(g_variant_is_of_type(parameters, G_VARIANT_TYPE("(oa{sv})")));
    assert(g_variant_type_equal(reply_type, G_VARIANT_TYPE("(h)")));
    assert(flags == G_DBUS_CALL_FLAGS_NONE && timeout_msec == 3000);
    assert(!fd_list && out_fd_list);

    const char *session;
    GVariant *options;
    g_variant_get(parameters, "(&o@a{sv})", &session, &options);
    assert(!strcmp(session, "/synthetic/session"));
    assert(g_variant_n_children(options) == 0);
    g_variant_unref(options);

    int client_fd = eis_backend_fd_add_client(active_harness->server);
    assert(client_fd >= 0);
    GUnixFDList *fds = g_unix_fd_list_new();
    int index = g_unix_fd_list_append(fds, client_fd, NULL);
    close(client_fd);
    assert(index == 0);
    *out_fd_list = fds;
    active_harness->connect_calls++;
    g_variant_ref_sink(parameters); g_variant_unref(parameters);
    /* Hand off the server only after creating the socket; libeis contexts
     * must never be dispatched and mutated by different threads at once. */
    assert(pthread_create(&active_harness->server_pump_thread, NULL,
        pump_server_during_open, active_harness) == 0);
    return g_variant_ref_sink(g_variant_new("(h)", index));
}

static void test_diagnostic(struct ei *ei, enum ei_log_priority priority,
    const char *message, struct ei_log_context *context) {
    (void)ei;
    (void)priority;
    (void)message;
    (void)context;
    if (active_harness) active_harness->diagnostics++;
}

static void add_device(struct harness *h) {
    if (h->device) return;
    h->device = eis_seat_new_device(h->seat);
    assert(h->device);
    eis_device_configure_type(h->device, EIS_DEVICE_TYPE_VIRTUAL);
    eis_device_configure_name(h->device, "synthetic portal EIS device");
    for (size_t i = 0; i < sizeof(capabilities) / sizeof(capabilities[0]); i++)
        eis_device_configure_capability(h->device, capabilities[i]);
    /* The first region belongs to another stream. Matching by order would
     * produce valid EIS events on the wrong screen. */
    struct eis_region *region = eis_device_new_region(h->device);
    assert(region);
    eis_region_set_offset(region, 0, 0);
    eis_region_set_size(region, 100, 100);
    eis_region_set_mapping_id(region, "another-screen");
    eis_region_add(region);
    eis_region_unref(region);
    region = eis_device_new_region(h->device);
    assert(region);
    eis_region_set_offset(region, h->geometry->x, h->geometry->y);
    eis_region_set_size(region, h->geometry->logical_width, h->geometry->logical_height);
    eis_region_set_mapping_id(region, "synthetic-screen");
    eis_region_add(region);
    eis_region_unref(region);
    eis_device_add(h->device);
    eis_device_resume(h->device);
}

static void handle_server_events(struct harness *h) {
    struct eis_event *event;
    while ((event = eis_get_event(h->server))) {
        enum eis_event_type type = eis_event_get_type(event);
        if (type == EIS_EVENT_CLIENT_CONNECT) {
            h->client = eis_client_ref(eis_event_get_client(event));
            eis_client_connect(h->client);
            h->seat = eis_client_new_seat(h->client, "synthetic-seat");
            assert(h->seat);
            for (size_t i = 0; i < sizeof(capabilities) / sizeof(capabilities[0]); i++)
                eis_seat_configure_capability(h->seat, capabilities[i]);
            eis_seat_add(h->seat);
        } else if (type == EIS_EVENT_SEAT_BIND) {
            unsigned bound = 0;
            for (size_t i = 0; i < sizeof(capabilities) / sizeof(capabilities[0]); i++) {
                if (eis_event_seat_has_capability(event, capabilities[i])) bound |= capabilities[i];
            }
            if (!h->device) {
                assert(bound == EVENT_CAP_MASK);
                add_device(h);
            }
        } else if (type == EIS_EVENT_FRAME) {
            h->frames++;
        } else if (type == EIS_EVENT_DEVICE_START_EMULATING) {
            h->starts++;
        } else if (type == EIS_EVENT_DEVICE_STOP_EMULATING) {
            h->stops++;
        } else if (type == EIS_EVENT_POINTER_MOTION || type == EIS_EVENT_POINTER_MOTION_ABSOLUTE ||
                   type == EIS_EVENT_BUTTON_BUTTON || type == EIS_EVENT_SCROLL_DISCRETE ||
                   type == EIS_EVENT_KEYBOARD_KEY || type == EIS_EVENT_TOUCH_DOWN ||
                   type == EIS_EVENT_TOUCH_MOTION || type == EIS_EVENT_TOUCH_UP ||
                   type == EIS_EVENT_SCROLL_DELTA || type == EIS_EVENT_SCROLL_STOP) {
            assert(h->n_events < sizeof(h->events) / sizeof(h->events[0]));
            struct received *got = &h->events[h->n_events++];
            got->type = type;
            switch (type) {
            case EIS_EVENT_POINTER_MOTION:
                got->x = eis_event_pointer_get_dx(event);
                got->y = eis_event_pointer_get_dy(event);
                break;
            case EIS_EVENT_POINTER_MOTION_ABSOLUTE:
                got->x = eis_event_pointer_get_absolute_x(event);
                got->y = eis_event_pointer_get_absolute_y(event);
                break;
            case EIS_EVENT_BUTTON_BUTTON:
                got->code = eis_event_button_get_button(event);
                got->pressed = eis_event_button_get_is_press(event);
                break;
            case EIS_EVENT_SCROLL_DISCRETE:
                got->dx = eis_event_scroll_get_discrete_dx(event);
                got->dy = eis_event_scroll_get_discrete_dy(event);
                break;
            case EIS_EVENT_SCROLL_DELTA:
                got->x = eis_event_scroll_get_dx(event);
                got->y = eis_event_scroll_get_dy(event);
                break;
            case EIS_EVENT_TOUCH_DOWN:
            case EIS_EVENT_TOUCH_MOTION:
                got->x = eis_event_touch_get_x(event);
                got->y = eis_event_touch_get_y(event);
                break;
            case EIS_EVENT_KEYBOARD_KEY:
                got->code = eis_event_keyboard_get_key(event);
                got->pressed = eis_event_keyboard_get_key_is_press(event);
                break;
            default:
                break;
            }
        }
        eis_event_unref(event);
    }
}

static void pump_server(struct harness *h) {
    eis_dispatch(h->server);
    handle_server_events(h);
}

static void pump(struct harness *h) {
    pump_server(h);
    uu_portal_input_dispatch(h->portal);
    pump_server(h);
}

static void *pump_server_during_open(void *data) {
    struct harness *h = data;
    while (!atomic_load(&h->stop_server_pump)) {
        pump_server(h);
        g_usleep(1000);
    }
    return NULL;
}

static int ready(struct harness *h) {
    return h->portal->ei_devices && h->device && h->starts > 0;
}

static int paused(struct harness *h) {
    return !h->portal->ei_devices;
}

static int disconnected(struct harness *h) {
    return atomic_load(&h->portal->ei_failed);
}

static void wait_for(struct harness *h, int (*condition)(struct harness *)) {
    gint64 deadline = g_get_monotonic_time() + 3000000;
    while (!condition(h) && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    assert(condition(h));
}

static void clear_events(struct harness *h) {
    h->n_events = 0;
    h->frames = 0;
}

static void wait_for_input(struct harness *h) {
    gint64 deadline = g_get_monotonic_time() + 1000000;
    while (!h->n_events && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    assert(h->n_events);
    assert(h->frames > 0);
}

static int input_within(struct harness *h, gint64 microseconds) {
    gint64 deadline = g_get_monotonic_time() + microseconds;
    while (!h->n_events && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    return h->n_events > 0;
}

static void pause_resume(struct harness *h) {
    /* libeis 1.5 pauses only a resumed device; pause while emulating is a
     * no-op. End the client transaction before exercising pause/resume. */
    assert(h->portal->ei_devices);
    ei_device_stop_emulating(h->portal->ei_devices->data);
    gint64 deadline = g_get_monotonic_time() + 3000000;
    while (!h->stops && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    assert(h->stops == 1);
    eis_device_pause(h->device);
    deadline = g_get_monotonic_time() + 3000000;
    while (h->portal->ei_devices && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    assert(!h->portal->ei_devices);
    eis_device_resume(h->device);
    deadline = g_get_monotonic_time() + 3000000;
    while ((!h->portal->ei_devices || h->starts < 2) && g_get_monotonic_time() < deadline) {
        pump(h);
        g_usleep(1000);
    }
    assert(h->portal->ei_devices && h->starts == 2 && !h->diagnostics);
}

static void run_geometry(const struct geometry *geometry) {
    struct harness h = {.geometry = geometry};
    h.portal = calloc(1, sizeof(*h.portal));
    assert(h.portal);
    pthread_mutex_init(&h.portal->lock, NULL);
    pthread_cond_init(&h.portal->changed, NULL);
    h.portal->width = geometry->width;
    h.portal->height = geometry->height;
    h.portal->logical_width = geometry->logical_width;
    h.portal->logical_height = geometry->logical_height;
    h.portal->mapping_id = g_strdup("synthetic-screen");
    h.portal->session = strdup("/synthetic/session");
    h.portal->absolute_stream_pixels = 1;
    h.portal->devices = 7;

    h.server = eis_new(NULL);
    assert(h.server && eis_setup_backend_fd(h.server) == 0);
    active_harness = &h;
    int scaled = geometry->width != geometry->logical_width || geometry->height != geometry->logical_height;
    int open_result = scaled ? prepare_input(h.portal) : portal_eis_open(h.portal);
    atomic_store(&h.stop_server_pump, 1);
    assert(pthread_join(h.server_pump_thread, NULL) == 0);
    assert(open_result == 0 && h.connect_calls == 1);
    ei_log_set_priority(h.portal->ei, EI_LOG_PRIORITY_ERROR);
    ei_log_set_handler(h.portal->ei, test_diagnostic);
    wait_for(&h, ready);

    assert(h.portal->ei_devices);
    struct ei_device *device = h.portal->ei_devices->data;
    assert(((ei_device_has_capability(device, EI_DEVICE_CAP_POINTER) ? EI_DEVICE_CAP_POINTER : 0) |
            (ei_device_has_capability(device, EI_DEVICE_CAP_POINTER_ABSOLUTE) ? EI_DEVICE_CAP_POINTER_ABSOLUTE : 0) |
            (ei_device_has_capability(device, EI_DEVICE_CAP_BUTTON) ? EI_DEVICE_CAP_BUTTON : 0) |
            (ei_device_has_capability(device, EI_DEVICE_CAP_SCROLL) ? EI_DEVICE_CAP_SCROLL : 0) |
            (ei_device_has_capability(device, EI_DEVICE_CAP_KEYBOARD) ? EI_DEVICE_CAP_KEYBOARD : 0)) == CAP_MASK);
    struct ei_region *region = ei_device_get_region(device, 1);
    assert(region && !strcmp(ei_region_get_mapping_id(region), "synthetic-screen"));
    assert(ei_region_get_x(region) == geometry->x && ei_region_get_y(region) == geometry->y);
    assert(ei_region_get_width(region) == geometry->logical_width && ei_region_get_height(region) == geometry->logical_height);

    clear_events(&h);
    assert(uu_portal_move(h.portal, 0, 0, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    assert(h.events[0].x == geometry->x && h.events[0].y == geometry->y);

    clear_events(&h);
    assert(uu_portal_move(h.portal, geometry->width / 2.0, geometry->height / 2.0, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    assert(h.events[0].x == geometry->x + geometry->logical_width / 2.0);
    assert(h.events[0].y == geometry->y + geometry->logical_height / 2.0);

    clear_events(&h);
    assert(uu_portal_move(h.portal, geometry->width - 1, geometry->height - 1, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    double right = geometry->x + geometry->logical_width -
        (double)geometry->logical_width / geometry->width;
    double bottom = geometry->y + geometry->logical_height -
        (double)geometry->logical_height / geometry->height;
    assert(fabs(h.events[0].x - right) < 0.001 && fabs(h.events[0].y - bottom) < 0.001);
    clear_events(&h);
    assert(uu_portal_move(h.portal, geometry->width - 1, 0, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    assert(fabs(h.events[0].x - right) < 0.001 && h.events[0].y == geometry->y);
    clear_events(&h);
    assert(uu_portal_move(h.portal, 0, geometry->height - 1, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    assert(h.events[0].x == geometry->x && fabs(h.events[0].y - bottom) < 0.001);
    assert(uu_portal_move(h.portal, geometry->width, geometry->height, 0) == 11);
    assert(uu_portal_move(h.portal, -1, -1, 0) == 11);

    clear_events(&h);
    assert(uu_portal_move(h.portal, geometry->width / 8.0, geometry->height / 8.0, 1) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION);
    assert(h.events[0].x == geometry->logical_width / 8.0);
    assert(h.events[0].y == geometry->logical_height / 8.0);

    clear_events(&h);
    assert(uu_portal_key(h.portal, 'a', 30, 1) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_KEYBOARD_KEY);
    assert(h.events[0].code == 30 && h.events[0].pressed);

    clear_events(&h);
    assert(uu_portal_button(h.portal, 1, 1) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_BUTTON_BUTTON);
    assert(h.events[0].code == 0x110 && h.events[0].pressed);

    clear_events(&h);
    assert(uu_portal_button(h.portal, 4, 1) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_SCROLL_DISCRETE);
    assert(h.events[0].dx == 0 && h.events[0].dy == -120);

    /* Smooth scroll maps like relative motion; finish ends the gesture with a scroll stop. */
    clear_events(&h);
    assert(uu_portal_scroll(h.portal, geometry->width / 8.0, -(double)geometry->height / 8.0, 1) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_SCROLL_DELTA);
    assert(fabs(h.events[0].x - geometry->logical_width / 8.0) < 0.001 && fabs(h.events[0].y + geometry->logical_height / 8.0) < 0.001);
    for (gint64 until = g_get_monotonic_time() + 300000; h.n_events < 2 && g_get_monotonic_time() < until; g_usleep(1000)) pump(&h);
    assert(h.n_events == 2 && h.events[1].type == EIS_EVENT_SCROLL_STOP);

    /* A touch drag lands in the stream's region; motion without a finger down is refused. */
    assert(uu_portal_touch(h.portal, 1, 1, 1) == 11);
    clear_events(&h);
    unsigned diagnostics = h.diagnostics;
    assert(uu_portal_touch(h.portal, 0, geometry->width / 2.0, geometry->height / 2.0) == 0);
    if (!input_within(&h, 300000)) {
        /* libei before 1.3.0 accepts a touch only inside every region of the device (later:
         * any region). This device also has a region for another screen, so an older library
         * rejects the point with a client-bug diagnostic; a real one-stream session has one region. */
        assert(h.diagnostics > diagnostics);
        assert(uu_portal_touch(h.portal, 2, 0, 0) == 0 && !h.portal->ei_touch);
        h.diagnostics = diagnostics; /* expected: the rejected touch and its release */
        goto touched;
    }
    assert(h.events[0].type == EIS_EVENT_TOUCH_DOWN);
    assert(h.events[0].x == geometry->x + geometry->logical_width / 2.0);
    assert(h.events[0].y == geometry->y + geometry->logical_height / 2.0);
    clear_events(&h);
    assert(uu_portal_touch(h.portal, 1, 0, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_TOUCH_MOTION);
    assert(h.events[0].x == geometry->x && h.events[0].y == geometry->y);
    assert(uu_portal_touch(h.portal, 1, geometry->width, 0) == 11);
    clear_events(&h);
    assert(uu_portal_touch(h.portal, 2, 0, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_TOUCH_UP && !h.portal->ei_touch);
    assert(uu_portal_touch(h.portal, 2, 0, 0) == 0);
touched:;

    uint8_t storage[256];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *format = spa_format_video_raw_build(&builder, SPA_PARAM_Format,
        &SPA_VIDEO_INFO_RAW_INIT(.format=SPA_VIDEO_FORMAT_BGRA,
            .size=SPA_RECTANGLE(geometry->width, geometry->height), .framerate=SPA_FRACTION(60,1)));
    assert(format);
    stream_format(h.portal, SPA_PARAM_Format, format);
    stream_state(h.portal, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_STREAMING, NULL);
    stream_state(h.portal, PW_STREAM_STATE_STREAMING, PW_STREAM_STATE_PAUSED, NULL);
    stream_format(h.portal, SPA_PARAM_Format, NULL);
    /* Expire only video recovery; keep EIS and the harness wall clock live. */
    pthread_mutex_lock(&h.portal->lock);
    h.portal->interrupted_at = g_get_monotonic_time() - PORTAL_RENEGOTIATION_TIMEOUT - 1;
    pthread_mutex_unlock(&h.portal->lock);
    assert(!uu_portal_capture_alive(h.portal));
    uint8_t pixel = 99;
    assert(uu_portal_capture_since(h.portal, &pixel, sizeof(pixel), NULL) == 9 && pixel == 99);

    clear_events(&h);
    assert(uu_portal_key(h.portal, 'a', 30, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_KEYBOARD_KEY);
    assert(h.events[0].code == 30 && !h.events[0].pressed);

    clear_events(&h);
    assert(uu_portal_button(h.portal, 1, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_BUTTON_BUTTON);
    assert(h.events[0].code == 0x110 && !h.events[0].pressed);

    stream_format(h.portal, SPA_PARAM_Format, format);
    stream_state(h.portal, PW_STREAM_STATE_PAUSED, PW_STREAM_STATE_STREAMING, NULL);
    assert(uu_portal_capture_alive(h.portal) && !h.portal->interrupted_at);
    /* Natural video recovery keeps this EIS device, with neither input held. */
    assert(h.portal->ei_devices->data == device && h.connect_calls == 1 && h.starts == 1);
    for (int pressed = 1; pressed >= 0; pressed--) {
        clear_events(&h);
        assert(uu_portal_key(h.portal, 'a', 30, pressed) == 0);
        wait_for_input(&h);
        assert(h.events[0].type == EIS_EVENT_KEYBOARD_KEY);
        assert(h.events[0].code == 30 && h.events[0].pressed == pressed);
        clear_events(&h);
        assert(uu_portal_button(h.portal, 1, pressed) == 0);
        wait_for_input(&h);
        assert(h.events[0].type == EIS_EVENT_BUTTON_BUTTON);
        assert(h.events[0].code == 0x110 && h.events[0].pressed == pressed);
    }

    pause_resume(&h);

    clear_events(&h);
    assert(uu_portal_move(h.portal, geometry->width - 1, geometry->height - 1, 0) == 0);
    wait_for_input(&h);
    assert(h.events[0].type == EIS_EVENT_POINTER_MOTION_ABSOLUTE);
    assert(fabs(h.events[0].x - right) < 0.001 && fabs(h.events[0].y - bottom) < 0.001);

    eis_device_remove(h.device);
    wait_for(&h, paused);
    clear_events(&h);
    assert(uu_portal_move(h.portal, 100, 100, 0) == 9);
    assert(uu_portal_key(h.portal, 'a', 30, 1) == 9);
    assert(uu_portal_button(h.portal, 1, 1) == 9);
    pump(&h);
    assert(h.n_events == 0);

    eis_client_disconnect(h.client);
    wait_for(&h, disconnected);
    assert(!uu_portal_alive(h.portal));
    clear_events(&h);
    assert(uu_portal_move(h.portal, 100, 100, 0) == 9);
    assert(uu_portal_key(h.portal, 'a', 30, 1) == 9);
    assert(uu_portal_button(h.portal, 1, 1) == 9);
    pump(&h);
    assert(h.n_events == 0);

    assert(!h.diagnostics);
    uu_portal_close(h.portal);
    active_harness = NULL;
    if (h.device) eis_device_unref(h.device);
    if (h.seat) eis_seat_unref(h.seat);
    if (h.client) eis_client_unref(h.client);
    eis_unref(h.server);
}

int main(void) {
    const struct geometry cases[] = {
        {1920, 1080, 1920, 1080, 0, 0},       /* 100% */
        {2560, 1440, 2048, 1152, 320, 90},    /* 125% */
        {2560, 1440, 1707, 960, 0, 0},        /* 150%, rounded logical size */
        {3840, 2160, 2560, 1440, 100, 200},   /* 150% */
        {3840, 2160, 2194, 1234, 0, 0},       /* 175%, rounded logical size */
        {3840, 2160, 1920, 1080, 320, 90},    /* 200% */
        {3840, 2160, 2304, 1296, 320, 90},    /* replacement at 166.67% */
        {3840, 2160, 1920, 1080, 320, 90},    /* replacement back at 200% */
        {5120, 2880, 2560, 1440, 0, 0},       /* 5K */
        {7680, 4320, 3840, 2160, 7680, 0},    /* 8K, nonzero origin */
        {3440, 1440, 2293, 960, 0, 1080},     /* ultrawide */
        {2160, 3840, 1080, 1920, 1920, 0},    /* portrait */
        {2257, 1505, 1806, 1204, 0, 0},       /* odd frame dimensions */
    };
    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) run_geometry(&cases[i]);
    printf("Portal EIS integration checks passed: %zu geometries\n", sizeof(cases) / sizeof(cases[0]));
    return 0;
}
