/* Standalone ScreenCast diagnostic; pixel reads require an explicit NVENC mode.
 * cc -std=gnu11 -Wall -Wextra -O2 contrib/portal-dmabuf-probe.c -o /tmp/portal-dmabuf-probe $(pkg-config --cflags --libs gio-2.0 gio-unix-2.0 libpipewire-0.3)
 * /tmp/portal-dmabuf-probe --self-test  (no portal access)
 * /tmp/portal-dmabuf-probe --authorize  (opens a visible consent dialog)
 * /tmp/portal-dmabuf-probe --authorize-dmabuf-only  (linear DMA-BUF only; consent dialog)
 * Optional EGL mode: add -DPROBE_EGL and $(pkg-config --cflags --libs egl gbm libdrm).
 * /tmp/portal-dmabuf-probe --authorize-egl-dmabuf-only /dev/dri/renderD... (consent dialog)
 * /tmp/portal-dmabuf-probe --authorize-egl-explicit-sync RENDER_NODE (consent dialog)
 * /tmp/portal-dmabuf-probe --authorize-remote-egl-explicit-sync RENDER_NODE (remote desktop consent; no input)
 * /tmp/portal-dmabuf-probe --authorize-egl-acquire-check RENDER_NODE
 *   additionally waits up to 2s on acquire and queries release; never signals.
 * /tmp/portal-dmabuf-probe --authorize-egl-image-check RENDER_NODE
 *   also creates/destroys an unused EGLImage; requires -DPROBE_IMAGE -DPROBE_EGL,
 *   linking c_engine/portal_nvenc_{device,image,egl_attrs,spa}.c and -ldl.
 * /tmp/portal-dmabuf-probe --authorize-cuda-layout-check RENDER_NODE
 *   also registers/queries/unregisters CUDA views, without pixel copying/encoding.
 *   Add -DPROBE_CUDA, c_engine/nvenc_engine.c and -Ivendor/ffnvcodec.
 * /tmp/portal-dmabuf-probe --authorize-nvenc-one-frame RENDER_NODE
 *   same build as CUDA mode; held-frame copy/encode and in-memory FFmpeg decode.
 *   No output file, release signal or requeue. Unknown completion parks the process.
 * /tmp/portal-dmabuf-probe --authorize-nvenc-eight-frames RENDER_NODE
 *   trusted flagless session diagnostic: reuse encoder, release/queue 8 frames.
 *   Same CUDA build; no output file. Not production/revoke/throughput acceptance.
 * /tmp/portal-dmabuf-probe --authorize-nvenc-revoke-check RENDER_NODE
 *   run up to 60s/600 frames; success requires observed Portal Closed after frames.
 * /tmp/portal-dmabuf-probe --authorize-nvenc-measure RENDER_NODE
 *   first-frame decode, 30 warmup frames, 300 measured frames, normal GOP;
 *   reports process CPU and delivered fps, not a production A/B comparison.
 * Metadata-only explicit modes perform no acquire wait, release signal, buffer
 * queue, pixel access or GPU submission. Success does not verify synchronization.
 * EGL modifiers describe import capability on the chosen device, not the compositor's GPU.
 * No restore token, persistence, input, clipboard or pixel mapping.
 */
#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/buffer/meta.h>
#include <time.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>
#include <stdio.h>
#include <string.h>
#include <inttypes.h>
/* This standalone probe's self-test relies on assertions, including their expressions. */
#ifdef NDEBUG
#undef NDEBUG
#endif
#include <assert.h>
#include <fcntl.h>
#include <errno.h>
#ifdef PROBE_EGL
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <xf86drm.h>
#include "portal-acquire-observe.inc"
#include <fcntl.h>
#endif

#if defined(PROBE_CUDA) && !defined(PROBE_IMAGE)
#error PROBE_CUDA requires PROBE_IMAGE
#endif
#ifdef PROBE_IMAGE
#ifndef PROBE_EGL
#error PROBE_IMAGE requires PROBE_EGL
#endif
#include "../c_engine/portal_nvenc_device.h"
#include "../c_engine/portal_nvenc_image.h"
#ifdef PROBE_CUDA
#include "../c_engine/nvenc_engine.h"
#endif
#endif

#define DEST "org.freedesktop.portal.Desktop"
#define PATH "/org/freedesktop/portal/desktop"
#define SCREEN "org.freedesktop.portal.ScreenCast"
#define REMOTE "org.freedesktop.portal.RemoteDesktop"
#define REQUEST "org.freedesktop.portal.Request"
#define SESSION "org.freedesktop.portal.Session"
/* Do not print D-Bus error messages, paths, result dictionaries or PipeWire
 * error strings: they can contain session identifiers or other private data. */
