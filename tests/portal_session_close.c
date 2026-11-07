#define _GNU_SOURCE
/* Fault injection only: no real D-Bus or Portal connection. */
#include <assert.h>
#include <gio/gio.h>
#include <errno.h>
static int calls, fail_close, revoke_in_call, nested_result;
static void *active;
static GVariant *close_call(GDBusConnection *, const gchar *, const gchar *,
    const gchar *, const gchar *, GVariant *, const GVariantType *,
    GDBusCallFlags, gint, GCancellable *, GError **);
#define g_dbus_connection_call_sync close_call
#include "../c_engine/portal_engine.c"
#undef g_dbus_connection_call_sync
static GVariant *close_call(GDBusConnection *bus, const gchar *dest, const gchar *path,
    const gchar *iface, const gchar *method, GVariant *args, const GVariantType *type,
    GDBusCallFlags flags, gint timeout, GCancellable *cancel, GError **error) {
    (void)bus; (void)dest; (void)path; (void)iface; (void)args; (void)flags; (void)cancel;
    assert(!strcmp(method, "Close") && timeout == 3000);
    assert(g_variant_type_equal(type, G_VARIANT_TYPE_UNIT));
    calls++;
    assert(atomic_load(&((struct uu_portal *)active)->closed));
    nested_result = uu_portal_end_checked(active);
    if (revoke_in_call) session_closed(NULL,NULL,NULL,NULL,NULL,NULL,active);
    if (fail_close) {
        g_set_error_literal(error, G_IO_ERROR, G_IO_ERROR_TIMED_OUT, "injected timeout");
        return NULL;
    }
    return g_variant_ref_sink(g_variant_new("()"));
}
int main(void) {
    struct uu_portal p = {0};
    pthread_mutex_init(&p.lock, NULL); pthread_cond_init(&p.changed, NULL);
    active = &p;
    assert(uu_portal_end_checked(NULL) == -EINVAL);
    assert(uu_portal_recovery_status(NULL) == -EINVAL);
    assert(uu_portal_recovery_status(&p) == 0);
    assert(uu_portal_end_checked(&p) == -ENOTCONN);
    assert(atomic_load(&p.closed) && !calls);
    p.bus = (void *)1; p.session = "/test/session";
    fail_close = 1;
    assert(uu_portal_end_checked(&p) == -EIO);
    assert(nested_result == -EBUSY && calls == 1);
    assert(atomic_load(&p.close_request) == 0);
    fail_close = 0;
    assert(uu_portal_end_checked(&p) == 0 && calls == 2);
    assert(uu_portal_end_checked(&p) == 0 && calls == 2);
    assert(!atomic_load(&p.remote_closed));
    atomic_store(&p.close_request, 0);
    fail_close = revoke_in_call = 1;
    assert(uu_portal_end_checked(&p) == 0 && calls == 3);
    assert(atomic_load(&p.remote_closed));
    assert(uu_portal_end_checked(&p) == 0 && calls == 3);
    assert(uu_portal_recovery_status(&p) == 0); /* Closed after local end is ambiguous. */
    pthread_cond_destroy(&p.changed); pthread_mutex_destroy(&p.lock);
    struct uu_portal revoked = {0};
    pthread_mutex_init(&revoked.lock, NULL); pthread_cond_init(&revoked.changed, NULL);
    session_closed(NULL,NULL,NULL,NULL,NULL,NULL,&revoked);
    assert(uu_portal_recovery_status(&revoked) == -ECANCELED);
    assert(uu_portal_end_checked(&revoked) == 0 && calls == 3);
    assert(uu_portal_recovery_status(&revoked) == -ECANCELED);
    session_closed(NULL,NULL,NULL,NULL,NULL,NULL,&revoked);
    assert(uu_portal_recovery_status(&revoked) == -ECANCELED);
    pthread_cond_destroy(&revoked.changed); pthread_mutex_destroy(&revoked.lock);
    puts("PASS: close failure/retry, Closed ordering, observed external revoke remains nonrecoverable after local cleanup");
    return 0;
}
