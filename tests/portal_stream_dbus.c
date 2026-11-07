/* Private mock D-Bus only: exercises Closed delivery during a PipeWire loop. */
#define main portal_probe_main
#include "../contrib/portal-dmabuf-probe.c"
#undef main
int main(void) {
    GTestDBus *bus=g_test_dbus_new(G_TEST_DBUS_NONE); g_test_dbus_up(bus);
    GError *error=NULL;
    GDBusConnectionFlags flags=G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT|G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION;
    GDBusConnection *producer=g_dbus_connection_new_for_address_sync(g_test_dbus_get_bus_address(bus),flags,NULL,NULL,&error);
    GDBusConnection *consumer=g_dbus_connection_new_for_address_sync(g_test_dbus_get_bus_address(bus),flags,NULL,NULL,&error);
    assert(producer && consumer);
    GVariant *reply=g_dbus_connection_call_sync(producer,"org.freedesktop.DBus","/org/freedesktop/DBus",
        "org.freedesktop.DBus","RequestName",g_variant_new("(su)",DEST,0),G_VARIANT_TYPE("(u)"),0,1000,NULL,&error);
    assert(reply); g_variant_unref(reply);
    pw_init(NULL,NULL); struct probe p={0}; p.loop=pw_main_loop_new(NULL); assert(p.loop);
    struct portal_watch watch={.probe=&p}; const char *path="/org/freedesktop/portal/desktop/session/test";
    guint sub=g_dbus_connection_signal_subscribe(consumer,DEST,SESSION,"Closed",path,NULL,0,portal_closed_cb,&watch,NULL);
    reply=g_dbus_connection_call_sync(consumer,"org.freedesktop.DBus","/org/freedesktop/DBus",
        "org.freedesktop.DBus","GetId",NULL,G_VARIANT_TYPE("(s)"),0,1000,NULL,&error);
    assert(reply); g_variant_unref(reply);
    assert(g_dbus_connection_emit_signal(producer,NULL,"/wrong/session",SESSION,"Closed",NULL,&error));
    assert(g_dbus_connection_flush_sync(producer,NULL,&error));
    gint64 until=g_get_monotonic_time()+50000;
    while(g_get_monotonic_time()<until) { dispatch_portal_events(NULL,0); g_usleep(1000); }
    assert(!watch.closed && !p.portal_closed);
    struct pw_loop *loop=pw_main_loop_get_loop(p.loop);
    struct spa_source *control=pw_loop_add_timer(loop,dispatch_portal_events,&watch);
    struct spa_source *guard=pw_loop_add_timer(loop,timeout_cb,&p); assert(control && guard);
    struct timespec tick={.tv_nsec=20000000},deadline={.tv_sec=1};
    assert(!pw_loop_update_timer(loop,control,&tick,&tick,false));
    assert(!pw_loop_update_timer(loop,guard,&deadline,NULL,false));
    assert(g_dbus_connection_emit_signal(producer,NULL,path,SESSION,"Closed",
        g_variant_new("(@a{sv})",g_variant_new_array(G_VARIANT_TYPE("{sv}"),NULL,0)),&error));
    assert(g_dbus_connection_flush_sync(producer,NULL,&error));
    assert(!pw_main_loop_run(p.loop));
    assert(watch.closed && p.portal_closed && p.stop_reasons==STOP_PORTAL);
    g_dbus_connection_signal_unsubscribe(consumer,sub);
    pw_loop_destroy_source(loop,control); pw_loop_destroy_source(loop,guard);
    pw_main_loop_destroy(p.loop); pw_deinit();
    assert(g_dbus_connection_close_sync(consumer,NULL,&error));
    assert(g_dbus_connection_close_sync(producer,NULL,&error));
    g_object_unref(consumer); g_object_unref(producer); g_test_dbus_down(bus); g_object_unref(bus);
    puts("PASS: private D-Bus Closed delivered during PipeWire wait; unrelated session ignored");
}
