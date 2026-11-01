#define _GNU_SOURCE
#include "portal_engine.h"
#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <libei.h>
#include <poll.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/pod/builder.h>
#include <spa/buffer/meta.h>
#include <pthread.h>
#include <stdatomic.h>
#include <math.h>
#include <time.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

#define DEST "org.freedesktop.portal.Desktop"
#define PATH "/org/freedesktop/portal/desktop"
#define REMOTE "org.freedesktop.portal.RemoteDesktop"
#define SCREEN "org.freedesktop.portal.ScreenCast"
#define REQUEST "org.freedesktop.portal.Request"
#define SESSION "org.freedesktop.portal.Session"
#define PORTAL_RENEGOTIATION_TIMEOUT (5 * G_TIME_SPAN_SECOND)
#define PORTAL_DISPLAY_GRACE (200 * G_TIME_SPAN_MILLISECOND)
#define MUTTER_DISPLAY "org.gnome.Mutter.DisplayConfig"
#define MUTTER_DISPLAY_PATH "/org/gnome/Mutter/DisplayConfig"

struct uu_portal {
    GDBusConnection *bus;
    GMainContext *context;
    GMainLoop *main_loop;
    pthread_t dbus_thread;
    int dbus_started;
    char *session;
    char *restore_token;
    guint closed_subscription, clipboard_owner_subscription, clipboard_transfer_subscription, display_subscription;
    int clipboard_enabled, clipboard_changed;
    char *clipboard_text, *clipboard_mime;
    size_t clipboard_length;
    uint64_t clipboard_revision;
    struct pw_thread_loop *pw_loop;
    struct pw_context *pw_context;
    struct pw_core *core;
    struct pw_stream *stream;
    struct spa_hook stream_listener;
    pthread_mutex_t lock;
    pthread_cond_t changed;
    atomic_int closed;
    uu_portal_cancelled_fn cancelled;
    void *cancel_data;
    int error;
    enum pw_stream_state state;
    int format_valid, stream_started;
    gint64 interrupted_at, display_changed_at;
    uint32_t node, width, height;
    int logical_width, logical_height, position_x, position_y, has_position;
    int absolute_stream_pixels;
    uint32_t devices, available;
    struct ei *ei;
    GList *ei_devices;
    struct ei_touch *ei_touch;
    char *mapping_id;
    uint32_t ei_sequence;
    atomic_int ei_failed;
    int ei_unavailable;
    enum spa_video_format format;
    uint8_t *frame;
    size_t frame_length;
    uint64_t sequence;
    uint64_t revision;
};
#include "portal_clipboard.inc"

