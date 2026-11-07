/* Private GTestDBus mock only; no real portal or display.
 * cc -g -O1 -fsanitize=address,undefined -fno-omit-frame-pointer tests/portal_dmabuf_request_dbus.c -o /tmp/portal-dmabuf-request-test $(pkg-config --cflags --libs gio-2.0 gio-unix-2.0 libpipewire-0.3)
 * ASAN_OPTIONS=detect_leaks=1 /tmp/portal-dmabuf-request-test
 */
#define PROBE_RESPONSE_TIMEOUT_US (50 * G_TIME_SPAN_MILLISECOND)
#define main probe_main
#include "../contrib/portal-dmabuf-probe.c"
#undef main

#define TEST_SESSION "/org/freedesktop/portal/desktop/session/test/session"
enum scenario { SUCCESS, CANCEL, TIMEOUT, ERROR, DIFFERENT, EARLY_DIFFERENT };
struct fixture { GMutex mutex; enum scenario scenario; enum request_mode mode; char *predicted, *other; guint registrations[2]; int calls, closes, other_closes; };
static const char xml[] =
 "<node><interface name='org.freedesktop.portal.ScreenCast'>"
 "<method name='CreateSession'><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
 "<method name='SelectSources'><arg type='o' direction='in'/><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
 "<method name='Start'><arg type='o' direction='in'/><arg type='s' direction='in'/><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
 "</interface><interface name='org.freedesktop.portal.RemoteDesktop'>"
 "<method name='SelectDevices'><arg type='o' direction='in'/><arg type='a{sv}' direction='in'/><arg type='o' direction='out'/></method>"
 "</interface><interface name='org.freedesktop.portal.Request'><method name='Close'/></interface></node>";
