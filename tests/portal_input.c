#define _GNU_SOURCE
/* Stub only the D-Bus I/O boundary; no real session bus or consent requests. */
#include <assert.h>
#include <gio/gio.h>
#include <sys/socket.h>
static GVariant *test_call(GDBusConnection *, const gchar *, const gchar *, const gchar *,
    const gchar *, GVariant *, const GVariantType *, GDBusCallFlags, gint, GCancellable *, GError **);
static GVariant *test_connect(GDBusConnection *, const gchar *, const gchar *, const gchar *,
    const gchar *, GVariant *, const GVariantType *, GDBusCallFlags, gint,
    GUnixFDList *, GUnixFDList **, GCancellable *, GError **);
#define g_dbus_connection_call_sync test_call
#define g_dbus_connection_call_with_unix_fd_list_sync test_connect
#include "../c_engine/portal_engine.c"
#undef g_dbus_connection_call_sync
#undef g_dbus_connection_call_with_unix_fd_list_sync

enum response_mode { UNKNOWN_METHOD, NOT_SUPPORTED, TEMPORARY_FAILURE, NO_FDS, BAD_INDEX, DISCONNECTED };
static enum response_mode mode;
static unsigned connects, legacy_calls;
static double last_x, last_y;
static int last_finish;

static GVariant *test_call(GDBusConnection *bus, const gchar *dest, const gchar *path,
    const gchar *interface, const gchar *method, GVariant *parameters,
    const GVariantType *type, GDBusCallFlags flags, gint timeout,
    GCancellable *cancel, GError **error) {
    assert(!strcmp(interface, REMOTE));
    assert(!strcmp(method, "NotifyKeyboardKeysym") || !strcmp(method, "NotifyPointerButton") ||
        !strcmp(method, "NotifyPointerAxisDiscrete") || !strcmp(method, "NotifyPointerMotion") ||
        !strcmp(method, "NotifyPointerMotionAbsolute") || !strcmp(method, "NotifyTouchDown") ||
        !strcmp(method, "NotifyTouchMotion") || !strcmp(method, "NotifyTouchUp") || !strcmp(method, "NotifyPointerAxis"));
    if (!strcmp(method, "NotifyPointerAxis")) {
        assert(g_variant_is_of_type(parameters, G_VARIANT_TYPE("(oa{sv}dd)")));
        GVariant *options = g_variant_get_child_value(parameters, 1);
        GVariant *x = g_variant_get_child_value(parameters, 2), *y = g_variant_get_child_value(parameters, 3);
        last_x = g_variant_get_double(x); last_y = g_variant_get_double(y);
        gboolean finish = FALSE; last_finish = g_variant_lookup(options, "finish", "b", &finish) && finish;
        g_variant_unref(options); g_variant_unref(x); g_variant_unref(y);
    }
    if (!strcmp(method, "NotifyTouchDown") || !strcmp(method, "NotifyTouchMotion")) {
        assert(g_variant_is_of_type(parameters, G_VARIANT_TYPE("(oa{sv}uudd)")));
        GVariant *x = g_variant_get_child_value(parameters, 4);
        GVariant *y = g_variant_get_child_value(parameters, 5);
        last_x = g_variant_get_double(x); last_y = g_variant_get_double(y);
        g_variant_unref(x); g_variant_unref(y);
    }
    if (!strcmp(method, "NotifyTouchUp")) assert(g_variant_is_of_type(parameters, G_VARIANT_TYPE("(oa{sv}u)")));
    if (!strcmp(method, "NotifyPointerMotion") || !strcmp(method, "NotifyPointerMotionAbsolute")) {
        unsigned offset = !strcmp(method, "NotifyPointerMotion") ? 2 : 3;
        GVariant *x = g_variant_get_child_value(parameters, offset);
        GVariant *y = g_variant_get_child_value(parameters, offset + 1);
        last_x = g_variant_get_double(x); last_y = g_variant_get_double(y);
        g_variant_unref(x); g_variant_unref(y);
    }
    legacy_calls++;
    g_variant_ref_sink(parameters); g_variant_unref(parameters);
    return g_variant_ref_sink(g_variant_new("()"));
}

static GVariant *test_connect(GDBusConnection *bus, const gchar *dest, const gchar *path,
    const gchar *interface, const gchar *method, GVariant *parameters,
    const GVariantType *type, GDBusCallFlags flags, gint timeout,
    GUnixFDList *in_fds, GUnixFDList **out_fds, GCancellable *cancel, GError **error) {
    assert(!strcmp(method, "ConnectToEIS") && !strcmp(interface, REMOTE));
    assert(g_variant_is_of_type(parameters, G_VARIANT_TYPE("(oa{sv})")));
    assert(g_variant_type_equal(type, G_VARIANT_TYPE("(h)")));
    connects++;
    g_variant_ref_sink(parameters); g_variant_unref(parameters);
    if (mode <= TEMPORARY_FAILURE) {
        int code = mode == UNKNOWN_METHOD ? G_DBUS_ERROR_UNKNOWN_METHOD :
            mode == NOT_SUPPORTED ? G_DBUS_ERROR_NOT_SUPPORTED : G_DBUS_ERROR_TIMED_OUT;
        g_set_error_literal(error, G_DBUS_ERROR, code, "synthetic EIS response");
        return NULL;
    }
    if (mode != NO_FDS) *out_fds = g_unix_fd_list_new();
    if (mode == DISCONNECTED) {
        int sockets[2];
        assert(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets) == 0);
        close(sockets[1]);
        assert(g_unix_fd_list_append(*out_fds, sockets[0], NULL) == 0);
        close(sockets[0]);
    }
    return g_variant_ref_sink(g_variant_new("(h)", 0));
}