static void dbus_error(const char *stage, const GError *error) {
    const char *domain = !error ? "none" : error->domain == G_DBUS_ERROR ? "g-dbus-error" :
                         error->domain == G_IO_ERROR ? "g-io-error" : "other";
    fprintf(stderr,"D-Bus %s failed: domain=%s code=%d\n",stage,domain,error ? error->code : 0);
}
static char *session_handle(GVariant *results) {
    const char *s=NULL;
    /* Portal specifies a string; accept an object path from older implementations. */
    if (!g_variant_lookup(results,"session_handle","&s",&s) &&
        !g_variant_lookup(results,"session_handle","&o",&s)) return NULL;
    return g_variant_is_object_path(s) ? g_strdup(s) : NULL;
}
struct response { gboolean done; guint code; GVariant *results; };
static void response_cb(GDBusConnection *b, const gchar *s, const gchar *p, const gchar *i,
                        const gchar *n, GVariant *v, gpointer data) {
    (void)b; (void)s; (void)p; (void)i; (void)n;
    struct response *r = data;
    if (!r->done) { g_variant_get(v, "(u@a{sv})", &r->code, &r->results); r->done = TRUE; }
}
enum request_mode { CREATE, SOURCES, START, DEVICES };
struct request_step { const char *interface, *method; enum request_mode mode; };
static const struct request_step screen_steps[]={{SCREEN,"CreateSession",CREATE},{SCREEN,"SelectSources",SOURCES},{SCREEN,"Start",START}};
#ifdef PROBE_EGL
static const struct request_step remote_steps[]={{REMOTE,"CreateSession",CREATE},{REMOTE,"SelectDevices",DEVICES},{SCREEN,"SelectSources",SOURCES},{REMOTE,"Start",START}};
#endif
static GVariant *request_args(enum request_mode mode, const char *session, const char *token) {
    GVariantBuilder opts; g_variant_builder_init(&opts, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&opts, "{sv}", "handle_token", g_variant_new_string(token));
    if (mode == CREATE) {
        char st[80]; snprintf(st, sizeof(st), "dmadiag_session_%u", (unsigned)getpid());
        g_variant_builder_add(&opts, "{sv}", "session_handle_token", g_variant_new_string(st));
    } else if (mode == SOURCES) {
        g_variant_builder_add(&opts, "{sv}", "types", g_variant_new_uint32(1));
        g_variant_builder_add(&opts, "{sv}", "multiple", g_variant_new_boolean(FALSE));
    } else if (mode == DEVICES) g_variant_builder_add(&opts, "{sv}", "types", g_variant_new_uint32(3));
    return mode == CREATE ? g_variant_new("(@a{sv})", g_variant_builder_end(&opts)) :
           mode == START ? g_variant_new("(os@a{sv})", session, "", g_variant_builder_end(&opts)) :
                           g_variant_new("(o@a{sv})", session, g_variant_builder_end(&opts));
}
static gboolean own_request_path(const char *path, const char *prefix) {
    if (!path || !g_str_has_prefix(path,prefix)) return FALSE;
    const char *token=path+strlen(prefix);
    if (!*token) return FALSE;
    for (const char *c=token;*c;c++)
        if (!g_ascii_isalnum(*c) && *c!='_') return FALSE;
    return TRUE;
}
#ifndef PROBE_RESPONSE_TIMEOUT_US
#define PROBE_RESPONSE_TIMEOUT_US (120 * G_TIME_SPAN_SECOND)
#endif
static GVariant *request(GDBusConnection *bus, const char *interface, const char *method, const char *session, enum request_mode mode) {
    static unsigned serial;
    char token[80]; snprintf(token, sizeof(token), "dmadiag_%u_%u", (unsigned)getpid(), ++serial);
    char *sender = g_strdup(g_dbus_connection_get_unique_name(bus) + 1);
    for (char *c = sender; *c; c++) if (*c == '.') *c = '_';
    char *prefix = g_strdup_printf(PATH "/request/%s/", sender); g_free(sender);
    char *path = g_strconcat(prefix, token, NULL);
    struct response response = {0};
    guint sub = g_dbus_connection_signal_subscribe(bus, DEST, REQUEST, "Response", path, NULL,
                                                   G_DBUS_SIGNAL_FLAGS_NONE, response_cb, &response, NULL);
    GVariant *args = request_args(mode, session, token);
    fprintf(stderr,"D-Bus %s.%s: calling\n",interface,method);
    GError *error=NULL;
    GVariant *reply = g_dbus_connection_call_sync(bus, DEST, PATH, interface, method, args,
                         G_VARIANT_TYPE("(o)"), G_DBUS_CALL_FLAGS_NONE, 10000, NULL, &error);
    gboolean valid = FALSE; char *actual=NULL;
    if (reply) { const char *handle; g_variant_get(reply, "(&o)", &handle); actual=g_strdup(handle); valid = !strcmp(actual, path); g_variant_unref(reply);
        fprintf(stderr,"D-Bus %s.%s: reply handle_matches=%d\n",interface,method,valid);
    } else { dbus_error(method,error); g_clear_error(&error); }
    gint64 until = g_get_monotonic_time() + PROBE_RESPONSE_TIMEOUT_US;
    while (valid && !response.done && g_get_monotonic_time() < until) {
        /* Check between dispatches; a single long-running callback cannot be preempted. */
        while (!response.done && g_get_monotonic_time() < until &&
               g_main_context_iteration(NULL, FALSE)) {}
        if (!response.done && g_get_monotonic_time() < until) g_usleep(5000);
    }
    if (valid && response.done) fprintf(stderr,"D-Bus %s.%s: Response code=%u\n",interface,method,response.code);
    else if (valid) fprintf(stderr,"D-Bus %s.%s: Response timed out\n",interface,method);
    if (!response.done || response.code || !valid) {
        /* A failed or timed-out CreateSession may leave an unknown session. */
        if (mode==CREATE)
            fprintf(stderr,"CreateSession failed: session cleanup cannot be guaranteed\n");
        /* A different returned path is not proof that this request owns it,
         * even if it shares our connection namespace. Never close it. */
        if (actual && !valid)
            fprintf(stderr,"D-Bus Request.Close: unexpected handle not closed; cleanup not guaranteed\n");
        if (!own_request_path(path,prefix)) {
            fprintf(stderr,"D-Bus Request.Close: unsafe request namespace; not closed\n");
        } else {
            GError *close_error=NULL;
            GVariant *closed = g_dbus_connection_call_sync(bus, DEST, path, REQUEST, "Close", NULL, NULL,
                                                            G_DBUS_CALL_FLAGS_NONE, 3000, NULL, &close_error);
            if (closed) { fprintf(stderr,"D-Bus Request.Close: ok\n"); g_variant_unref(closed); }
            else { dbus_error("Request.Close",close_error); g_clear_error(&close_error); }
        }
        if (response.results) g_variant_unref(response.results);
        response.results = NULL;
    }
    g_dbus_connection_signal_unsubscribe(bus, sub); g_free(actual); g_free(path); g_free(prefix); return response.results;
}
#define MAX_MODIFIERS 256
struct modifier_format { uint32_t spa; unsigned count; uint64_t mods[MAX_MODIFIERS]; unsigned char external[MAX_MODIFIERS]; };
struct modifier_list { struct modifier_format formats[4]; };
#ifdef PROBE_CUDA
/* Decode validation stays in memory: never log/store encoded or raw pixels. */
static int decode_one_frame(const uint8_t *packet, uint32_t length, uint32_t width, uint32_t height) {
    if (!packet || !length || !width || !height) return -EINVAL;
    GError *error=NULL;
    GSubprocess *decoder=g_subprocess_new(G_SUBPROCESS_FLAGS_STDIN_PIPE | G_SUBPROCESS_FLAGS_STDOUT_PIPE |
        G_SUBPROCESS_FLAGS_STDERR_SILENCE, &error, "ffmpeg", "-v", "error", "-xerror",
        "-f", "h264", "-i", "pipe:0", "-frames:v", "1", "-f", "framemd5", "pipe:1", NULL);
    if (!decoder) { g_clear_error(&error); return -EIO; }
    GBytes *input=g_bytes_new(packet,length), *output=NULL;
    gboolean done=g_subprocess_communicate(decoder,input,NULL,&output,NULL,&error);
    int ok=0;
    if (done && g_subprocess_get_successful(decoder) && output) {
        gsize size=0; const char *data=g_bytes_get_data(output,&size);
        char *report=g_strndup(data,size); char **lines=g_strsplit(report,"\n",-1);
        unsigned dimensions=0,frames=0;
        for (unsigned i=0;lines[i];i++) {
            unsigned stream=999,w=0,h=0;
            if (sscanf(lines[i],"#dimensions %u: %ux%u",&stream,&w,&h)==3 &&
                stream==0 && w==width && h==height) dimensions++;
            if (lines[i][0]!='#' && sscanf(lines[i],"%u,",&stream)==1 && stream==0) frames++;
        }
        ok=dimensions==1 && frames==1;
        g_strfreev(lines); g_free(report);
    }
    g_clear_error(&error); if (output) g_bytes_unref(output);
    g_bytes_unref(input); g_object_unref(decoder);
    return ok ? 0 : -EIO;
}
#endif
struct probe { struct pw_main_loop *loop; struct pw_stream *stream; int done, dma_only, dma_format, explicit_sync; const char *acquire_device; int acquire_fd, release_fd; uint64_t acquire_point, release_point; unsigned fixation_attempts; const struct modifier_list *egl_mods;
    int image_check, cuda_check, encode_check;
    struct pw_buffer *held;
    uint64_t epoch, frame_epoch;
    enum pw_stream_state stream_state;
    int stream_started, stop_reasons, portal_closed;
    gint64 stop_at;
#ifdef PROBE_IMAGE
    struct spa_video_info_raw fixed;
    struct pnspa_snapshot image_frame, source_frame;
    int pixel_fd;
#endif
};
enum { STOP_STATE=1, STOP_FORMAT=2, STOP_REMOVE=4, STOP_TIMEOUT=8, STOP_PORTAL=16 };
static void stop_probe(struct probe *p,int reason) {
    if (!p->stop_at) p->stop_at=g_get_monotonic_time();
    p->stop_reasons|=reason; p->epoch++; p->done=-1;
    pw_main_loop_quit(p->loop);
}
struct portal_watch { struct probe *probe; int closed; };
static void portal_closed_cb(GDBusConnection *bus,const gchar *sender,const gchar *path,
    const gchar *interface,const gchar *signal,GVariant *parameters,gpointer data) {
    (void)bus;(void)sender;(void)path;(void)interface;(void)signal;(void)parameters;
    struct portal_watch *watch=data; watch->closed=1;
    if (watch->probe) { watch->probe->portal_closed=1; stop_probe(watch->probe,STOP_PORTAL); }
}
#ifdef PROBE_CUDA
static void dispatch_portal_events(void *data,uint64_t expirations) {
    (void)data;(void)expirations;
    for (unsigned i=0;i<64 && g_main_context_iteration(NULL,FALSE);i++) {}
}
#endif
static int modifier_supported(const struct modifier_list *list, uint32_t format, uint64_t mod, int *external) {
    if (!list) return mod==0;
    for (unsigned i=0;i<4;i++) if (list->formats[i].spa==format)
        for (unsigned j=0;j<list->formats[i].count;j++) if (list->formats[i].mods[j]==mod) {
            if (external) *external=list->formats[i].external[j];
            return 1;
        }
    return 0;
}
/* Each SPA pixel format has its own driver-queried DRM modifier set. */
static unsigned build_egl_formats(struct spa_pod_builder *b, const struct spa_pod **params, const struct modifier_list *list, uint32_t fps) {
    unsigned count=0;
    for (unsigned i=0;i<4;i++) {
        const struct modifier_format *fmt=&list->formats[i];
        if (!fmt->count) continue;
        struct spa_pod_frame obj, choice;
        spa_pod_builder_push_object(b,&obj,SPA_TYPE_OBJECT_Format,SPA_PARAM_EnumFormat);
        spa_pod_builder_add(b,SPA_FORMAT_mediaType,SPA_POD_Id(SPA_MEDIA_TYPE_video),
            SPA_FORMAT_mediaSubtype,SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
            SPA_FORMAT_VIDEO_format,SPA_POD_Id(fmt->spa),
            SPA_FORMAT_VIDEO_size,SPA_POD_CHOICE_RANGE_Rectangle(&SPA_RECTANGLE(1920,1080),&SPA_RECTANGLE(1,1),&SPA_RECTANGLE(8192,8192)),
            SPA_FORMAT_VIDEO_framerate,SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(fps,1),&SPA_FRACTION(0,1),&SPA_FRACTION(fps,1)),
            SPA_FORMAT_VIDEO_maxFramerate,SPA_POD_Fraction(&SPA_FRACTION(fps,1)),0);
        /* Producer returns the intersection for consumer-side fixation. */
        spa_pod_builder_prop(b,SPA_FORMAT_VIDEO_modifier,SPA_POD_PROP_FLAG_MANDATORY|SPA_POD_PROP_FLAG_DONT_FIXATE);
        spa_pod_builder_push_choice(b,&choice,SPA_CHOICE_Enum,0);
        spa_pod_builder_long(b,(int64_t)fmt->mods[0]); /* choice default, repeated as first alternative */
        for (unsigned j=0;j<fmt->count;j++) spa_pod_builder_long(b,(int64_t)fmt->mods[j]);
        spa_pod_builder_pop(b,&choice); params[count++]=spa_pod_builder_pop(b,&obj);
    }
    return count;
}
static unsigned build_formats(struct spa_pod_builder *b, const struct spa_pod **params, int dma_only) {
    unsigned count=dma_only ? 1 : 2;
    for (unsigned i=0;i<count;i++) {
        struct spa_pod_frame f; spa_pod_builder_push_object(b,&f,SPA_TYPE_OBJECT_Format,SPA_PARAM_EnumFormat);
        spa_pod_builder_add(b,SPA_FORMAT_mediaType,SPA_POD_Id(SPA_MEDIA_TYPE_video),
            SPA_FORMAT_mediaSubtype,SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
            SPA_FORMAT_VIDEO_format,SPA_POD_CHOICE_ENUM_Id(5,SPA_VIDEO_FORMAT_BGRA,SPA_VIDEO_FORMAT_BGRA,SPA_VIDEO_FORMAT_BGRx,SPA_VIDEO_FORMAT_RGBA,SPA_VIDEO_FORMAT_RGBx),
            SPA_FORMAT_VIDEO_size,SPA_POD_CHOICE_RANGE_Rectangle(&SPA_RECTANGLE(1920,1080),&SPA_RECTANGLE(1,1),&SPA_RECTANGLE(8192,8192)),
            SPA_FORMAT_VIDEO_framerate,SPA_POD_CHOICE_RANGE_Fraction(&SPA_FRACTION(60,1),&SPA_FRACTION(0,1),&SPA_FRACTION(60,1)),0);
        if (!i) {
            struct spa_pod_frame choice;
            spa_pod_builder_prop(b,SPA_FORMAT_VIDEO_modifier,SPA_POD_PROP_FLAG_MANDATORY);
            spa_pod_builder_push_choice(b,&choice,SPA_CHOICE_Enum,0);
            spa_pod_builder_long(b,0); spa_pod_builder_long(b,0);
            spa_pod_builder_pop(b,&choice);
        }
        params[i]=spa_pod_builder_pop(b,&f);
    }
    return count;
}
/* Return 1 for an already fixed value, 2 for a choice requiring fixation,
 * 0 when absent, malformed, or outside the advertised driver capabilities. */