static GDBusNodeInfo *info;
static void call(GDBusConnection *, const char *, const char *, const char *, const char *, GVariant *, GDBusMethodInvocation *, gpointer);
static const GDBusInterfaceVTable vtable = { .method_call=call };
static void call(GDBusConnection *bus, const char *sender, const char *path, const char *interface,
                 const char *method, GVariant *params, GDBusMethodInvocation *inv, gpointer data) {
    struct fixture *f=data;
    g_mutex_lock(&f->mutex);
    if (!strcmp(interface,REQUEST)) {
        assert(!strcmp(method,"Close"));
        if (!strcmp(path,f->predicted)) f->closes++; else { assert(!strcmp(path,f->other)); f->other_closes++; }
        g_mutex_unlock(&f->mutex);
        g_dbus_method_invocation_return_value(inv,g_variant_new("()")); return;
    }
    f->calls++;
    const char *methods[]={"CreateSession","SelectSources","Start","SelectDevices"};
    assert(!strcmp(method,methods[f->mode]));
    assert(!strcmp(interface,f->mode==DEVICES ? REMOTE : SCREEN));
    assert(g_variant_is_of_type(params,G_VARIANT_TYPE(f->mode==CREATE ? "(a{sv})" : f->mode==START ? "(osa{sv})" : "(oa{sv})")));
    if (f->mode!=CREATE) { const char *s; g_variant_get_child(params,0,"&o",&s); assert(!strcmp(s,TEST_SESSION)); }
    if (f->mode==START) { const char *s; g_variant_get_child(params,1,"&s",&s); assert(!*s); }
    GVariant *opts=g_variant_get_child_value(params,f->mode==CREATE ? 0 : f->mode==START ? 2 : 1);
    const char *token=NULL; assert(g_variant_lookup(opts,"handle_token","&s",&token));
    char expected[80]; snprintf(expected,sizeof(expected),"dmadiag_%u_",(unsigned)getpid()); assert(g_str_has_prefix(token,expected));
    char *name=g_strdup(sender+1); for (char *p=name;*p;p++) if (*p=='.') *p='_';
    f->predicted=g_strdup_printf(PATH "/request/%s/%s",name,token);
    f->other=g_strdup_printf(PATH "/request/%s/other",name); g_free(name);
    assert(g_variant_n_children(opts)==(f->mode==SOURCES ? 3u : f->mode==START ? 1u : 2u));
    guint32 types; gboolean multiple; const char *st;
    if (f->mode==CREATE) { char session_token[80]; snprintf(session_token,sizeof(session_token),"dmadiag_session_%u",(unsigned)getpid()); assert(g_variant_lookup(opts,"session_handle_token","&s",&st) && !strcmp(st,session_token)); }
    if (f->mode==SOURCES || f->mode==DEVICES) assert(g_variant_lookup(opts,"types","u",&types) && types==(f->mode==SOURCES ? 1u : 3u));
    if (f->mode==SOURCES) assert(g_variant_lookup(opts,"multiple","b",&multiple) && !multiple);
    g_variant_unref(opts);
    GError *error=NULL;
    f->registrations[0]=g_dbus_connection_register_object(bus,f->predicted,info->interfaces[2],&vtable,f,NULL,&error); assert(f->registrations[0] && !error);
    f->registrations[1]=g_dbus_connection_register_object(bus,f->other,info->interfaces[2],&vtable,f,NULL,&error); assert(f->registrations[1] && !error);
    /* The service sends the signal before its reply; client dispatch order is not asserted. */
    if (f->scenario==SUCCESS || f->scenario==CANCEL || f->scenario==EARLY_DIFFERENT) {
        GVariantBuilder b; g_variant_builder_init(&b,G_VARIANT_TYPE_VARDICT);
        g_variant_builder_add(&b,"{sv}","session_handle",g_variant_new_string(TEST_SESSION));
        assert(g_dbus_connection_emit_signal(bus,sender,f->predicted,REQUEST,"Response",g_variant_new("(u@a{sv})",f->scenario==CANCEL ? 1u : 0u,g_variant_builder_end(&b)),&error) && !error);
    }
    if (f->scenario==ERROR) g_dbus_method_invocation_return_dbus_error(inv,"org.test.Failure","mock error");
    else g_dbus_method_invocation_return_value(inv,g_variant_new("(o)",(f->scenario==DIFFERENT || f->scenario==EARLY_DIFFERENT) ? f->other : f->predicted));
    g_mutex_unlock(&f->mutex);
}
static gpointer run_loop(gpointer p) { GMainLoop *loop=p; GMainContext *context=g_main_loop_get_context(loop); g_main_context_push_thread_default(context); g_main_loop_run(loop); g_main_context_pop_thread_default(context); return NULL; }
int main(void) {
    GTestDBus *test=g_test_dbus_new(G_TEST_DBUS_NONE); g_test_dbus_up(test);
    const char *address=g_test_dbus_get_bus_address(test); GError *error=NULL;
    GDBusConnectionFlags flags=G_DBUS_CONNECTION_FLAGS_AUTHENTICATION_CLIENT|G_DBUS_CONNECTION_FLAGS_MESSAGE_BUS_CONNECTION;
    GDBusConnection *service=g_dbus_connection_new_for_address_sync(address,flags,NULL,NULL,&error); assert(service && !error);
    GDBusConnection *client=g_dbus_connection_new_for_address_sync(address,flags,NULL,NULL,&error); assert(client && !error);
    GVariant *v=g_dbus_connection_call_sync(service,"org.freedesktop.DBus","/org/freedesktop/DBus","org.freedesktop.DBus","RequestName",g_variant_new("(su)",DEST,0u),G_VARIANT_TYPE("(u)"),0,3000,NULL,&error);
    assert(v && !error); guint name_result; g_variant_get(v,"(u)",&name_result); assert(name_result==1); g_variant_unref(v);
    info=g_dbus_node_info_new_for_xml(xml,&error); assert(info && !error);
    GMainContext *context=g_main_context_new(); GMainLoop *loop=g_main_loop_new(context,FALSE);
    struct fixture f={0}; g_mutex_init(&f.mutex); g_main_context_push_thread_default(context);
    guint a=g_dbus_connection_register_object(service,PATH,info->interfaces[0],&vtable,&f,NULL,&error);
    guint b=g_dbus_connection_register_object(service,PATH,info->interfaces[1],&vtable,&f,NULL,&error);
    g_main_context_pop_thread_default(context); assert(a && b && !error);
    GThread *thread=g_thread_new("mock",run_loop,loop);
    for (int mode=CREATE;mode<=DEVICES;mode++) for (int scenario=SUCCESS;scenario<=EARLY_DIFFERENT;scenario++) {
        g_mutex_lock(&f.mutex);
        f.mode=mode; f.scenario=scenario; f.calls=f.closes=f.other_closes=0;
        g_mutex_unlock(&f.mutex);
        const char *methods[]={"CreateSession","SelectSources","Start","SelectDevices"};
        FILE *log=tmpfile(); assert(log); fflush(stderr); int saved=dup(STDERR_FILENO); assert(saved>=0 && dup2(fileno(log),STDERR_FILENO)>=0);
        v=request(client,mode==DEVICES ? REMOTE : SCREEN,methods[mode],mode==CREATE ? NULL : TEST_SESSION,mode);
        fflush(stderr); assert(dup2(saved,STDERR_FILENO)>=0); close(saved);
        rewind(log); char output[4096]; size_t length=fread(output,1,sizeof(output)-1,log); output[length]=0; fclose(log);
        assert(!strstr(output,"Session.Close"));
        assert(!!strstr(output,"session cleanup cannot be guaranteed") == (mode==CREATE && scenario!=SUCCESS));
        char error_log[128]; snprintf(error_log,sizeof(error_log),"D-Bus %s failed:",methods[mode]);
        assert(!!strstr(output,"Response code=1") == (scenario==CANCEL));
        assert(!!strstr(output,"Response timed out") == (scenario==TIMEOUT));
        assert(!!strstr(output,error_log) == (scenario==ERROR));
        g_mutex_lock(&f.mutex);
        assert(f.calls==1 && f.other_closes==0 && f.closes==(scenario!=SUCCESS));
        if (scenario==SUCCESS) { assert(v); char *s=session_handle(v); assert(s && !strcmp(s,TEST_SESSION)); g_free(s); g_variant_unref(v); }
        else assert(!v);
        for (int i=0;i<2;i++) g_dbus_connection_unregister_object(service,f.registrations[i]);
        g_clear_pointer(&f.predicted,g_free); g_clear_pointer(&f.other,g_free);
        g_mutex_unlock(&f.mutex);
    }
    g_main_loop_quit(loop); g_thread_join(thread); g_mutex_clear(&f.mutex);
    g_dbus_connection_unregister_object(service,a); g_dbus_connection_unregister_object(service,b);
    g_dbus_node_info_unref(info); g_main_loop_unref(loop); g_main_context_unref(context);
    g_dbus_connection_close_sync(client,NULL,NULL); g_object_unref(client);
    g_dbus_connection_close_sync(service,NULL,NULL); g_object_unref(service);
    g_test_dbus_down(test); g_object_unref(test);
    puts("Private mock request tests passed (not a real Portal test)."); return 0;
}
