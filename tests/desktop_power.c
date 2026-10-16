#define _GNU_SOURCE
#include <assert.h>
#include <gio/gio.h>
#include <string.h>
#include "../c_engine/desktop_power.c"

static const char *introspection_xml =
    "<node>"
    " <interface name='org.freedesktop.DBus.Properties'>"
    "  <method name='Set'>"
    "   <arg type='s' direction='in'/><arg type='s' direction='in'/>"
    "   <arg type='v' direction='in'/>"
    "  </method>"
    " </interface>"
    " <interface name='org.gnome.SessionManager'>"
    "  <method name='Inhibit'>"
    "   <arg type='s' direction='in'/><arg type='u' direction='in'/>"
    "   <arg type='s' direction='in'/><arg type='u' direction='in'/>"
    "   <arg type='u' direction='out'/>"
    "  </method>"
    "  <method name='Uninhibit'>"
    "   <arg type='u' direction='in'/>"
    "  </method>"
    " </interface>"
    "</node>";

static GMutex state_lock;
static GCond ready_cond;
static gboolean service_ready;
static gboolean fail_uninhibit;
static unsigned wake_calls, inhibit_calls, uninhibit_calls;
static guint32 last_flags, last_cookie;
static char *wake_sender, *inhibit_sender, *uninhibit_sender;
static GDBusConnection *service_connection;
static GMainLoop *service_loop;

static void replace_string(char **target, const char *value) {
    g_free(*target);
    *target = g_strdup(value);
}

static void method_call(GDBusConnection *connection, const gchar *sender, const gchar *path,
    const gchar *interface, const gchar *method, GVariant *parameters,
    GDBusMethodInvocation *invocation, gpointer data) {
    (void)connection; (void)path; (void)data;
    g_mutex_lock(&state_lock);
    if (!strcmp(interface, "org.freedesktop.DBus.Properties") && !strcmp(method, "Set")) {
        const char *target_interface, *property;
        GVariant *boxed, *value;
        g_variant_get(parameters, "(&s&s@v)", &target_interface, &property, &boxed);
        value = g_variant_get_variant(boxed);
        assert(!strcmp(target_interface, "org.gnome.Mutter.DisplayConfig"));
        assert(!strcmp(property, "PowerSaveMode"));
        assert(g_variant_is_of_type(value, G_VARIANT_TYPE_INT32));
        assert(g_variant_get_int32(value) == 0);
        wake_calls++;
        replace_string(&wake_sender, sender);
        g_variant_unref(value);
        g_variant_unref(boxed);
        g_mutex_unlock(&state_lock);
        g_dbus_method_invocation_return_value(invocation, g_variant_new("()"));
        return;
    }
    if (!strcmp(interface, "org.gnome.SessionManager") && !strcmp(method, "Inhibit")) {
        const char *app, *reason;
        guint32 xid, flags;
        g_variant_get(parameters, "(&su&su)", &app, &xid, &reason, &flags);
        assert(!strcmp(app, "uu-remote-linux"));
        assert(xid == 0);
        assert(!strcmp(reason, "Remote desktop session is active"));
        inhibit_calls++;
        last_flags = flags;
        replace_string(&inhibit_sender, sender);
        g_mutex_unlock(&state_lock);
        g_dbus_method_invocation_return_value(invocation, g_variant_new("(u)", 73u));
        return;
    }
    if (!strcmp(interface, "org.gnome.SessionManager") && !strcmp(method, "Uninhibit")) {
        guint32 cookie;
        g_variant_get(parameters, "(u)", &cookie);
        uninhibit_calls++;
        last_cookie = cookie;
        replace_string(&uninhibit_sender, sender);
        gboolean fail = fail_uninhibit;
        g_mutex_unlock(&state_lock);
        if (fail) g_dbus_method_invocation_return_dbus_error(invocation,
            "org.freedesktop.DBus.Error.Failed", "synthetic failure");
        else g_dbus_method_invocation_return_value(invocation, g_variant_new("()"));
        return;
    }
    g_mutex_unlock(&state_lock);
    g_dbus_method_invocation_return_dbus_error(invocation,
        "org.freedesktop.DBus.Error.UnknownMethod", "unexpected method");
}

static const GDBusInterfaceVTable vtable = { .method_call = method_call };

static GDBusConnection *connect_bus(const char *address) {
    GError *error = NULL;
    GDBusConnection *connection = g_dbus_connection_new_for_address_sync(address,
        G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT |
            G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION,
        NULL, NULL, &error);
    assert(connection && !error);
    g_dbus_connection_set_exit_on_close(connection, FALSE);
    return connection;
}

static void request_name(GDBusConnection *connection, const char *name) {
    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(connection, "org.freedesktop.DBus",
        "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName",
        g_variant_new("(su)", name, 4u), G_VARIANT_TYPE("(u)"),
        G_DBUS_CALL_FLAGS_NONE, 3000, NULL, &error);
    assert(reply && !error);
    guint32 result;
    g_variant_get(reply, "(u)", &result);
    assert(result == 1);
    g_variant_unref(reply);
}