static int select_modifier(const struct spa_pod_prop *prop, const struct modifier_list *list,
                           uint32_t format, uint64_t *selected) {
    if (!prop) return 0;
    if (prop->value.type==SPA_TYPE_Long && prop->value.size==sizeof(int64_t)) {
        int64_t value; if (spa_pod_get_long(&prop->value,&value)<0) return 0;
        *selected=(uint64_t)value;
        if (!modifier_supported(list,format,*selected,NULL)) return 0;
        return (prop->flags & SPA_POD_PROP_FLAG_DONT_FIXATE) ? 2 : 1;
    }
    if (prop->value.type!=SPA_TYPE_Choice || prop->value.size<sizeof(struct spa_pod_choice_body)) return 0;
    const struct spa_pod_choice *choice=(const struct spa_pod_choice *)&prop->value;
    if (SPA_POD_CHOICE_VALUE_TYPE(choice)!=SPA_TYPE_Long || SPA_POD_CHOICE_VALUE_SIZE(choice)!=sizeof(int64_t)) return 0;
    uint32_t size=prop->value.size-sizeof(struct spa_pod_choice_body);
    if (!size || size%sizeof(int64_t)) return 0;
    uint32_t count=size/sizeof(int64_t), type=SPA_POD_CHOICE_TYPE(choice);
    /* spa_pod_fixate changes only the choice type, leaving old alternatives
     * in place. For None, SPA specifies that only the first value is valid. */
    if ((type!=SPA_CHOICE_None && type!=SPA_CHOICE_Enum) ||
        (type==SPA_CHOICE_Enum && count<2)) return 0;
    const uint8_t *values=SPA_POD_CHOICE_VALUES(choice);
    for (uint32_t i=type==SPA_CHOICE_Enum ? 1 : 0, end=type==SPA_CHOICE_None ? 1 : count;i<end;i++) {
        int64_t value; memcpy(&value,values+i*sizeof(value),sizeof(value));
        if (modifier_supported(list,format,(uint64_t)value,NULL)) {
            *selected=(uint64_t)value;
            return type==SPA_CHOICE_Enum || (prop->flags & SPA_POD_PROP_FLAG_DONT_FIXATE) ? 2 : 1;
        }
    }
    return 0;
}
static const struct spa_pod *build_buffers(struct spa_pod_builder *b, int modifier) {
    return spa_pod_builder_add_object(b,SPA_TYPE_OBJECT_ParamBuffers,SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers,SPA_POD_CHOICE_RANGE_Int(4,2,8),
        SPA_PARAM_BUFFERS_dataType,SPA_POD_CHOICE_FLAGS_Int(modifier ? (1<<SPA_DATA_DmaBuf) :
                                                ((1<<SPA_DATA_MemFd)|(1<<SPA_DATA_MemPtr))));
}
#if PW_CHECK_VERSION(1, 2, 0)
/* Mutter 51 advertises three blocks and mandatory SyncTimeline metadata. */
static unsigned build_explicit_params(struct spa_pod_builder *b, const struct spa_pod **params) {
    struct spa_pod_frame f;
    spa_pod_builder_push_object(b,&f,SPA_TYPE_OBJECT_ParamBuffers,SPA_PARAM_Buffers);
    spa_pod_builder_add(b,SPA_PARAM_BUFFERS_buffers,SPA_POD_CHOICE_RANGE_Int(4,2,8),
        SPA_PARAM_BUFFERS_blocks,SPA_POD_Int(3),
        SPA_PARAM_BUFFERS_dataType,SPA_POD_CHOICE_FLAGS_Int(1<<SPA_DATA_DmaBuf),0);
    spa_pod_builder_prop(b,SPA_PARAM_BUFFERS_metaType,SPA_POD_PROP_FLAG_MANDATORY);
    spa_pod_builder_int(b,1<<SPA_META_SyncTimeline);
    params[0]=spa_pod_builder_pop(b,&f);
    params[1]=spa_pod_builder_add_object(b,SPA_TYPE_OBJECT_ParamMeta,SPA_PARAM_Meta,
        SPA_PARAM_META_type,SPA_POD_Id(SPA_META_SyncTimeline),
        SPA_PARAM_META_size,SPA_POD_Int(sizeof(struct spa_meta_sync_timeline)));
    return 2;
}
static int validate_explicit_report(const struct spa_buffer *b, int report) {
    if (!b || b->n_datas!=3 || !b->datas || (b->n_metas && !b->metas)) return 0;
    const struct spa_meta_sync_timeline *m=spa_buffer_find_meta_data(b,SPA_META_SyncTimeline,sizeof(*m));
    int types=b->datas[0].type==SPA_DATA_DmaBuf && b->datas[0].fd>=0 &&
        b->datas[1].type==SPA_DATA_SyncObj && b->datas[1].fd>=0 &&
        b->datas[2].type==SPA_DATA_SyncObj && b->datas[2].fd>=0;
    int points=m && m->acquire_point!=0 && m->release_point>m->acquire_point;
    if (report) fprintf(stderr,"PipeWire: explicit blocks=3 types_and_fds_valid=%d timeline_present=%d points_ordered=%d\n",types,!!m,points);
    if (report && m) fprintf(stderr,"PipeWire: timeline flags=0x%08" PRIx32 " unscheduled_release=%d (metadata only)\n",m->flags,!!(m->flags & SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE));
    return types && points;
}
static int validate_explicit(const struct spa_buffer *b) {
    return validate_explicit_report(b,1);
}
#endif
#ifdef PROBE_EGL
static int has_extension(const char *extensions, const char *name) {
    if (!extensions) return 0;
    size_t n=strlen(name); const char *p=extensions;
    while ((p=strstr(p,name))) { if ((p==extensions || p[-1]==' ') && (p[n]==' ' || !p[n])) return 1; p+=n; }
    return 0;
}
static int query_egl_modifiers(const char *device, struct modifier_list *list) {
    static const uint32_t spa[4]={SPA_VIDEO_FORMAT_BGRA,SPA_VIDEO_FORMAT_BGRx,SPA_VIDEO_FORMAT_RGBA,SPA_VIDEO_FORMAT_RGBx};
    static const uint32_t drm[4]={DRM_FORMAT_ARGB8888,DRM_FORMAT_XRGB8888,DRM_FORMAT_ABGR8888,DRM_FORMAT_XBGR8888};
    int result=1, fd=open(device,O_RDWR|O_CLOEXEC), initialized=0;
    struct gbm_device *gbm=NULL; EGLDisplay display=EGL_NO_DISPLAY;
    if (fd<0) { fprintf(stderr,"EGL: render device open failed\n"); return 1; }
    gbm=gbm_create_device(fd); if (!gbm) { fprintf(stderr,"EGL: GBM device creation failed\n"); goto out; }
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display=(PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!get_display) { fprintf(stderr,"EGL: platform display unavailable\n"); goto out; }
    display=get_display(EGL_PLATFORM_GBM_KHR,gbm,NULL);
    if (display==EGL_NO_DISPLAY || !eglInitialize(display,NULL,NULL)) { fprintf(stderr,"EGL: display initialization failed\n"); goto out; }
    initialized=1;
    if (!has_extension(eglQueryString(display,EGL_EXTENSIONS),"EGL_EXT_image_dma_buf_import_modifiers")) {
        fprintf(stderr,"EGL: DMA-BUF modifier query unsupported\n"); goto out;
    }
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query=(PFNEGLQUERYDMABUFMODIFIERSEXTPROC)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    if (!query) { fprintf(stderr,"EGL: modifier query entry point unavailable\n"); goto out; }
    unsigned total=0;
    for (unsigned i=0;i<4;i++) {
        EGLint count=0, got=0; EGLuint64KHR values[MAX_MODIFIERS]; EGLBoolean external[MAX_MODIFIERS];
        list->formats[i].spa=spa[i];
        if (!query(display,(EGLint)drm[i],0,NULL,NULL,&count)) { fprintf(stderr,"EGL: format=%u modifier query failed; excluded\n",spa[i]); continue; }
        if (count<0 || count>MAX_MODIFIERS) { fprintf(stderr,"EGL: format=%u modifier count exceeds limit; excluded\n",spa[i]); continue; }
        if (count && (!query(display,(EGLint)drm[i],count,values,external,&got) || got<0 || got>count)) {
            fprintf(stderr,"EGL: format=%u modifier list query failed; excluded\n",spa[i]); continue;
        }
        for (EGLint j=0;j<got;j++) {
            if (values[j]==DRM_FORMAT_MOD_INVALID || (external[j]!=EGL_TRUE && external[j]!=EGL_FALSE)) continue;
            unsigned n=list->formats[i].count++;
            list->formats[i].mods[n]=values[j]; list->formats[i].external[n]=(external[j]==EGL_TRUE);
        }
        total+=list->formats[i].count;
        fprintf(stderr,"EGL: format=%u usable_modifiers=%u (import support only)\n",spa[i],list->formats[i].count);
    }
    if (!total) fprintf(stderr,"EGL: no queried modifiers available; no Portal request made\n");
    else result=0;
out:
    if (initialized) eglTerminate(display);
    if (gbm) gbm_device_destroy(gbm);
    close(fd); return result;
}
#endif
static int is_dma_buffer(const struct spa_buffer *b) {
    if (!b || !b->n_datas || !b->datas) return 0;
    for (uint32_t i=0;i<b->n_datas;i++) if (b->datas[i].type!=SPA_DATA_DmaBuf || b->datas[i].fd<0) return 0;
    return 1;
}
static void report_buffer(const struct spa_buffer *b) {
    printf("buffer planes=%u header=%d gpu_device=unknown implicit_sync=unknown", b->n_datas, !!spa_buffer_find_meta_data(b, SPA_META_Header, sizeof(struct spa_meta_header)));
#if PW_CHECK_VERSION(1, 2, 0)
    printf(" sync_timeline=%d", !!spa_buffer_find_meta_data(b, SPA_META_SyncTimeline, sizeof(struct spa_meta_sync_timeline)));
#else
    printf(" sync_timeline=unknown");
#endif
    putchar('\n');
    for (uint32_t i=0; i<b->n_datas; i++) {
        const struct spa_data *d=&b->datas[i]; struct stat st;
        printf("plane=%u type=%u stride=%d offset=%u size=%u maxsize=%u", i, d->type,
               d->chunk ? d->chunk->stride : 0, d->chunk ? d->chunk->offset : 0,
               d->chunk ? d->chunk->size : 0, d->maxsize);
        if (d->fd >= 0 && fstat((int)d->fd, &st)==0)
            printf(" fd_fs_device=%u:%u", major(st.st_dev), minor(st.st_dev));
        else printf(" fd_fs_device=unknown");
        putchar('\n');
    }
}
static void process(void *data) {
    struct probe *p=data;
    if (p->held) return;
    struct pw_buffer *b=pw_stream_dequeue_buffer(p->stream);
    if (!b) return;
    if (p->explicit_sync) {
        p->held=b; p->frame_epoch=p->epoch;
#if PW_CHECK_VERSION(1, 2, 0)
        p->done=p->dma_format && validate_explicit_report(b->buffer,p->encode_check!=4) ? 1 : -1;
        if (p->done==1 && p->acquire_device) {
            const struct spa_meta_sync_timeline *m=spa_buffer_find_meta_data(b->buffer,SPA_META_SyncTimeline,sizeof(*m));
            p->acquire_fd=fcntl((int)b->buffer->datas[1].fd,F_DUPFD_CLOEXEC,0);
            p->release_fd=fcntl((int)b->buffer->datas[2].fd,F_DUPFD_CLOEXEC,0);
            p->acquire_point=m->acquire_point; p->release_point=m->release_point;
            if (p->acquire_fd<0 || p->release_fd<0) p->done=-1;
#ifdef PROBE_IMAGE
            if (p->done==1 && p->image_check) {
                if (!pnspa_snapshot(b->buffer,&p->fixed,p->fixed.modifier,&p->image_frame)) p->done=-1;
                else {
                    p->source_frame=p->image_frame;
                    p->pixel_fd=fcntl(p->image_frame.pixel_fd,F_DUPFD_CLOEXEC,0);
                    if (p->pixel_fd<0) p->done=-1;
                    else p->image_frame.pixel_fd=p->pixel_fd;
                }
            }
#endif
        }
#else
        p->done=-1;
#endif
        if (p->encode_check!=4) fprintf(stderr,"PipeWire: buffer held; release/GPU/queue not yet performed; synchronization authority not established by metadata\n");
        pw_main_loop_quit(p->loop); return;
    }
    if (b->buffer) {
        report_buffer(b->buffer);
        if (p->dma_only && (!p->dma_format || !is_dma_buffer(b->buffer))) {
            fprintf(stderr,"PipeWire: rejected buffer: not a negotiated DMA-BUF\n"); p->done=-1;
        } else p->done=1;
    }
    pw_stream_queue_buffer(p->stream,b);
    if (p->done) pw_main_loop_quit(p->loop);
}
static void format(void *data, uint32_t id, const struct spa_pod *param) {
    struct probe *p=data; struct spa_video_info_raw info={0};
    if (id != SPA_PARAM_Format) return;
    p->epoch++;
    if (p->stream_started) stop_probe(p,STOP_FORMAT);
    if (!param) { p->dma_format=0; fprintf(stderr,"PipeWire: format removed\n"); return; }
    if (spa_format_video_raw_parse(param,&info)<0) { fprintf(stderr,"PipeWire: invalid raw video format\n"); if (p->dma_only) pw_main_loop_quit(p->loop); return; }
    int modifier=spa_pod_find_prop(param,NULL,SPA_FORMAT_VIDEO_modifier)!=NULL;
    int external=-1; uint64_t selected=0;
    const struct spa_pod_prop *mp=spa_pod_find_prop(param,NULL,SPA_FORMAT_VIDEO_modifier);
    int selection=select_modifier(mp,p->egl_mods,info.format,&selected);
    p->dma_format=selection==1;
    if (modifier && selection==2) {
        if (++p->fixation_attempts>3) { fprintf(stderr,"PipeWire: modifier fixation did not converge\n"); pw_main_loop_quit(p->loop); return; }
        info.modifier=selected; info.flags=SPA_VIDEO_FLAG_MODIFIER;
        uint8_t storage[1024]; struct spa_pod_builder b=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
        const struct spa_pod *fixed=spa_format_video_raw_build(&b,SPA_PARAM_EnumFormat,&info);
        int rc=pw_stream_update_params(p->stream,&fixed,1);
        fprintf(stderr,"PipeWire: fixating modifier=0x%016" PRIx64 " attempt=%u result=%d (import not tested)\n",selected,p->fixation_attempts,rc);
        if (rc<0) pw_main_loop_quit(p->loop);
        return;
    }
    if ((p->dma_only || modifier) && !p->dma_format) {
        fprintf(stderr,"PipeWire: rejected absent, malformed or unadvertised modifier/format (format=%u type=%u flags=%u size=%u)\n",info.format,mp ? mp->value.type : 0,mp ? mp->flags : 0,mp ? mp->value.size : 0);
        if (mp && mp->value.type==SPA_TYPE_Choice && mp->value.size>=sizeof(struct spa_pod_choice_body)) {
            const struct spa_pod_choice *c=(const struct spa_pod_choice *)&mp->value;
            fprintf(stderr,"PipeWire: modifier choice type=%u child_type=%u child_size=%u payload_bytes=%u\n",
                    SPA_POD_CHOICE_TYPE(c),SPA_POD_CHOICE_VALUE_TYPE(c),SPA_POD_CHOICE_VALUE_SIZE(c),
                    (unsigned)(mp->value.size-sizeof(struct spa_pod_choice_body)));
        }
        pw_main_loop_quit(p->loop); return;
    }
    if (p->encode_check==4) fprintf(stderr,"Measurement format: framerate=%u/%u max_framerate=%u/%u requested_cap=30\n",
        info.framerate.num,info.framerate.denom,info.max_framerate.num,info.max_framerate.denom);
    if (p->dma_format) { info.modifier=selected; modifier_supported(p->egl_mods,info.format,selected,&external); }
#ifdef PROBE_IMAGE
    p->fixed=info;
#endif
    if (p->dma_only) fprintf(stderr,"PipeWire: modifier driver_external_only=%d pixel_import=not_attempted modifier_import=not_yet_tested\n",external);
    printf("format=%u size=%ux%u modifier_present=%d modifier=0x%016" PRIx64 "\n",
           info.format,info.size.width,info.size.height,modifier,info.modifier);
    uint8_t storage[256]; struct spa_pod_builder b=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    const struct spa_pod *params[2]; unsigned count;
#if PW_CHECK_VERSION(1, 2, 0)
    if (p->explicit_sync) count=build_explicit_params(&b,params);
    else
#endif
    { params[0]=build_buffers(&b,modifier); count=1; }
    int rc=pw_stream_update_params(p->stream,params,count);
    fprintf(stderr,"PipeWire: update buffer parameters result=%d\n",rc);
    if (rc<0) pw_main_loop_quit(p->loop);
}
static void state(void *data, enum pw_stream_state old, enum pw_stream_state now, const char *error) {
    (void)error; struct probe *p=data;
    p->stream_state=now;
    if (now==PW_STREAM_STATE_STREAMING) p->stream_started=1;
    if (old==PW_STREAM_STATE_STREAMING && now!=old) stop_probe(p,STOP_STATE);
    fprintf(stderr,"PipeWire: stream state %d -> %d\n",old,now);
    if (now==PW_STREAM_STATE_ERROR) { fprintf(stderr,"PipeWire: stream error (details suppressed)\n"); pw_main_loop_quit(p->loop); }
}
static void remove_buffer(void *data, struct pw_buffer *buffer) {
    struct probe *p=data; p->epoch++;
    if (p->held==buffer) p->held=NULL;
    if (p->stream_started) stop_probe(p,STOP_REMOVE);
}
static const struct pw_stream_events events={ PW_VERSION_STREAM_EVENTS, .state_changed=state, .param_changed=format, .process=process, .remove_buffer=remove_buffer };
static void timeout_cb(void *data, uint64_t expirations) {
    (void)expirations; struct probe *p=data;
    fprintf(stderr,"PipeWire: buffer wait timed out\n"); stop_probe(p,STOP_TIMEOUT);
}
#ifdef PROBE_CUDA
#include "portal-stream-diagnostic.inc"
#endif
static int capture(int fd, uint32_t node, int dma_only, const struct modifier_list *mods, int explicit_sync, const char *acquire_device, int image_check, int cuda_check, int encode_check, struct portal_watch *watch) {
    int result=1; struct probe p={.dma_only=dma_only,.egl_mods=mods,.explicit_sync=explicit_sync,.acquire_device=acquire_device,.acquire_fd=-1,.release_fd=-1,.image_check=image_check,.cuda_check=cuda_check,.encode_check=encode_check};
#ifdef PROBE_IMAGE
    p.pixel_fd=-1;
#endif
    struct pw_context *context=NULL; struct pw_core *core=NULL;
    struct spa_hook listener; struct spa_source *timer=NULL,*control_timer=NULL;
    fprintf(stderr,"PipeWire: mode=%s; creating loop and connecting portal remote\n",mods ? "egl-dmabuf-only" : dma_only ? "linear-dmabuf-only" : "dmabuf-preferred-with-shm-fallback");
    pw_init(NULL,NULL); p.loop=pw_main_loop_new(NULL); if (!p.loop) { fprintf(stderr,"PipeWire: loop creation failed\n"); close(fd); return 1; }
    context=pw_context_new(pw_main_loop_get_loop(p.loop),NULL,0); if (!context) { fprintf(stderr,"PipeWire: context creation failed\n"); close(fd); goto out; }
    core=pw_context_connect_fd(context,fd,NULL,0); if (!core) { fprintf(stderr,"PipeWire: remote connection failed\n"); goto out; }
    p.stream=pw_stream_new(core,"portal-metadata-probe",pw_properties_new(PW_KEY_MEDIA_TYPE,"Video",PW_KEY_MEDIA_CATEGORY,"Capture",PW_KEY_MEDIA_ROLE,"Screen",NULL));
    if (!p.stream) { fprintf(stderr,"PipeWire: stream creation failed\n"); goto out; }
    pw_stream_add_listener(p.stream,&listener,&events,&p);
    uint8_t storage[16384]; struct spa_pod_builder b=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
    const struct spa_pod *params[4];
    unsigned count=mods ? build_egl_formats(&b,params,mods,encode_check==4 ? 30 : 60) : build_formats(&b,params,dma_only);
    if (!count || b.state.offset>sizeof(storage)) { fprintf(stderr,"PipeWire: modifier format construction failed\n"); goto out; }
    int rc=pw_stream_connect(p.stream,PW_DIRECTION_INPUT,node,PW_STREAM_FLAG_AUTOCONNECT,params,count);
    fprintf(stderr,"PipeWire: stream connect result=%d\n",rc); if (rc<0) goto out;
    timer=pw_loop_add_timer(pw_main_loop_get_loop(p.loop),timeout_cb,&p);
    if (!timer) { fprintf(stderr,"PipeWire: timer creation failed\n"); goto out; }
    struct timespec delay={.tv_sec=15}; rc=pw_loop_update_timer(pw_main_loop_get_loop(p.loop),timer,&delay,NULL,false);
    if (rc<0) { fprintf(stderr,"PipeWire: timer setup failed result=%d\n",rc); goto out; }
#ifdef PROBE_CUDA
    if (encode_check>=2) {
        watch->probe=&p;
        if (watch->closed) { p.portal_closed=1; stop_probe(&p,STOP_PORTAL); goto out; }
        control_timer=pw_loop_add_timer(pw_main_loop_get_loop(p.loop),dispatch_portal_events,watch);
        if (!control_timer) goto out;
        struct timespec interval={.tv_nsec=20000000};
        if (pw_loop_update_timer(pw_main_loop_get_loop(p.loop),control_timer,&interval,&interval,false)<0) goto out;
        result=run_stream_diagnostic(&p,timer); goto out;
    }
#endif
    rc=pw_main_loop_run(p.loop); fprintf(stderr,"PipeWire: loop ended result=%d buffer_status=%d\n",rc,p.done); result=rc>=0 && p.done==1 ? 0 : 1;
#ifdef PROBE_EGL
    if (!result && acquire_device) {
        int observed=observe_acquire(acquire_device,p.acquire_fd,p.acquire_point,p.release_fd,p.release_point,2000);
        fprintf(stderr,"DRM: acquire observation result=%d; no signal or GPU submission\n",observed);
        if (observed) result=1;
    }
#endif
#ifdef PROBE_IMAGE
    if (!result && image_check) {
        int render=open(acquire_device,O_RDWR|O_CLOEXEC);
        struct pnegl_device *device=NULL;
        struct pnegl_image *image=calloc(1,sizeof(*image));
        int imported=render<0 ? -errno : !image ? -ENOMEM : pnegl_device_open(&device,render);
        if (render>=0) close(render);
        if (!imported) imported=pnegl_image_create(image,device->display,&p.image_frame,
            device->create_image,device->destroy_image);
        fprintf(stderr,"EGL: real DMA-BUF image create result=%d; no pixel read or GPU submission\n",imported);
        if (imported) result=1;
#ifdef PROBE_CUDA
        if (!imported && cuda_check) {
            NvencConfig config={.width=p.image_frame.width,.height=p.image_frame.height,
                .fps=30,.gop=30,.cq=23,.target_bps=2000000,.max_bps=2000000,.buffer_bits=2000000};
            NvencEngine *encoder=nvenc_create(&config);
            if (!encoder) result=1;
            else {
                int complete=0;
                int inspected=nvenc_inspect_egl(encoder,image->image,pnegl_device_pci(device),&complete);
                fprintf(stderr,"CUDA: real EGL layout inspect result=%d views_complete=%d; no pixel copy or encoding\n",inspected,complete);
                if (!complete) {
                    fprintf(stderr,"CUDA: retaining encoder/image/device/buffer after uncertain cleanup\n");
                    return 1;
                }
                if (!inspected && encode_check) {
                    // One held frame only. No release signal, buffer queue or
                    // promise retirement: the producer is never invited to reuse.
                    int observed=observe_acquire(acquire_device,p.acquire_fd,p.acquire_point,p.release_fd,p.release_point,2000);
                    if (observed) result=1;
                    else {
                        const uint8_t *packet=NULL; uint32_t length=0; int source_complete=0;
                        int encoded=nvenc_encode_egl(encoder,image->image,pnegl_device_pci(device),1,
                            &packet,&length,&source_complete);
                        fprintf(stderr,"NVENC: held-frame encode result=%d source_complete=%d packet_bytes=%u\n",
                            encoded,source_complete,length);
                        if (encoded || !source_complete) {
                            // Source completion alone does not clear engine quarantine.
                            // Do not free dependencies or claim process-exit isolation.
                            fprintf(stderr,"NVENC: uncertain completion; probe quarantined with all owners retained\n");
                            fflush(stderr);
                            /* Keep the process and its resources alive. No timeout,
                             * destructor or Session.Close is a completion proof. */
                            for (;;) g_usleep(G_USEC_PER_SEC);
                        }
                        int decoded=decode_one_frame(packet,length,p.image_frame.width,p.image_frame.height);
                        fprintf(stderr,"Decode: one frame dimensions validated=%d; contents not compared or saved\n",decoded==0);
                        if (decoded) result=1;
                    }
                }
                nvenc_destroy(encoder);
                if (inspected) result=1;
            }
        }
#endif
        if (image && image->image) {
            int destroyed=pnegl_image_destroy_completed(image,1); /* Every submitted view/access completed successfully above. */
            fprintf(stderr,"EGL: completed image destroy result=%d\n",destroyed);
            if (destroyed) {
                /* Never retry or free dependencies after failed destruction.
                 * Retain for this diagnostic process's remaining lifetime. */
                fprintf(stderr,"EGL: retaining image/device/buffer owners after destroy failure\n");
                return 1;
            }
        }
        free(image);
        if (device) {
            int closed=pnegl_device_close_idle(&device,1);
            fprintf(stderr,"EGL: idle device close result=%d\n",closed);
            if (closed) return 1;
        }
    }
#endif
out:
    watch->probe=NULL;
    if (control_timer) pw_loop_destroy_source(pw_main_loop_get_loop(p.loop),control_timer);
#ifdef PROBE_IMAGE
    if (p.pixel_fd>=0) close(p.pixel_fd);
#endif
    if (p.acquire_fd>=0) close(p.acquire_fd);
    if (p.release_fd>=0) close(p.release_fd);
    if (timer) pw_loop_destroy_source(pw_main_loop_get_loop(p.loop),timer);
    if (p.stream) pw_stream_destroy(p.stream);
    if (core) pw_core_disconnect(core);
    if (context) pw_context_destroy(context);
    pw_main_loop_destroy(p.loop); pw_deinit(); return result;
}
int main(int argc,char **argv) {
    if (argc==2 && !strcmp(argv[1],"--self-test")) {
        {
            const char *prefix=PATH "/request/1_42/";
            assert(own_request_path(PATH "/request/1_42/test_123",prefix));
            assert(!own_request_path(PATH "/request/1_43/test_123",prefix));
            assert(!own_request_path(PATH "/request/1_42/",prefix));
            assert(!own_request_path(PATH "/request/1_42/test/other",prefix));
            assert(!own_request_path(PATH "/request/1_42/bad-token",prefix));
#ifdef PROBE_EGL
            const struct request_step *steps=remote_steps;
            assert(G_N_ELEMENTS(remote_steps)==4);
            assert(!strcmp(steps[0].interface,REMOTE) && !strcmp(steps[0].method,"CreateSession") && steps[0].mode==CREATE);
            assert(!strcmp(steps[1].interface,REMOTE) && !strcmp(steps[1].method,"SelectDevices") && steps[1].mode==DEVICES);
            assert(!strcmp(steps[2].interface,SCREEN) && !strcmp(steps[2].method,"SelectSources") && steps[2].mode==SOURCES);
            assert(!strcmp(steps[3].interface,REMOTE) && !strcmp(steps[3].method,"Start") && steps[3].mode==START);
#else
            const struct request_step *steps=screen_steps;
#endif
            (void)steps;
            for (int mode=CREATE;mode<=DEVICES;mode++) {
                GVariant *args=g_variant_ref_sink(request_args(mode,"/test/session","test_request"));
                const char *signature=mode==CREATE ? "(a{sv})" : mode==START ? "(osa{sv})" : "(oa{sv})";
                assert(g_variant_is_of_type(args,G_VARIANT_TYPE(signature)));
                if (mode!=CREATE) { const char *handle; g_variant_get_child(args,0,"&o",&handle); assert(!strcmp(handle,"/test/session")); }
                if (mode==START) { const char *parent; g_variant_get_child(args,1,"&s",&parent); assert(!*parent); }
                GVariant *opts=g_variant_get_child_value(args,mode==CREATE ? 0 : mode==START ? 2 : 1);
                const char *token=NULL; guint32 types=0; gboolean multiple=TRUE;
                assert(g_variant_lookup(opts,"handle_token","&s",&token) && !strcmp(token,"test_request"));
                assert(!g_variant_lookup_value(opts,"restore_token",NULL) && !g_variant_lookup_value(opts,"persist_mode",NULL));
                assert(g_variant_n_children(opts)==(mode==SOURCES ? 3u : mode==START ? 1u : 2u));
                if (mode==CREATE) assert(g_variant_lookup(opts,"session_handle_token","&s",&token) && *token);
                if (mode==SOURCES || mode==DEVICES) assert(g_variant_lookup(opts,"types","u",&types) && types==(mode==SOURCES ? 1u : 3u));
                if (mode==SOURCES) assert(g_variant_lookup(opts,"multiple","b",&multiple) && !multiple);
                g_variant_unref(opts); g_variant_unref(args);
            }
        }
        struct spa_chunk chunk={.stride=128,.size=256};
        struct spa_data d={.type=SPA_DATA_DmaBuf,.fd=-1,.chunk=&chunk};
        struct spa_buffer b={.n_datas=1,.datas=&d}; report_buffer(&b);
        for (int i=0;i<3;i++) {
            GVariantBuilder opts; g_variant_builder_init(&opts,G_VARIANT_TYPE_VARDICT);
            if (i<2) g_variant_builder_add(&opts,"{sv}","session_handle",i ? g_variant_new_object_path("/test/session") : g_variant_new_string("/test/session"));
            else g_variant_builder_add(&opts,"{sv}","session_handle",g_variant_new_string("invalid"));
            GVariant *v=g_variant_ref_sink(g_variant_builder_end(&opts));
            char *s=session_handle(v); assert(i==2 ? s==NULL : s && !strcmp(s,"/test/session"));
            g_free(s); g_variant_unref(v);
        }
        for (int only=0;only<2;only++) {
            uint8_t storage[2048]; struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            const struct spa_pod *params[2]={0}; unsigned count=build_formats(&builder,params,only);
            assert(count==(only ? 1u : 2u));
            const struct spa_pod_prop *prop=spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_modifier);
            assert(prop && (prop->flags & SPA_POD_PROP_FLAG_MANDATORY) && prop->value.type==SPA_TYPE_Choice);
            const struct spa_pod_choice *modchoice=(const struct spa_pod_choice *)&prop->value;
            assert(SPA_POD_CHOICE_TYPE(modchoice)==SPA_CHOICE_Enum && SPA_POD_CHOICE_N_VALUES(modchoice)==2);
            int64_t modifier=-1; memcpy(&modifier,SPA_POD_CHOICE_VALUES(modchoice),sizeof(modifier)); assert(modifier==0);
            if (!only) assert(!spa_pod_find_prop(params[1],NULL,SPA_FORMAT_VIDEO_modifier));
            builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            const struct spa_pod *buffers=build_buffers(&builder,1);
            prop=spa_pod_find_prop(buffers,NULL,SPA_PARAM_BUFFERS_dataType);
            assert(prop && prop->value.type==SPA_TYPE_Choice);
            const struct spa_pod_choice *choice=(const struct spa_pod_choice *)&prop->value;
            assert(SPA_POD_CHOICE_TYPE(choice)==SPA_CHOICE_Flags);
            int32_t mask=0; memcpy(&mask,SPA_POD_CHOICE_VALUES(choice),sizeof(mask));
            assert(mask==(1<<SPA_DATA_DmaBuf));
        }
        {
            struct modifier_list list={0}; list.formats[0].spa=SPA_VIDEO_FORMAT_BGRA;
            list.formats[0].count=1;
            uint8_t storage[1024]; struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            const struct spa_pod *params[4]={0};
            assert(build_egl_formats(&builder,params,&list,30)==1);
            const struct spa_pod_prop *rate=spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_maxFramerate);
            struct spa_fraction max={0};
            assert(rate && !spa_pod_get_fraction(&rate->value,&max) && max.num==30 && max.denom==1);
            rate=spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_framerate);
            assert(rate && rate->value.type==SPA_TYPE_Choice);
            const struct spa_pod_choice *choice=(const struct spa_pod_choice *)&rate->value;
            assert(SPA_POD_CHOICE_TYPE(choice)==SPA_CHOICE_Range && SPA_POD_CHOICE_N_VALUES(choice)==3);
            struct spa_fraction rates[3]; memcpy(rates,SPA_POD_CHOICE_VALUES(choice),sizeof(rates));
            assert(rates[0].num==30 && rates[2].num==30 && rates[2].denom==1);
        }
        {
            struct modifier_list list={0}; list.formats[0].spa=SPA_VIDEO_FORMAT_BGRA;
            list.formats[0].count=2; list.formats[0].mods[0]=0; list.formats[0].mods[1]=0x1234;
            list.formats[0].external[1]=1;
            uint8_t storage[16384]; struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            const struct spa_pod *params[4]={0}; assert(build_egl_formats(&builder,params,&list,60)==1);
            const struct spa_pod_prop *prop=spa_pod_find_prop(params[0],NULL,SPA_FORMAT_VIDEO_modifier);
            assert(prop && (prop->flags & SPA_POD_PROP_FLAG_MANDATORY) && (prop->flags & SPA_POD_PROP_FLAG_DONT_FIXATE) && prop->value.type==SPA_TYPE_Choice);
            const struct spa_pod_choice *choice=(const struct spa_pod_choice *)&prop->value;
            assert(SPA_POD_CHOICE_TYPE(choice)==SPA_CHOICE_Enum && SPA_POD_CHOICE_N_VALUES(choice)==3);
            uint64_t selected=42; assert(select_modifier(prop,&list,SPA_VIDEO_FORMAT_BGRA,&selected)==2 && selected==0);
            assert(!select_modifier(prop,&list,SPA_VIDEO_FORMAT_RGBA,&selected));
            builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            struct spa_video_info_raw raw=SPA_VIDEO_INFO_RAW_INIT(.format=SPA_VIDEO_FORMAT_BGRA,.flags=SPA_VIDEO_FLAG_MODIFIER,.modifier=0x1234);
            const struct spa_pod *fixed=spa_format_video_raw_build(&builder,SPA_PARAM_Format,&raw);
            prop=spa_pod_find_prop(fixed,NULL,SPA_FORMAT_VIDEO_modifier);
            assert(select_modifier(prop,&list,SPA_VIDEO_FORMAT_BGRA,&selected)==1 && selected==0x1234);
            builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            struct spa_pod_frame obj, ch; spa_pod_builder_push_object(&builder,&obj,SPA_TYPE_OBJECT_Format,SPA_PARAM_Format);
            spa_pod_builder_prop(&builder,SPA_FORMAT_VIDEO_modifier,0);
            spa_pod_builder_push_choice(&builder,&ch,SPA_CHOICE_None,0); spa_pod_builder_long(&builder,0x1234); spa_pod_builder_pop(&builder,&ch);
            fixed=spa_pod_builder_pop(&builder,&obj); prop=spa_pod_find_prop(fixed,NULL,SPA_FORMAT_VIDEO_modifier);
            assert(select_modifier(prop,&list,SPA_VIDEO_FORMAT_BGRA,&selected)==1 && selected==0x1234);
            /* Real spa_pod_fixate retains the original choice payload. */
            builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            spa_pod_builder_push_object(&builder,&obj,SPA_TYPE_OBJECT_Format,SPA_PARAM_Format);
            spa_pod_builder_prop(&builder,SPA_FORMAT_VIDEO_modifier,SPA_POD_PROP_FLAG_MANDATORY);
            spa_pod_builder_push_choice(&builder,&ch,SPA_CHOICE_Enum,0);
            spa_pod_builder_long(&builder,0x1234); spa_pod_builder_long(&builder,0x1234); spa_pod_builder_long(&builder,0x9876);
            spa_pod_builder_pop(&builder,&ch); struct spa_pod *fixatable=spa_pod_builder_pop(&builder,&obj);
            assert(spa_pod_fixate(fixatable)==0);
            prop=spa_pod_find_prop(fixatable,NULL,SPA_FORMAT_VIDEO_modifier);
            assert(SPA_POD_CHOICE_TYPE((const struct spa_pod_choice *)&prop->value)==SPA_CHOICE_None);
            assert(SPA_POD_CHOICE_N_VALUES((const struct spa_pod_choice *)&prop->value)==3);
            assert(select_modifier(prop,&list,SPA_VIDEO_FORMAT_BGRA,&selected)==1 && selected==0x1234);
            /* Ignore trailing supported values when the fixed first value is unsupported. */
            builder=(struct spa_pod_builder)SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            spa_pod_builder_push_object(&builder,&obj,SPA_TYPE_OBJECT_Format,SPA_PARAM_Format);
            spa_pod_builder_prop(&builder,SPA_FORMAT_VIDEO_modifier,0);
            spa_pod_builder_push_choice(&builder,&ch,SPA_CHOICE_None,0);
            spa_pod_builder_long(&builder,0x9876); spa_pod_builder_long(&builder,0x1234);
            spa_pod_builder_pop(&builder,&ch); fixed=spa_pod_builder_pop(&builder,&obj);
            prop=spa_pod_find_prop(fixed,NULL,SPA_FORMAT_VIDEO_modifier);
            assert(!select_modifier(prop,&list,SPA_VIDEO_FORMAT_BGRA,&selected));
            int ext=-1; assert(modifier_supported(&list,SPA_VIDEO_FORMAT_BGRA,0x1234,&ext) && ext==1);
            assert(!modifier_supported(&list,SPA_VIDEO_FORMAT_RGBA,0x1234,NULL));
            assert(!modifier_supported(&list,SPA_VIDEO_FORMAT_BGRA,0x4321,NULL));
        }