static struct uu_portal *session(int scaled, int gnome, int mapping) {
    struct uu_portal *p = calloc(1, sizeof(*p));
    assert(p);
    pthread_mutex_init(&p->lock, NULL); pthread_cond_init(&p->changed, NULL);
    p->width = 3840; p->height = 2160;
    p->logical_width = scaled ? 1920 : 3840; p->logical_height = scaled ? 1080 : 2160;
    p->absolute_stream_pixels = gnome;
    p->session = g_strdup("/synthetic/session");
    if (mapping) p->mapping_id = g_strdup("synthetic-screen");
    connects = legacy_calls = 0;
    return p;
}

static void legacy_input(struct uu_portal *p) {
    assert(uu_portal_key(p, 'a', 30, 1) == 0);
    assert(uu_portal_key(p, 'a', 30, 0) == 0);
    assert(uu_portal_button(p, 1, 1) == 0);
    assert(uu_portal_button(p, 1, 0) == 0);
    assert(uu_portal_button(p, 4, 1) == 0);
    assert(uu_portal_move(p, 8, 4, 1) == 0);
    assert(last_x == (double)8 * p->logical_width / p->width);
    assert(last_y == (double)4 * p->logical_height / p->height);
    assert(uu_portal_move(p, 1920, 1080, 0) == 0);
    assert(last_x == (p->absolute_stream_pixels ? 1920 : 960));
    assert(last_y == (p->absolute_stream_pixels ? 1080 : 540));
    assert(legacy_calls == 7 && uu_portal_alive(p) && !p->ei);
}

/* Smooth scroll maps like relative motion; only the last event of a gesture carries "finish". */
static void legacy_scroll(struct uu_portal *p) {
    unsigned before = legacy_calls;
    assert(uu_portal_scroll(p, 20, -40, 0) == 0);
    assert(last_x == 20. * p->logical_width / p->width && last_y == -40. * p->logical_height / p->height && !last_finish);
    assert(uu_portal_scroll(p, 0, 0, 1) == 0 && last_finish && last_x == 0 && last_y == 0);
    assert(uu_portal_scroll(p, NAN, 0, 0) == 11 && legacy_calls == before + 2);
}
/* Touch needs the granted touchscreen type and uses the same stream mapping as absolute motion. */
static void legacy_touch(struct uu_portal *p) {
    p->devices = 3;
    assert(uu_portal_touch(p, 0, 10, 10) == 11 && legacy_calls == 7);
    p->devices = 7;
    assert(uu_portal_touch(p, 0, 1920, 1080) == 0);
    assert(last_x == (p->absolute_stream_pixels ? 1920 : 960));
    assert(last_y == (p->absolute_stream_pixels ? 1080 : 540));
    assert(uu_portal_touch(p, 1, 3840, 0) == 11);
    assert(uu_portal_touch(p, 1, 0, 0) == 0 && last_x == 0 && last_y == 0);
    assert(uu_portal_touch(p, 2, 0, 0) == 0);
    assert(legacy_calls == 10 && !p->ei);
}

int main(void) {
    struct uu_portal *p = session(1, 1, 0);
    legacy_input(p);
    assert(!connects && p->ei_unavailable);
    uu_portal_close(p);
    for (int response = UNKNOWN_METHOD; response <= NOT_SUPPORTED; response++) {
        mode = response;
        p = session(1, 1, 1);
        legacy_input(p);
        assert(connects == 1 && p->ei_unavailable);
        uu_portal_close(p);
    }
    p = session(0, 1, 1); /* Unscaled GNOME keeps Notify*. */
    legacy_input(p);
    legacy_touch(p);
    legacy_scroll(p);
    assert(!connects);
    uu_portal_close(p);
    p = session(1, 0, 1); /* KDE keeps its compositor-logical mapping. */
    legacy_input(p);
    legacy_touch(p);
    legacy_scroll(p);
    assert(!connects);
    uu_portal_close(p);

    mode = TEMPORARY_FAILURE;
    p = session(1, 1, 1);
    assert(uu_portal_key(p, 'a', 30, 1) == 9);
    assert(connects == 1 && !legacy_calls && !uu_portal_alive(p) && !p->ei_unavailable);
    assert(uu_portal_key(p, 'a', 30, 0) == 9 && connects == 1);
    uu_portal_close(p);
    mode = UNKNOWN_METHOD; /* A replacement session can select its input backend afresh. */
    p = session(1, 1, 1);
    legacy_input(p);
    assert(connects == 1);
    uu_portal_close(p);

    for (int response = NO_FDS; response <= DISCONNECTED; response++) {
        mode = response;
        p = session(1, 1, 1);
        assert(uu_portal_move(p, 10, 10, 0) == 9);
        assert(!uu_portal_alive(p));
        assert(uu_portal_key(p, 'a', 30, 1) == 9);
        assert(uu_portal_button(p, 1, 1) == 9);
        assert(connects == 1 && !legacy_calls); /* Never mix Notify* after success. */
        uu_portal_close(p);
    }
    puts("Portal input capability/failure checks passed");
    return 0;
}