static gpointer run_service(gpointer data) {
    const char *address = data;
    GMainContext *context = g_main_context_new();
    g_main_context_push_thread_default(context);
    service_connection = connect_bus(address);

    GError *error = NULL;
    GDBusNodeInfo *info = g_dbus_node_info_new_for_xml(introspection_xml, &error);
    assert(info && !error);
    assert(g_dbus_connection_register_object(service_connection, MUTTER_PATH,
        info->interfaces[0], &vtable, NULL, NULL, &error));
    assert(!error);
    assert(g_dbus_connection_register_object(service_connection, SESSION_PATH,
        info->interfaces[1], &vtable, NULL, NULL, &error));
    assert(!error);
    request_name(service_connection, MUTTER_DEST);
    request_name(service_connection, SESSION_DEST);

    service_loop = g_main_loop_new(context, FALSE);
    g_mutex_lock(&state_lock);
    service_ready = TRUE;
    g_cond_broadcast(&ready_cond);
    g_mutex_unlock(&state_lock);
    g_main_loop_run(service_loop);

    g_dbus_node_info_unref(info);
    g_main_loop_unref(service_loop);
    g_dbus_connection_close_sync(service_connection, NULL, NULL);
    g_object_unref(service_connection);
    g_main_context_pop_thread_default(context);
    g_main_context_unref(context);
    return NULL;
}

static gboolean has_owner(const char *name) {
    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(service_connection,
        "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
        "GetNameOwner", g_variant_new("(s)", name), G_VARIANT_TYPE("(s)"),
        G_DBUS_CALL_FLAGS_NONE, 500, NULL, &error);
    if (reply) g_variant_unref(reply);
    g_clear_error(&error);
    return reply != NULL;
}

static gboolean wait_until_gone(const char *name) {
    gint64 deadline = g_get_monotonic_time() + 3 * G_TIME_SPAN_SECOND;
    do {
        if (!has_owner(name)) return TRUE;
        g_usleep(10000);
    } while (g_get_monotonic_time() < deadline);
    return FALSE;
}

static char *copy_sender(unsigned which) {
    g_mutex_lock(&state_lock);
    char *sender = which == 0 ? wake_sender : which == 1 ? inhibit_sender : uninhibit_sender;
    char *copy = g_strdup(sender);
    g_mutex_unlock(&state_lock);
    return copy;
}

int main(void) {
    g_mutex_init(&state_lock);
    g_cond_init(&ready_cond);
    GTestDBus *bus = g_test_dbus_new(G_TEST_DBUS_NONE);
    g_test_dbus_up(bus);
    GThread *thread = g_thread_new("fake-session-bus", run_service,
        (gpointer)g_test_dbus_get_bus_address(bus));

    g_mutex_lock(&state_lock);
    while (!service_ready) g_cond_wait(&ready_cond, &state_lock);
    g_mutex_unlock(&state_lock);

    assert(uu_desktop_power_wake() == 0);
    char *wake_owner = copy_sender(0);
    assert(wake_owner && wait_until_gone(wake_owner));
    g_free(wake_owner);

    int error = 0;
    struct uu_idle_inhibitor *first = uu_desktop_idle_inhibitor_acquire(&error);
    assert(first && error == 0);
    char *first_owner = copy_sender(1);
    assert(first_owner && has_owner(first_owner));

    struct uu_idle_inhibitor *second = uu_desktop_idle_inhibitor_acquire(&error);
    assert(second && error == 0);
    char *second_owner = copy_sender(1);
    assert(second_owner && strcmp(first_owner, second_owner));
    assert(has_owner(first_owner) && has_owner(second_owner));

    uu_desktop_idle_inhibitor_release(first);
    g_mutex_lock(&state_lock);
    assert(inhibit_calls == 2 && last_flags == 8);
    assert(uninhibit_calls == 1 && last_cookie == 73);
    g_mutex_unlock(&state_lock);
    assert(wait_until_gone(first_owner) && has_owner(second_owner));

    g_mutex_lock(&state_lock);
    fail_uninhibit = TRUE;
    g_mutex_unlock(&state_lock);
    uu_desktop_idle_inhibitor_release(second);
    g_mutex_lock(&state_lock);
    assert(uninhibit_calls == 2);
    assert(uninhibit_sender && strcmp(uninhibit_sender, second_owner) == 0);
    g_mutex_unlock(&state_lock);
    assert(wait_until_gone(second_owner));

    g_free(first_owner);
    g_free(second_owner);
    g_mutex_lock(&state_lock);
    g_main_loop_quit(service_loop);
    g_mutex_unlock(&state_lock);
    g_thread_join(thread);
    g_test_dbus_down(bus);
    g_object_unref(bus);
    g_free(wake_sender);
    g_free(inhibit_sender);
    g_free(uninhibit_sender);
    g_cond_clear(&ready_cond);
    g_mutex_clear(&state_lock);
    return 0;
}