#if PW_CHECK_VERSION(1, 2, 0)
        {
            uint8_t storage[512]; struct spa_pod_builder builder=SPA_POD_BUILDER_INIT(storage,sizeof(storage));
            const struct spa_pod *params[2]; assert(build_explicit_params(&builder,params)==2);
            const struct spa_pod_prop *prop=spa_pod_find_prop(params[0],NULL,SPA_PARAM_BUFFERS_metaType);
            int32_t value=0; assert(prop && (prop->flags & SPA_POD_PROP_FLAG_MANDATORY));
            assert(!spa_pod_get_int(&prop->value,&value) && value==(1<<SPA_META_SyncTimeline));
            prop=spa_pod_find_prop(params[0],NULL,SPA_PARAM_BUFFERS_blocks);
            assert(prop && !spa_pod_get_int(&prop->value,&value) && value==3);
            prop=spa_pod_find_prop(params[1],NULL,SPA_PARAM_META_size);
            assert(prop && !spa_pod_get_int(&prop->value,&value) && value==(int)sizeof(struct spa_meta_sync_timeline));
            struct spa_data data[3]={{.type=SPA_DATA_DmaBuf,.fd=1},{.type=SPA_DATA_SyncObj,.fd=1},{.type=SPA_DATA_SyncObj,.fd=1}};
            struct spa_meta_sync_timeline timeline={.acquire_point=1,.release_point=2};
            struct spa_meta meta={.type=SPA_META_SyncTimeline,.size=sizeof(timeline),.data=&timeline};
            struct spa_buffer buf={.n_datas=3,.datas=data,.n_metas=1,.metas=&meta};
            assert(validate_explicit(&buf) && timeline.flags==0);
            timeline.flags=SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE|2;
            assert(validate_explicit(&buf) && timeline.flags==(SPA_META_SYNC_TIMELINE_UNSCHEDULED_RELEASE|2));
            data[2].type=SPA_DATA_MemFd; assert(!validate_explicit(&buf)); data[2].type=SPA_DATA_SyncObj;
            data[1].fd=-1; assert(!validate_explicit(&buf)); data[1].fd=1;
            timeline.release_point=0; assert(!validate_explicit(&buf));
            timeline.release_point=1; assert(!validate_explicit(&buf));
            timeline.acquire_point=3; timeline.release_point=2; assert(!validate_explicit(&buf));
            timeline.acquire_point=0; assert(!validate_explicit(&buf)); timeline.acquire_point=1;
            meta.size--; assert(!validate_explicit(&buf)); meta.size++;
            buf.n_datas=1; assert(!validate_explicit(&buf));
        }