static pthread_once_t pw_once = PTHREAD_ONCE_INIT;
static atomic_uint token_sequence = 1;
static void initialize_pipewire(void) { pw_init(NULL, NULL); }
static GVariant *empty_options(void) { return g_variant_new_array(G_VARIANT_TYPE("{sv}"), NULL, 0); }
static int open_cancelled(struct uu_portal *p) {
    return p->cancelled && p->cancelled(p->cancel_data);
}
static int wait_for_first_frame(struct uu_portal *p);
#include "portal_eis.inc"
/* A uint32 RemoteDesktop property (version, AvailableDeviceTypes); 0 when unreadable. */
static guint remote_property(struct uu_portal *p, const char *name) {
    GVariant *reply = g_dbus_connection_call_sync(p->bus, DEST, PATH,
        "org.freedesktop.DBus.Properties", "Get", g_variant_new("(ss)", REMOTE, name),
        G_VARIANT_TYPE("(v)"), G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
    guint result = 0;
    if (reply) {
        GVariant *value; g_variant_get(reply, "(v)", &value);
        if (g_variant_is_of_type(value, G_VARIANT_TYPE_UINT32)) result = g_variant_get_uint32(value);
        g_variant_unref(value); g_variant_unref(reply);
    }
    return result;
}
static void fail(struct uu_portal *p, int error) {
    pthread_mutex_lock(&p->lock);
    if (!p->error) p->error = error;
    pthread_cond_broadcast(&p->changed);
    pthread_mutex_unlock(&p->lock);
}
struct response { int done; guint code; GVariant *results; };
static void response_received(GDBusConnection *bus, const gchar *sender, const gchar *path,
    const gchar *interface, const gchar *signal, GVariant *parameters, gpointer data) {
    (void)bus; (void)sender; (void)path; (void)interface; (void)signal;
    struct response *r = data;
    if (r->done) return;
    g_variant_get(parameters, "(u@a{sv})", &r->code, &r->results);
    r->done = 1;
}
/* Bounded consent wait; cancellation must break it within one poll interval. */
static int wait_for_response(struct uu_portal *p, struct response *response, unsigned consent_seconds) {
    gint64 deadline = g_get_monotonic_time() + consent_seconds * G_TIME_SPAN_SECOND;
    while (!response->done && !atomic_load(&p->closed) && !open_cancelled(p) &&
        g_get_monotonic_time() < deadline) {
        while (g_main_context_iteration(p->context, FALSE)) {}
        if (!response->done) g_usleep(5000);
    }
    return response->done ? 0 : open_cancelled(p) ? 13 : atomic_load(&p->closed) ? 9 : 5;
}
static void close_request(struct uu_portal *p, const char *path) {
    if (!path) return;
    GVariant *r = g_dbus_connection_call_sync(p->bus, DEST, path, REQUEST, "Close",
        NULL, NULL, G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
    if (r) g_variant_unref(r);
}
/* Subscribe before invocation, using the documented handle_token path. */
static GVariant *request(struct uu_portal *p, const char *interface, const char *method,
    GVariantBuilder *options, int mode, int *error) {
    if (open_cancelled(p)) { *error = 13; return NULL; }
    char token[64];
    snprintf(token, sizeof(token), "uu_%u_%u", (unsigned)getpid(), atomic_fetch_add(&token_sequence, 1));
    g_variant_builder_add(options, "{sv}", "handle_token", g_variant_new_string(token));
    char *sender = g_strdup(g_dbus_connection_get_unique_name(p->bus) + 1);
    for (char *c = sender; *c; c++) if (*c == '.') *c = '_';
    char *expected = g_strdup_printf("/org/freedesktop/portal/desktop/request/%s/%s", sender, token);
    g_free(sender);
    struct response response = {0};
    guint subscription = g_dbus_connection_signal_subscribe(p->bus, DEST, REQUEST, "Response", expected,
        NULL, G_DBUS_SIGNAL_FLAGS_NONE, response_received, &response, NULL);
    GVariant *args = mode == 0 ? g_variant_new("(@a{sv})", g_variant_builder_end(options)) :
        mode == 1 ? g_variant_new("(o@a{sv})", p->session, g_variant_builder_end(options)) :
        g_variant_new("(os@a{sv})", p->session, "", g_variant_builder_end(options));
    GVariant *reply = g_dbus_connection_call_sync(p->bus, DEST, PATH, interface, method, args,
        G_VARIANT_TYPE("(o)"), G_DBUS_CALL_FLAGS_NONE, 10000, NULL, NULL);
    char *actual = NULL;
    if (!reply) { *error = open_cancelled(p) ? 13 : 3; goto finish; }
    const char *handle;
    g_variant_get(reply, "(&o)", &handle);
    actual = g_strdup(handle);
    g_variant_unref(reply);
    if (strcmp(actual, expected)) { *error = 3; goto finish; }
    unsigned consent_seconds=120;
    const char *configured_timeout=getenv("UU_PORTAL_CONSENT_TIMEOUT_SECONDS");
    if(configured_timeout) {
        char *end=NULL;
        unsigned long parsed=strtoul(configured_timeout,&end,10);
        if(end && !*end && parsed>=30 && parsed<=900) consent_seconds=(unsigned)parsed;
    }
    *error = wait_for_response(p, &response, consent_seconds);
    if (!*error && response.code != 0) *error = 4;
finish:
    if (*error) close_request(p, actual ? actual : expected);
    g_dbus_connection_signal_unsubscribe(p->bus, subscription);
    g_free(actual); g_free(expected);
    if (*error && response.results) { g_variant_unref(response.results); response.results = NULL; }
    return response.results;
}
static void session_closed(GDBusConnection *bus, const gchar *sender, const gchar *path,
    const gchar *interface, const gchar *signal, GVariant *parameters, gpointer data) {
    (void)bus; (void)sender; (void)path; (void)interface; (void)signal; (void)parameters;
    struct uu_portal *p = data;
    atomic_store(&p->closed, 1); fail(p, 9);
}
static void *run_dbus(void *data) {
    struct uu_portal *p = data;
    g_main_context_push_thread_default(p->context);
    g_main_loop_run(p->main_loop);
    g_main_context_pop_thread_default(p->context);
    return NULL;
}
static int has_interface(struct uu_portal *p, const char *interface) {
    GVariant *r = g_dbus_connection_call_sync(p->bus, DEST, PATH, "org.freedesktop.DBus.Properties",
        "Get", g_variant_new("(ss)", interface, "version"), G_VARIANT_TYPE("(v)"),
        G_DBUS_CALL_FLAGS_NONE, 5000, NULL, NULL);
    if (!r) return 0;
    g_variant_unref(r); return 1;
}
/* Called with the frame mutex held. Count only the continuous invalid pause,
 * not a healthy pause before format removal, and never arm during startup. */
static void update_interruption(struct uu_portal *p) {
    if (p->stream_started && p->state == PW_STREAM_STATE_PAUSED && !p->format_valid) {
        if (!p->interrupted_at) p->interrupted_at = g_get_monotonic_time();
    } else {
        if (p->interrupted_at) p->display_changed_at = 0;
        p->interrupted_at = 0;
    }
}
static void monitors_changed(GDBusConnection *bus, const gchar *sender, const gchar *path,
    const gchar *interface, const gchar *signal, GVariant *parameters, gpointer data) {
    struct uu_portal *p = data;
    pthread_mutex_lock(&p->lock);
    /* #27: measured NULL format 10-36ms after the notification, with PAUSED
     * 5-9ms before it. Preserve a correlated signal across repeated changes. */
    if (p->stream_started && (!p->display_changed_at || !p->interrupted_at ||
        p->display_changed_at < p->interrupted_at - PORTAL_DISPLAY_GRACE))
        p->display_changed_at = g_get_monotonic_time();
    pthread_mutex_unlock(&p->lock);
}
static void stream_state(void *data, enum pw_stream_state old, enum pw_stream_state state, const char *message) {
    (void)old; (void)message;
    struct uu_portal *p = data;
    pthread_mutex_lock(&p->lock);
    p->state = state;
    if (state == PW_STREAM_STATE_STREAMING) {
        p->stream_started = 1;
        p->display_changed_at = 0;
    }
    update_interruption(p);
    pthread_mutex_unlock(&p->lock);
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) fail(data, 9);
}
static void stream_format(void *data, uint32_t id, const struct spa_pod *param) {
    struct uu_portal *p = data;
    if (id != SPA_PARAM_Format) return;
    if (!param) {
        pthread_mutex_lock(&p->lock);
        p->format_valid = 0;
        update_interruption(p);
        pthread_mutex_unlock(&p->lock);
        return;
    }
    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0 || !info.size.width || !info.size.height ||
        info.size.width > 8192 || info.size.height > 8192 ||
        (info.format != SPA_VIDEO_FORMAT_BGRA && info.format != SPA_VIDEO_FORMAT_BGRx &&
         info.format != SPA_VIDEO_FORMAT_RGBA && info.format != SPA_VIDEO_FORMAT_RGBx)) { fail(p, 8); return; }
    pthread_mutex_lock(&p->lock);
    if (p->width && (p->width != info.size.width || p->height != info.size.height)) {
        p->error = 8; pthread_cond_broadcast(&p->changed); pthread_mutex_unlock(&p->lock); return;
    }
    size_t length = (size_t)info.size.width * info.size.height * 4;
    if (!p->frame) p->frame = malloc(length);
    if (!p->frame) { p->error = 8; pthread_mutex_unlock(&p->lock); return; }
    p->width = info.size.width; p->height = info.size.height; p->format = info.format;
    p->frame_length = length;
    p->format_valid = 1;
    p->display_changed_at = 0;
    update_interruption(p);
    pthread_cond_broadcast(&p->changed);
    pthread_mutex_unlock(&p->lock);
}
/* Called with the frame mutex held. Invalid transient chunks are discarded;
 * allocation capacity is not evidence that valid pixels have been supplied. */
