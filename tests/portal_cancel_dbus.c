#define _GNU_SOURCE
/* Private-bus regression: cancellation while Properties.Get is in flight.
 * Build: cc -g -O1 -fsanitize=address,undefined -fno-omit-frame-pointer
 *   tests/portal_cancel_dbus.c -o /tmp/portal-cancel-dbus
 *   $(pkg-config --cflags --libs gio-2.0 gio-unix-2.0 libpipewire-0.3 libei-1.0) -lm -pthread
 * Run with dbus-daemon on PATH. The test fails unless cancellation interrupts
 * the delayed call; it also checks normal and late replies. */
#include "../c_engine/portal_engine.c"
#include <assert.h>

struct fixture {
    GMutex lock;
    GCond changed;
    gboolean entered, release, done;
    int session_test, delay_version, remote_versions, sessions_created, sessions_closed, select_calls;
    GDBusMethodInvocation *pending;
    atomic_int cancelled;
    pthread_t callback_thread;
    int callback_calls, error;
};
static const char xml[] =
    "<node><interface name='org.freedesktop.DBus.Properties'>"
    "<method name='Get'><arg type='s' direction='in'/><arg type='s' direction='in'/>"
    "<arg type='v' direction='out'/></method></interface>"
    "<interface name='org.freedesktop.portal.RemoteDesktop'>"
    "<method name='CreateSession'><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
    "<method name='SelectDevices'><arg type='o' direction='in'/><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
    "</interface><interface name='org.freedesktop.portal.Session'><method name='Close'/></interface></node>";