#endif
        d.fd=1; assert(is_dma_buffer(&b)); d.type=SPA_DATA_MemFd; assert(!is_dma_buffer(&b));
        d.type=SPA_DATA_DmaBuf; d.fd=-1; assert(!is_dma_buffer(&b));
        fprintf(stderr,"Self-test: local synthetic construction and validation passed; no D-Bus end-to-end test\n"); return 0;
    }
    int remote_mode=0;
#ifdef PROBE_EGL
    remote_mode=argc==3 && !strcmp(argv[1],"--authorize-remote-egl-explicit-sync");
#endif
    int encode_check=argc==3 && !strcmp(argv[1],"--authorize-nvenc-one-frame");
    if (argc==3 && !strcmp(argv[1],"--authorize-nvenc-eight-frames")) encode_check=2;
    if (argc==3 && !strcmp(argv[1],"--authorize-nvenc-revoke-check")) encode_check=3;
    if (argc==3 && !strcmp(argv[1],"--authorize-nvenc-measure")) encode_check=4;
    int cuda_check=encode_check || (argc==3 && !strcmp(argv[1],"--authorize-cuda-layout-check"));
    if (encode_check) {
        char *decoder=g_find_program_in_path("ffmpeg");
        if (!decoder) { fprintf(stderr,"Decoder unavailable / 缺少 ffmpeg 解码校验程序\n"); return 2; }
        g_free(decoder);
    }
