#include <gio/gio.h>
#include <stdlib.h>

#define CALL_TIMEOUT_MS 3000
#define MUTTER_DEST "org.gnome.Mutter.DisplayConfig"
#define MUTTER_PATH "/org/gnome/Mutter/DisplayConfig"
#define SESSION_DEST "org.gnome.SessionManager"
#define SESSION_PATH "/org/gnome/SessionManager"
#define SESSION_IFACE "org.gnome.SessionManager"

enum {
    UU_POWER_OK = 0,
    UU_POWER_SESSION_BUS = 1,
    UU_POWER_CALL_FAILED = 2,
};

struct uu_idle_inhibitor {
    GDBusConnection *connection;
    guint32 cookie;
};

struct setup_timeout {
    GMutex lock;
    GCond cond;
    gboolean finished;
    GCancellable *cancellable;
};

static gpointer cancel_setup_on_timeout(gpointer data) {
    struct setup_timeout *timeout = data;
    gint64 deadline = g_get_monotonic_time() + CALL_TIMEOUT_MS * G_TIME_SPAN_MILLISECOND;
    g_mutex_lock(&timeout->lock);
    while (!timeout->finished &&
        g_cond_wait_until(&timeout->cond, &timeout->lock, deadline)) {}
    gboolean expired = !timeout->finished;
    g_mutex_unlock(&timeout->lock);
    if (expired) g_cancellable_cancel(timeout->cancellable);
    return NULL;
}

static void finish_setup_timeout(struct setup_timeout *timeout, GThread *thread) {
    g_mutex_lock(&timeout->lock);
    timeout->finished = TRUE;
    g_cond_signal(&timeout->cond);
    g_mutex_unlock(&timeout->lock);
    g_thread_join(thread);
    g_cond_clear(&timeout->cond);
    g_mutex_clear(&timeout->lock);
    g_object_unref(timeout->cancellable);
}

/* Use a dedicated connection so closing this inhibitor cannot affect another app's bus state. */
static GDBusConnection *private_session_bus(int *error_code) {
    struct setup_timeout timeout = {0};
    g_mutex_init(&timeout.lock);
    g_cond_init(&timeout.cond);
    timeout.cancellable = g_cancellable_new();
    GThread *timer = g_thread_new("uu-dbus-connect-timeout", cancel_setup_on_timeout, &timeout);

    GError *error = NULL;
    gchar *address = g_dbus_address_get_for_bus_sync(G_BUS_TYPE_SESSION, timeout.cancellable, &error);
    if (!address) {
        g_clear_error(&error);
        finish_setup_timeout(&timeout, timer);
        *error_code = UU_POWER_SESSION_BUS;
        return NULL;
    }

    GDBusConnection *connection = g_dbus_connection_new_for_address_sync(
        address,
        G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT |
            G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION,
        NULL, timeout.cancellable, &error);
    g_free(address);
    finish_setup_timeout(&timeout, timer);
    if (!connection) {
        g_clear_error(&error);
        *error_code = UU_POWER_SESSION_BUS;
        return NULL;
    }
    g_dbus_connection_set_exit_on_close(connection, FALSE);
    return connection;
}

static void close_private_bus(GDBusConnection *connection) {
    if (!connection) return;
    g_dbus_connection_close(connection, NULL, NULL, NULL);
    g_object_unref(connection);
}

int uu_desktop_power_wake(void) {
    int error_code = UU_POWER_OK;
    GDBusConnection *connection = private_session_bus(&error_code);
    if (!connection) return error_code;

    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(
        connection, MUTTER_DEST, MUTTER_PATH, "org.freedesktop.DBus.Properties", "Set",
        g_variant_new("(ssv)", "org.gnome.Mutter.DisplayConfig", "PowerSaveMode",
            g_variant_new_int32(0)),
        G_VARIANT_TYPE("()"), G_DBUS_CALL_FLAGS_NONE, CALL_TIMEOUT_MS, NULL, &error);
    if (!reply) error_code = UU_POWER_CALL_FAILED;
    if (reply) g_variant_unref(reply);
    g_clear_error(&error);
    close_private_bus(connection);
    return error_code;
}

struct uu_idle_inhibitor *uu_desktop_idle_inhibitor_acquire(int *error_code) {
    if (!error_code) return NULL;
    *error_code = UU_POWER_OK;
    GDBusConnection *connection = private_session_bus(error_code);
    if (!connection) return NULL;

    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(
        connection, SESSION_DEST, SESSION_PATH, SESSION_IFACE, "Inhibit",
        g_variant_new("(susu)", "uu-remote-linux", 0u,
            "Remote desktop session is active", 8u),
        G_VARIANT_TYPE("(u)"), G_DBUS_CALL_FLAGS_NONE, CALL_TIMEOUT_MS, NULL, &error);
    if (!reply) {
        *error_code = UU_POWER_CALL_FAILED;
        g_clear_error(&error);
        close_private_bus(connection);
        return NULL;
    }

    struct uu_idle_inhibitor *inhibitor = calloc(1, sizeof(*inhibitor));
    if (!inhibitor) {
        *error_code = UU_POWER_CALL_FAILED;
        g_variant_unref(reply);
        close_private_bus(connection);
        return NULL;
    }
    g_variant_get(reply, "(u)", &inhibitor->cookie);
    g_variant_unref(reply);
    inhibitor->connection = connection;
    return inhibitor;
}

void uu_desktop_idle_inhibitor_release(struct uu_idle_inhibitor *inhibitor) {
    if (!inhibitor) return;
    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(
        inhibitor->connection, SESSION_DEST, SESSION_PATH, SESSION_IFACE, "Uninhibit",
        g_variant_new("(u)", inhibitor->cookie), G_VARIANT_TYPE("()"),
        G_DBUS_CALL_FLAGS_NONE, CALL_TIMEOUT_MS, NULL, &error);
    if (reply) g_variant_unref(reply);
    g_clear_error(&error);

    /* Closing the private bus also revokes the cookie if Uninhibit failed or the host exits. */
    close_private_bus(inhibitor->connection);
    free(inhibitor);
}