static void copy_video_buffer(struct uu_portal *p, struct spa_buffer *spa) {
    if (!p->error && p->format_valid && p->frame && spa->n_datas >= 1) {
        struct spa_data *d = &spa->datas[0];
        struct spa_meta_header *header = spa_buffer_find_meta_data(spa, SPA_META_Header, sizeof(*header));
        if (!d->chunk || (d->chunk->flags & SPA_CHUNK_FLAG_CORRUPTED) ||
            (header && (header->flags & SPA_META_HEADER_FLAG_CORRUPTED))) return;
        if (d->chunk->flags & SPA_CHUNK_FLAG_EMPTY) {
            /* EMPTY denotes media-neutral (black) video, never old pixels. */
            memset(p->frame, 0, p->frame_length);
            for (size_t pixel = 0; pixel < p->frame_length / 4; pixel++) p->frame[pixel * 4 + 3] = 255;
            p->sequence++;
            p->revision++;
            return;
        }
        struct spa_meta_region *crop = spa_buffer_find_meta_data(spa, SPA_META_VideoCrop, sizeof(*crop));
        if (crop && spa_meta_region_is_valid(crop) && (crop->region.position.x || crop->region.position.y ||
            crop->region.size.width != p->width || crop->region.size.height != p->height)) p->error = 8;
        if (!d->data || d->chunk->stride < (int32_t)(p->width * 4) || !d->chunk->size ||
            d->chunk->offset > d->maxsize ||
            (d->type != SPA_DATA_MemPtr && d->type != SPA_DATA_MemFd)) return;
        uint64_t required = (uint64_t)(p->height - 1) * (uint32_t)d->chunk->stride + p->width * 4;
        if (required > d->maxsize - d->chunk->offset || required > d->chunk->size) return;
        if (!p->error) {
            uint8_t *src = (uint8_t *)d->data + d->chunk->offset;
            for (uint32_t y = 0; y < p->height; y++) {
                uint8_t *row = src + (size_t)y * d->chunk->stride;
                uint8_t *dst = p->frame + (size_t)y * p->width * 4;
                if (p->format == SPA_VIDEO_FORMAT_BGRA || p->format == SPA_VIDEO_FORMAT_BGRx) memcpy(dst, row, p->width * 4);
                else for (uint32_t x = 0; x < p->width; x++) {
                    dst[4*x] = row[4*x+2]; dst[4*x+1] = row[4*x+1]; dst[4*x+2] = row[4*x]; dst[4*x+3] = 255;
                }
            }
            p->sequence++;
            p->revision++;
        }
    }
}
static void stream_process(void *data) {
    struct uu_portal *p = data;
    struct pw_buffer *buffer, *newest = NULL;
    while ((buffer = pw_stream_dequeue_buffer(p->stream))) {
        if (newest) pw_stream_queue_buffer(p->stream, newest);
        newest = buffer;
    }
    if (!newest) return;
    pthread_mutex_lock(&p->lock);
    copy_video_buffer(p, newest->buffer);
    pthread_cond_broadcast(&p->changed);
    pthread_mutex_unlock(&p->lock);
    pw_stream_queue_buffer(p->stream, newest);
}
static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS, .state_changed = stream_state, .param_changed = stream_format, .process = stream_process
};
static int connect_pipewire(struct uu_portal *p) {
    if (open_cancelled(p)) return 13;
    GUnixFDList *fds = NULL;
    GVariant *reply = g_dbus_connection_call_with_unix_fd_list_sync(p->bus, DEST, PATH, SCREEN,
        "OpenPipeWireRemote", g_variant_new("(o@a{sv})", p->session, empty_options()),
        G_VARIANT_TYPE("(h)"), G_DBUS_CALL_FLAGS_NONE, 10000, NULL, &fds, NULL, NULL);
    if (open_cancelled(p)) {
        if (reply) g_variant_unref(reply);
        if (fds) g_object_unref(fds);
        return 13;
    }
    if (!reply) return 7;
    gint32 index; g_variant_get(reply, "(h)", &index);
    int fd = g_unix_fd_list_get(fds, index, NULL);
    g_variant_unref(reply); g_object_unref(fds);
    if (fd < 0) return 7;
    pthread_once(&pw_once, initialize_pipewire);
    p->pw_loop = pw_thread_loop_new("uuremote-portal", NULL);
    if (!p->pw_loop) { close(fd); return 7; }
    p->pw_context = pw_context_new(pw_thread_loop_get_loop(p->pw_loop), NULL, 0);
    if (!p->pw_context) { close(fd); return 7; }
    /* pw_context_connect_fd takes ownership of fd, including failure. */
    p->core = pw_context_connect_fd(p->pw_context, fd, NULL, 0);
    if (!p->core) return 7;
    p->stream = pw_stream_new(p->core, "uuremote-portal-capture", pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture", PW_KEY_MEDIA_ROLE, "Screen", NULL));
    if (!p->stream) return 7;
    pw_stream_add_listener(p->stream, &p->stream_listener, &stream_events, p);
    uint8_t storage[1024]; struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
    const struct spa_pod *params[2];
    params[0] = spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
        SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video), SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
        SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(4, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_RGBA, SPA_VIDEO_FORMAT_RGBx),
        SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&SPA_RECTANGLE(1920,1080), &SPA_RECTANGLE(1,1), &SPA_RECTANGLE(8192,8192)),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(60,1), &SPA_FRACTION(0,1), &SPA_FRACTION(60,1)));
    params[1] = spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int((1 << SPA_DATA_MemPtr) | (1 << SPA_DATA_MemFd)));
    /* Node ID belongs to this portal's restricted remote. Do not put it into
     * target.object, whose numeric value means object.serial on modern PW. */
    if (pw_stream_connect(p->stream, PW_DIRECTION_INPUT, p->node,
        PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params, 2) < 0) return 7;
    if (pw_thread_loop_start(p->pw_loop) < 0) return 7;
    return wait_for_first_frame(p);
}
static int wait_for_first_frame(struct uu_portal *p) {
    struct timespec until;
    gint64 deadline = g_get_monotonic_time() + 10 * G_TIME_SPAN_SECOND;
    pthread_mutex_lock(&p->lock);
    while (!p->sequence && !p->error && !open_cancelled(p)) {
        gint64 remaining = deadline - g_get_monotonic_time();
        if (remaining <= 0) break;
        clock_gettime(CLOCK_REALTIME, &until);
        gint64 wait_us = MIN(100000, remaining);
        until.tv_nsec += wait_us * 1000;
        if (until.tv_nsec >= 1000000000) { until.tv_sec++; until.tv_nsec -= 1000000000; }
        pthread_cond_timedwait(&p->changed, &p->lock, &until);
    }
    int result = p->error ? p->error : open_cancelled(p) ? 13 : p->sequence ? 0 : 10;
    pthread_mutex_unlock(&p->lock);
    return result;
}
static int read_stream_result(struct uu_portal *p, GVariant *results) {
    guint devices = 0;
    GVariant *streams = g_variant_lookup_value(results, "streams", G_VARIANT_TYPE("a(ua{sv})"));
    gboolean has_devices = g_variant_lookup(results, "devices", "u", &devices);
    fprintf(stderr, "Portal authorization result: devices_present=%d device_bits=%u stream_count=%zu\n",
        has_devices, devices, streams ? (size_t)g_variant_n_children(streams) : 0);
    int error = 0;
    if (!has_devices || (devices & 3) != 3 || !streams || g_variant_n_children(streams) != 1) {
        error = 6;
    } else {
        p->devices = devices;
        GVariant *entry = g_variant_get_child_value(streams, 0), *properties;
        g_variant_get(entry, "(u@a{sv})", &p->node, &properties);
        /* Optional 'size' is compositor logical coordinates, not frame pixels. */
        g_variant_lookup(properties, "size", "(ii)", &p->logical_width, &p->logical_height);
        p->has_position = g_variant_lookup(properties, "position", "(ii)", &p->position_x, &p->position_y);
        const char *mapping_id = NULL;
        if (g_variant_lookup(properties, "mapping_id", "&s", &mapping_id)) p->mapping_id = g_strdup(mapping_id);
        g_variant_unref(properties); g_variant_unref(entry);
    }
    if (streams) g_variant_unref(streams);
    return error;
}
static struct uu_portal *open_persist(const char *restore, void (*save_token)(const char *, void *),
    void *save_data, uu_portal_cancelled_fn cancelled, void *cancel_data,
    int *error, uint32_t *width, uint32_t *height) {
    *error = 0; *width = *height = 0;
    struct uu_portal *p = calloc(1, sizeof(*p));
    if (!p) { *error = 8; return NULL; }
    p->cancelled = cancelled;
    p->cancel_data = cancel_data;
    /* GNOME forwards NotifyPointerMotionAbsolute into Mutter's stream-to-stage
     * transform. It expects stream pixels, unlike the compositor-sized KDE path.
     * Do not apply the compositor scale twice on fractional-scale displays. */
    gchar **desktops = g_strsplit(g_getenv("XDG_CURRENT_DESKTOP") ? g_getenv("XDG_CURRENT_DESKTOP") : "", ":", -1);
    for (gchar **desktop = desktops; *desktop; desktop++)
        if (!g_ascii_strcasecmp(*desktop, "GNOME")) p->absolute_stream_pixels = 1;
    g_strfreev(desktops);
    pthread_mutex_init(&p->lock, NULL); pthread_cond_init(&p->changed, NULL);
    p->context = g_main_context_new(); g_main_context_push_thread_default(p->context);
    p->bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, NULL);
    if (!p->bus) { *error = 1; goto done; }
    if (open_cancelled(p)) { *error = 13; goto done; }
    int interfaces_available = has_interface(p, REMOTE) && has_interface(p, SCREEN);
    if (open_cancelled(p)) { *error = 13; goto done; }
    if (!interfaces_available) { *error = 2; goto done; }
    GVariantBuilder options; GVariant *results;
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    char token[64]; snprintf(token, sizeof(token), "uu_session_%u_%u", (unsigned)getpid(), atomic_fetch_add(&token_sequence, 1));
    g_variant_builder_add(&options, "{sv}", "session_handle_token", g_variant_new_string(token));
    results = request(p, REMOTE, "CreateSession", &options, 0, error);
    if (!results) goto done;
    const char *handle = NULL;
    if (g_variant_lookup(results, "session_handle", "&s", &handle)) p->session = g_strdup(handle);
    g_variant_unref(results);
    if (!p->session || !g_variant_is_object_path(p->session)) { *error = 3; goto done; }
    p->closed_subscription = g_dbus_connection_signal_subscribe(p->bus, DEST, SESSION, "Closed", p->session,
        NULL, G_DBUS_SIGNAL_FLAGS_NONE, session_closed, p, NULL);
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    /* Keyboard and pointer, plus the touchscreen (4) where the portal offers one (mobile viewers). */
    p->available = remote_property(p, "AvailableDeviceTypes");
    g_variant_builder_add(&options, "{sv}", "types", g_variant_new_uint32(3 | (p->available & 4)));
    guint version = remote_property(p, "version");
    if (version >= 2) {
        g_variant_builder_add(&options, "{sv}", "persist_mode", g_variant_new_uint32(2));
        if (restore && *restore) g_variant_builder_add(&options, "{sv}", "restore_token", g_variant_new_string(restore));
    }

    results = request(p, REMOTE, "SelectDevices", &options, 1, error);
    if (!results) goto done;
    g_variant_unref(results);
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&options, "{sv}", "types", g_variant_new_uint32(1));
    g_variant_builder_add(&options, "{sv}", "multiple", g_variant_new_boolean(FALSE));
    /* Hidden cursor avoids duplicate pointer; supported since ScreenCast v1. */
    results = request(p, SCREEN, "SelectSources", &options, 1, error);
    if (!results) goto done;
    g_variant_unref(results);
    if (has_interface(p, CLIPBOARD)) {
        p->clipboard_owner_subscription=g_dbus_connection_signal_subscribe(p->bus,DEST,CLIPBOARD,
            "SelectionOwnerChanged",PATH,NULL,G_DBUS_SIGNAL_FLAGS_NONE,selection_owner,p,NULL);
        p->clipboard_transfer_subscription=g_dbus_connection_signal_subscribe(p->bus,DEST,CLIPBOARD,
            "SelectionTransfer",PATH,NULL,G_DBUS_SIGNAL_FLAGS_NONE,selection_transfer,p,NULL);
        GVariant *reply=g_dbus_connection_call_sync(p->bus,DEST,PATH,CLIPBOARD,"RequestClipboard",
            g_variant_new("(o@a{sv})",p->session,empty_options()),NULL,G_DBUS_CALL_FLAGS_NONE,3000,NULL,NULL);
        if (reply) g_variant_unref(reply);
    }
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    results = request(p, REMOTE, "Start", &options, 2, error);
    if (!results) goto done;
    gboolean clipboard_enabled=FALSE;
    g_variant_lookup(results,"clipboard_enabled","b",&clipboard_enabled);
    p->clipboard_enabled=clipboard_enabled;
    fprintf(stderr,"Portal clipboard permission: granted=%d\n",clipboard_enabled);
    *error = read_stream_result(p, results);
    if (!*error) {
        const char *next = NULL;
        if (g_variant_lookup(results, "restore_token", "&s", &next) && next && strlen(next) <= 4096)
            p->restore_token = g_strdup(next);
        fprintf(stderr, "Portal persistence: requested=%d restore_attempted=%d token_returned=%d\n",
            version >= 2, restore && *restore, p->restore_token && *p->restore_token);
    }
    if (!*error && p->restore_token && save_token) save_token(p->restore_token, save_data);
    g_variant_unref(results);
    if (*error) goto done;
    p->main_loop = g_main_loop_new(p->context, FALSE);
    if (p->absolute_stream_pixels)
        p->display_subscription = g_dbus_connection_signal_subscribe(p->bus, MUTTER_DISPLAY, MUTTER_DISPLAY,
            "MonitorsChanged", MUTTER_DISPLAY_PATH, NULL, G_DBUS_SIGNAL_FLAGS_NONE, monitors_changed, p, NULL);
    g_main_context_pop_thread_default(p->context);
    if (pthread_create(&p->dbus_thread, NULL, run_dbus, p)) {
        *error = 3; p->cancelled = NULL; p->cancel_data = NULL;
        uu_portal_close(p); return NULL;
    }
    p->dbus_started = 1;
    *error = connect_pipewire(p);
    if (*error) {
        p->cancelled = NULL; p->cancel_data = NULL;
        uu_portal_close(p); return NULL;
    }
    fprintf(stderr, "Portal pointer geometry: frame=%ux%u compositor=%dx%d absolute=%s\n",
        p->width, p->height, p->logical_width, p->logical_height,
        p->absolute_stream_pixels ? "stream-pixels-or-eis" : "compositor-logical");
    *width = p->width; *height = p->height;
    p->cancelled = NULL; p->cancel_data = NULL;
    return p;