#ifndef PROBE_CUDA
    if (cuda_check) { fprintf(stderr,"CUDA mode unavailable / 请启用 PROBE_CUDA 重新编译\n"); return 2; }
#endif
    int image_check=cuda_check || (argc==3 && !strcmp(argv[1],"--authorize-egl-image-check"));
#ifndef PROBE_IMAGE
    if (image_check) { fprintf(stderr,"Image mode unavailable / 请启用 PROBE_IMAGE 重新编译\n"); return 2; }
#endif
    int acquire_check=image_check || (argc==3 && !strcmp(argv[1],"--authorize-egl-acquire-check"));
    int explicit_sync=acquire_check || remote_mode || (argc==3 && !strcmp(argv[1],"--authorize-egl-explicit-sync"));
    int egl_mode=explicit_sync || (argc==3 && !strcmp(argv[1],"--authorize-egl-dmabuf-only"));
#if !PW_CHECK_VERSION(1, 2, 0)
    if (explicit_sync) { fprintf(stderr,"Explicit sync requires PipeWire headers >= 1.2 / 需要 PipeWire 1.2 或更新的头文件\n"); return 1; }
#endif
    if (!egl_mode && (argc!=2 || (strcmp(argv[1],"--authorize") && strcmp(argv[1],"--authorize-dmabuf-only")))) {
        fprintf(stderr,"Usage / 用法: %s --self-test | --authorize | --authorize-dmabuf-only | --authorize-egl-dmabuf-only RENDER_NODE | --authorize-egl-explicit-sync RENDER_NODE | --authorize-remote-egl-explicit-sync RENDER_NODE | --authorize-egl-acquire-check RENDER_NODE | --authorize-egl-image-check RENDER_NODE | --authorize-cuda-layout-check RENDER_NODE | --authorize-nvenc-one-frame RENDER_NODE | --authorize-nvenc-eight-frames RENDER_NODE | --authorize-nvenc-revoke-check RENDER_NODE | --authorize-nvenc-measure RENDER_NODE (visible Portal dialog / 显示授权窗口)\n",argv[0]); return 2;
    }
    struct modifier_list mods={0};
    if (remote_mode) fprintf(stderr,"WARNING: RemoteDesktop requests keyboard and pointer permissions (types=3); this probe sends no input.\n");
    if (explicit_sync) fprintf(stderr,"Explicit diagnostic: acquire_check=%d encode_mode=%d release_and_queue=%d\n",acquire_check,encode_check,encode_check>=2);
    if (egl_mode) {
#ifdef PROBE_EGL
        if (query_egl_modifiers(argv[2],&mods)) return 1;
#else
        fprintf(stderr,"EGL modifier mode unavailable: rebuild with PROBE_EGL / 请启用 PROBE_EGL 重新编译\n"); return 1;
#endif
    }
    int dma_only=egl_mode || !strcmp(argv[1],"--authorize-dmabuf-only");
    int status=1; char *session=NULL; GVariant *v=NULL; GError *error=NULL;
    struct portal_watch watch={0}; guint closed_subscription=0;
    fprintf(stderr,"D-Bus session bus: connecting\n");
    GDBusConnection *bus=g_bus_get_sync(G_BUS_TYPE_SESSION,NULL,&error);
    if (!bus) { dbus_error("session bus",error); g_clear_error(&error); return 1; }
    fprintf(stderr,"D-Bus session bus: connected\n");
    const struct request_step *steps=screen_steps; size_t nsteps=G_N_ELEMENTS(screen_steps);