#define TEST_SESSION "/org/freedesktop/portal/desktop/session/test/session"
static void method_call(GDBusConnection *bus, const char *sender, const char *path,
    const char *interface, const char *method, GVariant *parameters,
    GDBusMethodInvocation *invocation, gpointer data) {
    (void)path;
    struct fixture *f = data;
    if (!strcmp(interface, REMOTE)) {
        if (!strcmp(method, "SelectDevices")) {
            f->select_calls++;
            g_dbus_method_invocation_return_dbus_error(invocation, "org.test.Unexpected", "unexpected");
            return;
        }
        GVariant *options; const char *token;
        g_variant_get(parameters, "(@a{sv})", &options);
        assert(g_variant_lookup(options, "handle_token", "&s", &token));
        char *name = g_strdup(sender + 1);
        for (char *c = name; *c; c++) if (*c == '.') *c = '_';
        char *request_path = g_strdup_printf("/org/freedesktop/portal/desktop/request/%s/%s", name, token);
        GVariantBuilder results; g_variant_builder_init(&results, G_VARIANT_TYPE_VARDICT);
        g_variant_builder_add(&results, "{sv}", "session_handle", g_variant_new_string(TEST_SESSION));
        f->sessions_created++;
        assert(g_dbus_connection_emit_signal(bus, sender, request_path, REQUEST, "Response",
            g_variant_new("(u@a{sv})", 0u, g_variant_builder_end(&results)), NULL));
        g_dbus_method_invocation_return_value(invocation, g_variant_new("(o)", request_path));
        g_free(request_path); g_free(name); g_variant_unref(options);
        return;
    }
    if (!strcmp(interface, SESSION)) {
        f->sessions_closed++;
        g_dbus_method_invocation_return_value(invocation, g_variant_new("()"));
        return;
    }
    if (f->session_test) {
        const char *target, *property;
        g_variant_get(parameters, "(&s&s)", &target, &property);
        int remote_version = !strcmp(target, REMOTE) && !strcmp(property, "version");
        if (remote_version) f->remote_versions++;
        int delay = (!f->delay_version && !strcmp(property, "AvailableDeviceTypes")) ||
            (f->delay_version && remote_version && f->remote_versions == 2);
        if (delay) {
            g_mutex_lock(&f->lock);
            f->pending = g_object_ref(invocation);
            f->entered = TRUE;
            g_cond_broadcast(&f->changed);
            g_mutex_unlock(&f->lock);
            return;
        }
        g_dbus_method_invocation_return_value(invocation, g_variant_new("(v)", g_variant_new_uint32(2)));
        return;
    }
    g_mutex_lock(&f->lock);
    f->entered = TRUE;
    g_cond_broadcast(&f->changed);
    while (!f->release) g_cond_wait(&f->changed, &f->lock);
    g_mutex_unlock(&f->lock);
    g_dbus_method_invocation_return_value(invocation, g_variant_new("(v)", g_variant_new_uint32(1)));
}
static const GDBusInterfaceVTable vtable = { .method_call = method_call };
static int cancelled(void *data) {
    struct fixture *f = data;
    assert(pthread_equal(pthread_self(), f->callback_thread));
    f->callback_calls++;
    return atomic_load(&f->cancelled);
}
static void *open_thread(void *data) {
    struct fixture *f = data;
    f->callback_thread = pthread_self();
    uint32_t width, height;
    struct uu_portal *p = uu_portal_open_persist_cancellable(NULL, NULL, NULL,
        cancelled, f, &f->error, &width, &height);
    if (p) uu_portal_close(p);
    g_mutex_lock(&f->lock);
    f->done = TRUE;
    g_cond_broadcast(&f->changed);
    g_mutex_unlock(&f->lock);
    return NULL;
}
static gpointer loop_thread(gpointer data) { g_main_loop_run(data); return NULL; }
int main(void) {
    struct fixture f = {0};
    g_mutex_init(&f.lock); g_cond_init(&f.changed);
    GTestDBus *test_bus = g_test_dbus_new(G_TEST_DBUS_NONE);
    g_test_dbus_up(test_bus);
    GError *error = NULL;
    GDBusConnection *service = g_dbus_connection_new_for_address_sync(g_test_dbus_get_bus_address(test_bus),
        G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT | G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION,
        NULL, NULL, &error);
    assert(service && !error);
    GVariant *reply = g_dbus_connection_call_sync(service, "org.freedesktop.DBus", "/org/freedesktop/DBus",
        "org.freedesktop.DBus", "RequestName", g_variant_new("(su)", DEST, 0u),
        G_VARIANT_TYPE("(u)"), G_DBUS_CALL_FLAGS_NONE, 3000, NULL, &error);
    assert(reply && !error); guint result; g_variant_get(reply, "(u)", &result); g_variant_unref(reply);
    assert(result == 1);
    GMainContext *context = g_main_context_new();
    GMainLoop *loop = g_main_loop_new(context, FALSE);
    GDBusNodeInfo *info = g_dbus_node_info_new_for_xml(xml, &error);
    assert(info && !error);
    g_main_context_push_thread_default(context);
    guint registration = g_dbus_connection_register_object(service, PATH, info->interfaces[0], &vtable, &f, NULL, &error);
    guint remote_registration = g_dbus_connection_register_object(service, PATH, info->interfaces[1], &vtable, &f, NULL, &error);
    guint session_registration = g_dbus_connection_register_object(service, TEST_SESSION, info->interfaces[2], &vtable, &f, NULL, &error);
    g_main_context_pop_thread_default(context);
    assert(registration && remote_registration && session_registration && !error);
    GThread *server = g_thread_new("fake-portal", loop_thread, loop);
    pthread_t client; assert(!pthread_create(&client, NULL, open_thread, &f));
    g_mutex_lock(&f.lock);
    gint64 limit = g_get_monotonic_time() + 5 * G_TIME_SPAN_SECOND;
    while (!f.entered && g_cond_wait_until(&f.changed, &f.lock, limit)) {}
    assert(f.entered);
    atomic_store(&f.cancelled, 1);
    limit = g_get_monotonic_time() + 400 * G_TIME_SPAN_MILLISECOND;
    while (!f.done && g_cond_wait_until(&f.changed, &f.lock, limit)) {}
    gboolean prompt = f.done;
    f.release = TRUE; g_cond_broadcast(&f.changed);
    g_mutex_unlock(&f.lock);
    pthread_join(client, NULL);
    assert(f.error == 13 && f.callback_calls > 0);
    /* The delayed response arrives only after the client completed and its
     * private context and callback state were destroyed. */
    f.callback_thread = pthread_self();
    atomic_store(&f.cancelled, 0);
    struct uu_portal p = {.bus=service, .cancelled=cancelled, .cancel_data=&f};
    GVariant *value = get_property(&p, REMOTE, "version", 3000);
    assert(value); g_variant_unref(value);
    p.cancelled = NULL;
    value = get_property(&p, REMOTE, "version", 3000);
    assert(value); g_variant_unref(value);
    assert(g_dbus_connection_flush_sync(service, NULL, NULL));
    g_usleep(50000);
    /* Cancel at each of the two property reads after a session was created.
     * The fake service keeps the reply pending but can still process Close. */
    for (int delay_version = 0; delay_version <= 1; delay_version++) {
        g_mutex_lock(&f.lock);
        f.session_test = 1; f.delay_version = delay_version; f.remote_versions = 0;
        f.sessions_created = f.sessions_closed = f.select_calls = 0;
        f.entered = f.done = FALSE; f.pending = NULL;
        atomic_store(&f.cancelled, 0);
        g_mutex_unlock(&f.lock);
        assert(!pthread_create(&client, NULL, open_thread, &f));
        g_mutex_lock(&f.lock);
        limit = g_get_monotonic_time() + 5 * G_TIME_SPAN_SECOND;
        while (!f.entered && g_cond_wait_until(&f.changed, &f.lock, limit)) {}
        assert(f.entered && f.pending);
        atomic_store(&f.cancelled, 1);
        limit = g_get_monotonic_time() + 2 * G_TIME_SPAN_SECOND;
        while (!f.done && g_cond_wait_until(&f.changed, &f.lock, limit)) {}
        assert(f.done);
        GDBusMethodInvocation *pending = f.pending; f.pending = NULL;
        g_mutex_unlock(&f.lock);
        pthread_join(client, NULL);
        assert(f.error == 13 && f.sessions_created == 1 && f.sessions_closed == 1 && !f.select_calls);
        g_dbus_method_invocation_return_value(pending, g_variant_new("(v)", g_variant_new_uint32(2)));
        g_object_unref(pending);
        assert(g_dbus_connection_flush_sync(service, NULL, NULL));
        g_usleep(50000);
    }
    g_main_loop_quit(loop); g_thread_join(server);
    g_dbus_connection_unregister_object(service, session_registration);
    g_dbus_connection_unregister_object(service, remote_registration);
    g_dbus_connection_unregister_object(service, registration);
    g_dbus_node_info_unref(info); g_main_loop_unref(loop); g_main_context_unref(context);
    g_dbus_connection_close_sync(service, NULL, NULL); g_object_unref(service);
    g_test_dbus_down(test_bus); g_object_unref(test_bus);
    g_cond_clear(&f.changed); g_mutex_clear(&f.lock);
    if (!prompt) { fprintf(stderr, "FAIL: cancellation did not interrupt delayed D-Bus Get within 400ms.\n"); return 1; }
    puts("Delayed D-Bus cancellation passed."); return 0;
}