done:
    g_main_context_pop_thread_default(p->context);
    p->cancelled = NULL; p->cancel_data = NULL;
    uu_portal_close(p); return NULL;
}
struct uu_portal *uu_portal_open_persist_cancellable(const char *restore,
    void (*save_token)(const char *, void *), void *save_data,
    uu_portal_cancelled_fn cancelled, void *cancel_data,
    int *error, uint32_t *width, uint32_t *height) {
    return open_persist(restore, save_token, save_data, cancelled, cancel_data, error, width, height);
}
struct uu_portal *uu_portal_open_persist(const char *restore, void (*save_token)(const char *, void *),
    void *data, int *error, uint32_t *width, uint32_t *height) {
    return open_persist(restore, save_token, data, NULL, NULL, error, width, height);
}
struct uu_portal *uu_portal_open_restore(const char *restore, int *error, uint32_t *width, uint32_t *height) {
    return uu_portal_open_persist(restore, NULL, NULL, error, width, height);
}
struct uu_portal *uu_portal_open(int *error, uint32_t *width, uint32_t *height) {
    return uu_portal_open_restore(NULL, error, width, height);
}
const char *uu_portal_restore_token(struct uu_portal *p) { return p ? p->restore_token : NULL; }
static gboolean quit_dbus_loop(gpointer data) { g_main_loop_quit(data); return G_SOURCE_REMOVE; }
void uu_portal_close(struct uu_portal *p) {
    if (!p) return;
    atomic_store(&p->closed, 1);
    portal_eis_close(p);
    if (p->bus && p->session) {
        GVariant *reply = g_dbus_connection_call_sync(p->bus, DEST, p->session, SESSION, "Close",
            NULL, NULL, G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
        if (reply) g_variant_unref(reply);
    }
    if (p->clipboard_owner_subscription) g_dbus_connection_signal_unsubscribe(p->bus,p->clipboard_owner_subscription);
    if (p->clipboard_transfer_subscription) g_dbus_connection_signal_unsubscribe(p->bus,p->clipboard_transfer_subscription);
    if (p->closed_subscription) g_dbus_connection_signal_unsubscribe(p->bus, p->closed_subscription);
    if (p->display_subscription) g_dbus_connection_signal_unsubscribe(p->bus, p->display_subscription);
    if (p->dbus_started) {
        /* A queued source also handles close racing with thread startup; an
         * immediate g_main_loop_quit before run would leave run blocking. */
        GSource *quit = g_idle_source_new();
        g_source_set_callback(quit, quit_dbus_loop, p->main_loop, NULL);
        g_source_attach(quit, p->context); g_source_unref(quit);
        g_main_context_wakeup(p->context); pthread_join(p->dbus_thread, NULL);
    } else if (p->context) {
        while (g_main_context_iteration(p->context, FALSE)) {}
    }
    if (p->pw_loop) pw_thread_loop_stop(p->pw_loop);
    if (p->stream) pw_stream_destroy(p->stream);
    if (p->core) pw_core_disconnect(p->core);
    if (p->pw_context) pw_context_destroy(p->pw_context);
    if (p->pw_loop) pw_thread_loop_destroy(p->pw_loop);
    if (p->main_loop) g_main_loop_unref(p->main_loop);
    if (p->context) g_main_context_unref(p->context);
    if (p->bus) g_object_unref(p->bus);
    g_free(p->mapping_id);
    g_free(p->clipboard_text); g_free(p->clipboard_mime); g_free(p->restore_token); free(p->session); free(p->frame);
    pthread_cond_destroy(&p->changed); pthread_mutex_destroy(&p->lock); free(p);
}
void uu_portal_reset_frame(struct uu_portal *p) {
    if (!p) return;
    pthread_mutex_lock(&p->lock);
    p->sequence = 0;
    p->revision++;
    if (p->frame) memset(p->frame, 0, p->frame_length);
    pthread_mutex_unlock(&p->lock);
}
int uu_portal_capture(struct uu_portal *p, uint8_t *frame, size_t length) {
    return uu_portal_capture_since(p, frame, length, NULL);
}
/* A static desktop can stop producing frames indefinitely. Only time out the
 * paused, format-less renegotiation observed after a live scaling change (#25).
 * Called with the frame mutex held; recovery runs on the Rust capture worker. */
static int stream_stalled(struct uu_portal *p) {
    if (p->state != PW_STREAM_STATE_PAUSED || p->format_valid || !p->interrupted_at) return 0;
    gint64 now = g_get_monotonic_time();
    return now - p->interrupted_at >= PORTAL_RENEGOTIATION_TIMEOUT ||
        (p->display_changed_at && p->display_changed_at >= p->interrupted_at - PORTAL_DISPLAY_GRACE &&
         now - MAX(p->interrupted_at, p->display_changed_at) >= PORTAL_DISPLAY_GRACE);
}
int uu_portal_capture_since(struct uu_portal *p, uint8_t *frame, size_t length, uint64_t *revision) {
    if (!p || !frame) return 11;
    pthread_mutex_lock(&p->lock);
    int result = atomic_load(&p->closed) ? 9 : p->error ? p->error : stream_stalled(p) ? 9 :
        !p->sequence ? 10 : length != p->frame_length ? 11 : 0;
    if (!result) {
        if (revision && *revision == p->revision) result = 12;
        else memcpy(frame, p->frame, length);
        if (revision) *revision = p->revision;
    }
    pthread_mutex_unlock(&p->lock); return result;
}
int uu_portal_alive(struct uu_portal *p) {
    if (!p || atomic_load(&p->closed) || atomic_load(&p->ei_failed)) return 0;
    pthread_mutex_lock(&p->lock);
    int alive = !p->error;
    pthread_mutex_unlock(&p->lock);
    return alive;
}
/* Video renegotiation must not block input releases or clipboard traffic. */
int uu_portal_capture_alive(struct uu_portal *p) {
    if (!uu_portal_alive(p)) return 0;
    pthread_mutex_lock(&p->lock);
    int alive = !stream_stalled(p);
    pthread_mutex_unlock(&p->lock);
    return alive;
}
void uu_portal_input_dispatch(struct uu_portal *p) {
    if (p->ei) portal_eis_dispatch(p);
}
static int prepare_input(struct uu_portal *p) {
    if (!uu_portal_alive(p)) return 9;
    /* xdg-desktop-portal 1.21 checks Notify absolute positions against logical
     * stream size, while GNOME 50 forwards them into Mutter's pixel transform.
     * Initialize EIS only when native input is used, leaving capture/uinput
     * independent of EIS support. ConnectToEIS disables all Notify* methods. */
    if (!p->ei && !p->ei_unavailable && p->absolute_stream_pixels &&
        p->logical_width > 0 && p->logical_height > 0 &&
        (p->width != (uint32_t)p->logical_width || p->height != (uint32_t)p->logical_height)) {
        int result = portal_eis_open(p);
        if (result == PORTAL_EIS_UNSUPPORTED) {
            p->ei_unavailable = 1;
            fprintf(stderr, "Portal EIS unavailable; retaining legacy input\n");
            return 0;
        }
        if (result) {
            /* A timeout may have committed ConnectToEIS at the portal. Any
             * failure except explicit lack of support needs a fresh session. */
            atomic_store(&p->ei_failed, 1);
            static atomic_uint failures;
            if (atomic_fetch_add(&failures, 1) < 12)
                fprintf(stderr, "Portal EIS input initialization failed: code=%d\n", result);
            return result;
        }
    }
    return 0;
}
static int notify(struct uu_portal *p, const char *method, GVariant *parameters) {
    if (!uu_portal_alive(p)) { g_variant_ref_sink(parameters); g_variant_unref(parameters); return 9; }
    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_sync(p->bus, DEST, PATH, REMOTE, method, parameters,
        NULL, G_DBUS_CALL_FLAGS_NONE, 3000, NULL, &error);
    if (!reply) {
        static atomic_uint failures = 0;
        if (atomic_fetch_add(&failures, 1) < 12) {
            char *name = error ? g_dbus_error_get_remote_error(error) : NULL;
            fprintf(stderr, "Portal input method failed: method=%s remote_error=%s\n", method, name ? name : "unavailable");
            g_free(name);
        }
        g_clear_error(&error); return 9;
    }
    g_variant_unref(reply); return 0;
}
void uu_portal_logical_size(struct uu_portal *p, uint32_t *width, uint32_t *height) {
    /* Stream size is optional; unknown logical geometry keeps the existing
     * pixel-sized inventory without preventing capture or uinput. */
    int known = p->logical_width > 0 && p->logical_height > 0;
    *width = known ? (uint32_t)p->logical_width : p->width;
    *height = known ? (uint32_t)p->logical_height : p->height;
}
int uu_portal_move(struct uu_portal *p, double x, double y, int relative) {
    if (!p || !isfinite(x) || !isfinite(y)) return 11;
    if (p->logical_width <= 0 || p->logical_height <= 0 || !p->width || !p->height) return 11;
    if (!relative && (x < 0 || y < 0 || x >= p->width || y >= p->height)) return 11;
    int result = prepare_input(p); if (result) return result;
    if (p->ei) return portal_eis_move(p, x, y, relative);
    if (relative) return notify(p, "NotifyPointerMotion", g_variant_new("(o@a{sv}dd)", p->session, empty_options(),
        x * p->logical_width / p->width, y * p->logical_height / p->height));
    return notify(p, "NotifyPointerMotionAbsolute", g_variant_new("(o@a{sv}udd)", p->session, empty_options(), p->node,
        p->absolute_stream_pixels ? x : x * p->logical_width / p->width,
        p->absolute_stream_pixels ? y : y * p->logical_height / p->height));
}
int uu_portal_button(struct uu_portal *p, int button, int pressed) {
    if (!p) return 11;
    int result = prepare_input(p); if (result) return result;
    if (p->ei) return portal_eis_button(p, button, pressed);
    if (button >= 4 && button <= 7) {
        if (!pressed) return 0;
        return notify(p, "NotifyPointerAxisDiscrete", g_variant_new("(o@a{sv}ui)", p->session, empty_options(),
            (guint)(button <= 5 ? 0 : 1), button == 4 || button == 6 ? -1 : 1));
    }
    int code = button == 1 ? 0x110 : button == 2 ? 0x112 : button == 3 ? 0x111 : button == 8 ? 0x113 : button == 9 ? 0x114 : 0;
    if (!code) return 11;
    return notify(p, "NotifyPointerButton", g_variant_new("(o@a{sv}iu)", p->session, empty_options(), code, (guint)!!pressed));
}
/* Smooth scroll in stream pixels (positive scrolls down/right, like a touchpad); `finish` ends the
 * gesture so clients may coast. */
int uu_portal_scroll(struct uu_portal *p, double dx, double dy, int finish) {
    if (!p || !isfinite(dx) || !isfinite(dy)) return 11;
    if (p->logical_width <= 0 || p->logical_height <= 0 || !p->width || !p->height) return 11;
    int result = prepare_input(p); if (result) return result;
    double lx = dx * p->logical_width / p->width, ly = dy * p->logical_height / p->height;
    if (p->ei) return portal_eis_scroll(p, lx, ly, finish);
    GVariantBuilder options; g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    if (finish) g_variant_builder_add(&options, "{sv}", "finish", g_variant_new_boolean(TRUE));
    return notify(p, "NotifyPointerAxis", g_variant_new("(oa{sv}dd)", p->session, &options, lx, ly));
}
/* One finger (slot 0) in stream pixels: phase 0 down, 1 motion, 2 up. */
int uu_portal_touch_supported(struct uu_portal *p) { return p && (p->devices & 4); }
/* The portal offers a touchscreen this session's consent did not grant (restored from before touch). */
int uu_portal_touch_available(struct uu_portal *p) { return p && (p->available & 4); }
int uu_portal_touch(struct uu_portal *p, int phase, double x, double y) {
    if (!uu_portal_touch_supported(p) || phase < 0 || phase > 2) return 11;
    if (phase != 2 && (!isfinite(x) || !isfinite(y) || p->logical_width <= 0 || p->logical_height <= 0 ||
        !p->width || !p->height || x < 0 || y < 0 || x >= p->width || y >= p->height)) return 11;
    int result = prepare_input(p); if (result) return result;
    if (p->ei) return portal_eis_touch(p, phase, x, y);
    if (phase == 2) return notify(p, "NotifyTouchUp", g_variant_new("(o@a{sv}u)", p->session, empty_options(), 0u));
    return notify(p, phase ? "NotifyTouchMotion" : "NotifyTouchDown", g_variant_new("(o@a{sv}uudd)", p->session, empty_options(), p->node, 0u,
        p->absolute_stream_pixels ? x : x * p->logical_width / p->width,
        p->absolute_stream_pixels ? y : y * p->logical_height / p->height));
}
int uu_portal_key(struct uu_portal *p, uint32_t symbol, int keycode, int pressed) {
    if (!p || !symbol || symbol > INT32_MAX) return 11;
    int result = prepare_input(p); if (result) return result;
    if (p->ei) return portal_eis_key(p, keycode, pressed);
    return notify(p, "NotifyKeyboardKeysym", g_variant_new("(o@a{sv}iu)", p->session, empty_options(), (gint)symbol, (guint)!!pressed));
}

/* Logical position of the granted monitor: matches a Mutter logical monitor. */
int uu_portal_stream_position(struct uu_portal *p, int32_t *x, int32_t *y) {
    if (!p || !p->has_position) return 11;
    *x = p->position_x; *y = p->position_y;
    return 0;
}
/* Mutter's monitors as "connector\twidth\theight\tx\ty\tprimary\n" lines: current mode in
 * pixels, logical position. Returns the count, or -1 without Mutter (KDE, no session bus). */
int uu_portal_monitors(char *out, size_t length) {
    if (!out || !length) return -1;
    out[0] = 0;
    GDBusConnection *bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, NULL);
    if (!bus) return -1;
    GVariant *reply = g_dbus_connection_call_sync(bus, MUTTER_DISPLAY, MUTTER_DISPLAY_PATH, MUTTER_DISPLAY,
        "GetCurrentState", NULL, G_VARIANT_TYPE("(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})"),
        G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
    g_object_unref(bus);
    if (!reply) return -1;
    GVariant *monitors = g_variant_get_child_value(reply, 1), *logical = g_variant_get_child_value(reply, 2);
    int count = 0; size_t used = 0;
    for (gsize i = 0; i < g_variant_n_children(logical); i++) {
        gint32 x, y; gdouble scale; guint32 transform; gboolean primary; GVariant *specs, *props;
        g_variant_get_child(logical, i, "(iidub@a(ssss)@a{sv})", &x, &y, &scale, &transform, &primary, &specs, &props);
        const gchar *connector = NULL;
        if (g_variant_n_children(specs)) g_variant_get_child(specs, 0, "(&s&s&s&s)", &connector, NULL, NULL, NULL);
        gint32 width = 0, height = 0;
        for (gsize m = 0; connector && m < g_variant_n_children(monitors); m++) {
            GVariant *monitor = g_variant_get_child_value(monitors, m), *spec = g_variant_get_child_value(monitor, 0),
                *modes = g_variant_get_child_value(monitor, 1);
            const gchar *name = NULL; g_variant_get_child(spec, 0, "&s", &name);
            for (gsize k = 0; name && !strcmp(name, connector) && k < g_variant_n_children(modes); k++) {
                GVariant *mode = g_variant_get_child_value(modes, k), *mode_props = g_variant_get_child_value(mode, 6);
                gboolean current = FALSE;
                if (g_variant_lookup(mode_props, "is-current", "b", &current) && current)
                    g_variant_get_child(mode, 1, "i", &width), g_variant_get_child(mode, 2, "i", &height);
                g_variant_unref(mode_props); g_variant_unref(mode);
            }
            g_variant_unref(modes); g_variant_unref(spec); g_variant_unref(monitor);
        }
        if (transform % 2) { gint32 t = width; width = height; height = t; } /* 90/270, flipped or not */
        if (connector && width > 0 && height > 0) {
            int n = snprintf(out + used, length - used, "%s\t%d\t%d\t%d\t%d\t%d\n", connector, width, height, x, y, primary ? 1 : 0);
            if (n > 0 && (size_t)n < length - used) { used += (size_t)n; count++; }
        }
        g_variant_unref(specs); g_variant_unref(props);
    }
    g_variant_unref(monitors); g_variant_unref(logical); g_variant_unref(reply);
    return count;
}
/* End the compositor session now (screen-share indicator off) while the handle stays valid;
 * captures then report a closed stream until the handle is replaced. */
void uu_portal_end(struct uu_portal *p) {
    if (!p || atomic_exchange(&p->closed, 1)) return;
    if (p->bus && p->session) {
        GVariant *reply = g_dbus_connection_call_sync(p->bus, DEST, p->session, SESSION, "Close",
            NULL, NULL, G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
        if (reply) g_variant_unref(reply);
    }
}