#ifdef PROBE_EGL
    if (remote_mode) { steps=remote_steps; nsteps=G_N_ELEMENTS(remote_steps); }
#endif
    for (size_t i=0;i<nsteps;i++) {
        v=request(bus,steps[i].interface,steps[i].method,session,steps[i].mode); if (!v) goto out;
        if (steps[i].mode==CREATE) {
            session=session_handle(v);
            fprintf(stderr,"D-Bus CreateSession: valid_session_handle=%d\n",session!=NULL);
            if (!session) goto out;
            closed_subscription=g_dbus_connection_signal_subscribe(bus,DEST,SESSION,"Closed",session,NULL,
                G_DBUS_SIGNAL_FLAGS_NONE,portal_closed_cb,&watch,NULL);
        }
        if (steps[i].mode!=START) { g_variant_unref(v); v=NULL; }
    }
    GVariant *streams=g_variant_lookup_value(v,"streams",G_VARIANT_TYPE("a(ua{sv})"));
    fprintf(stderr,"D-Bus Start: stream_count=%zu\n",streams ? (size_t)g_variant_n_children(streams) : 0);
    if (!streams || g_variant_n_children(streams)!=1) { if (streams) g_variant_unref(streams); goto out; }
    GVariant *entry=g_variant_get_child_value(streams,0), *props=NULL; guint32 node;
    g_variant_get(entry,"(u@a{sv})",&node,&props); g_variant_unref(props); g_variant_unref(entry); g_variant_unref(streams);
    g_variant_unref(v); v=NULL;
    GUnixFDList *fds=NULL; GVariantBuilder opts; g_variant_builder_init(&opts,G_VARIANT_TYPE_VARDICT);
    fprintf(stderr,"D-Bus ScreenCast.OpenPipeWireRemote: calling\n");
    v=g_dbus_connection_call_with_unix_fd_list_sync(bus,DEST,PATH,SCREEN,"OpenPipeWireRemote",
        g_variant_new("(o@a{sv})",session,g_variant_builder_end(&opts)),G_VARIANT_TYPE("(h)"),
        G_DBUS_CALL_FLAGS_NONE,10000,NULL,&fds,NULL,&error);
    if (!v || !fds) { if (!v) { dbus_error("OpenPipeWireRemote",error); g_clear_error(&error); }
        else fprintf(stderr,"D-Bus OpenPipeWireRemote: missing fd list\n");
        if (fds) g_object_unref(fds);
        goto out; }
    gint32 idx; g_variant_get(v,"(h)",&idx); int fd=g_unix_fd_list_get(fds,idx,&error); g_object_unref(fds);
    if (fd>=0) { fprintf(stderr,"D-Bus OpenPipeWireRemote: fd received\n"); status=capture(fd,node,dma_only,egl_mode ? &mods : NULL,explicit_sync,acquire_check ? argv[2] : NULL,image_check,cuda_check,encode_check,&watch); }
    else { dbus_error("OpenPipeWireRemote fd",error); g_clear_error(&error); }
out:
    if (closed_subscription) g_dbus_connection_signal_unsubscribe(bus,closed_subscription);
    if (v) g_variant_unref(v);
    if (session && watch.closed) { fprintf(stderr,"D-Bus Session.Close skipped: Closed already observed\n"); g_free(session); session=NULL; }
    if (session) { fprintf(stderr,"D-Bus Session.Close: calling\n");
        GVariant *r=g_dbus_connection_call_sync(bus,DEST,session,SESSION,"Close",NULL,NULL,G_DBUS_CALL_FLAGS_NONE,3000,NULL,&error);
        if (r) { fprintf(stderr,"D-Bus Session.Close: ok\n"); g_variant_unref(r); }
        else { dbus_error("Session.Close",error); g_clear_error(&error); status=1; } g_free(session); }
    g_object_unref(bus);
    if (explicit_sync) fprintf(stderr,"Producer session termination and universal timeline exclusivity are not proven\n");
    if (status) fprintf(stderr,"Portal metadata probe failed or timed out\n");
    return status;
}
