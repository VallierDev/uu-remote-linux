/* Included after uu_portal definition. One Clipboard permission shares RemoteDesktop. */
#include <fcntl.h>
#include <poll.h>
#include <errno.h>
#include <signal.h>
#define CLIPBOARD "org.freedesktop.portal.Clipboard"
#define CLIP_LIMIT (30 * 1024)
static int text_mime(const char *mime) {
    return !strcmp(mime,"text/plain;charset=utf-8") || !strcmp(mime,"text/plain") || !strcmp(mime,"UTF8_STRING");
}
static int clipboard_fd(struct uu_portal *p, const char *method, GVariant *args) {
    GUnixFDList *fds=NULL;
    GVariant *reply=g_dbus_connection_call_with_unix_fd_list_sync(p->bus,DEST,PATH,CLIPBOARD,method,
        args,G_VARIANT_TYPE("(h)"),G_DBUS_CALL_FLAGS_NONE,2000,NULL,&fds,NULL,NULL);
    int fd=-1;
    if (reply && fds) { gint index; g_variant_get(reply,"(h)",&index); fd=g_unix_fd_list_get(fds,index,NULL); }
    if (reply) g_variant_unref(reply);
    if (fds) g_object_unref(fds);
    if (fd>=0 && fcntl(fd,F_SETFL,fcntl(fd,F_GETFL)|O_NONBLOCK)<0) { close(fd); return -1; }
    return fd;
}
static int clipboard_io(int fd, uint8_t *bytes, size_t capacity, int writing) {
    struct timespec started,now; clock_gettime(CLOCK_MONOTONIC,&started);
    size_t used=0;
    sigset_t blocked,previous,pending; sigemptyset(&blocked); sigaddset(&blocked,SIGPIPE);
    sigpending(&pending); int had_pipe=sigismember(&pending,SIGPIPE);
    if (writing) pthread_sigmask(SIG_BLOCK,&blocked,&previous);
    int result=-1,broken=0;
    for (;;) {
        if (writing && used==capacity) { result=(int)used; break; }
        clock_gettime(CLOCK_MONOTONIC,&now);
        long elapsed=(now.tv_sec-started.tv_sec)*1000+(now.tv_nsec-started.tv_nsec)/1000000;
        if (elapsed>=2000) break;
        struct pollfd item={.fd=fd,.events=writing?POLLOUT:POLLIN};
        int polled=poll(&item,1,(int)(2000-elapsed));
        if (polled<0 && errno==EINTR) continue;
        if (polled<=0 || (item.revents&POLLNVAL)) break;
        ssize_t count=writing?write(fd,bytes+used,capacity-used):read(fd,bytes+used,capacity-used);
        if (count<0 && (errno==EINTR || errno==EAGAIN)) continue;
        if (count<0) { broken=errno==EPIPE; break; }
        if (!count) { result=writing?-1:(int)used; break; }
        used+=(size_t)count;
        if (!writing && used==capacity) break; /* oversize: never publish truncated data */
    }
    if (writing) {
        if (broken && !had_pipe) { struct timespec zero={0}; (void)sigtimedwait(&blocked,NULL,&zero); }
        pthread_sigmask(SIG_SETMASK,&previous,NULL);
    }
    return result;
}
static void selection_owner(GDBusConnection *bus,const gchar *sender,const gchar *path,
    const gchar *interface,const gchar *signal,GVariant *parameters,gpointer data) {
    (void)bus;(void)sender;(void)path;(void)interface;(void)signal;
    struct uu_portal *p=data; const char *session; GVariant *options;
    g_variant_get(parameters,"(&o@a{sv})",&session,&options);
    if (!strcmp(session,p->session)) {
        gboolean owner=FALSE; g_variant_lookup(options,"session_is_owner","b",&owner);
        GVariant *types=g_variant_lookup_value(options,"mime_types",G_VARIANT_TYPE_STRING_ARRAY);
        char *selected=NULL;
        if (!owner && types) {
            GVariantIter iter; const char *type; g_variant_iter_init(&iter,types);
            while (g_variant_iter_next(&iter,"&s",&type)) if (text_mime(type)) {
                g_free(selected); selected=g_strdup(type); if (!strcmp(type,"text/plain;charset=utf-8")) break;
            }
        }
        if (getenv("UU_CLIPBOARD_DIAGNOSTICS")) fprintf(stderr,"Portal clipboard owner: own=%d text=%d\n",owner,selected!=NULL);
        pthread_mutex_lock(&p->lock);
        g_free(p->clipboard_mime); p->clipboard_mime=selected;
        p->clipboard_changed=!owner && selected!=NULL; p->clipboard_revision++;
        p->clipboard_owns=owner?1:0;
        pthread_mutex_unlock(&p->lock);
        if (types) g_variant_unref(types);
    }
    g_variant_unref(options);
}
static void selection_transfer(GDBusConnection *bus,const gchar *sender,const gchar *path,
    const gchar *interface,const gchar *signal,GVariant *parameters,gpointer data) {
    (void)bus;(void)sender;(void)path;(void)interface;(void)signal;
    struct uu_portal *p=data; const char *session,*mime; guint serial;
    g_variant_get(parameters,"(&o&su)",&session,&mime,&serial);
    if (strcmp(session,p->session)) return;
    uint8_t *copy=NULL; size_t length=0;
    pthread_mutex_lock(&p->lock);
    if (text_mime(mime) && p->clipboard_text) {
        length=p->clipboard_length; copy=g_memdup2(p->clipboard_text,length+1);
    }
    pthread_mutex_unlock(&p->lock);
    int success=0;
    if (copy && uu_portal_alive(p)) {
        int fd=clipboard_fd(p,"SelectionWrite",g_variant_new("(ou)",p->session,serial));
        if (fd>=0) { success=clipboard_io(fd,copy,length,1)==(int)length; close(fd); }
    }
    g_free(copy);
    GVariant *reply=g_dbus_connection_call_sync(p->bus,DEST,PATH,CLIPBOARD,"SelectionWriteDone",
        g_variant_new("(oub)",p->session,serial,success),NULL,G_DBUS_CALL_FLAGS_NONE,2000,NULL,NULL);
    if (reply) g_variant_unref(reply);
}
int uu_portal_clipboard_supported(struct uu_portal *p) { return p && p->clipboard_enabled; }
int uu_portal_clipboard_ready(struct uu_portal *p) { return p && p->clipboard_enabled && uu_portal_alive(p); }
int uu_portal_clipboard_set(struct uu_portal *p,const uint8_t *text,size_t length) {
    if (!uu_portal_clipboard_ready(p) || length>CLIP_LIMIT || !g_utf8_validate((const char *)text,(gssize)length,NULL)) return -1;
    char *copy=g_malloc(length+1); memcpy(copy,text,length); copy[length]=0;
    pthread_mutex_lock(&p->lock);
    /* Serve the new text while the portal announces it, and restore the old one on failure. */
    char *old_text=p->clipboard_text; size_t old_length=p->clipboard_length;
    uint64_t revision=p->clipboard_revision;
    p->clipboard_text=copy; p->clipboard_length=length; p->clipboard_changed=0;
    pthread_mutex_unlock(&p->lock);
    const char *types[]={"text/plain;charset=utf-8","text/plain",NULL};
    GVariantBuilder options; g_variant_builder_init(&options,G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&options,"{sv}","mime_types",g_variant_new_strv(types,-1));
    GVariant *reply=g_dbus_connection_call_sync(p->bus,DEST,PATH,CLIPBOARD,"SetSelection",
        g_variant_new("(o@a{sv})",p->session,g_variant_builder_end(&options)),NULL,G_DBUS_CALL_FLAGS_NONE,2000,NULL,NULL);
    if (!reply) {
        pthread_mutex_lock(&p->lock);
        g_free(p->clipboard_text);
        p->clipboard_text=old_text; p->clipboard_length=old_length;
        pthread_mutex_unlock(&p->lock);
        return -1;
    }
    g_variant_unref(reply);
    g_free(old_text);
    pthread_mutex_lock(&p->lock);
    /* Another owner may have been reported meanwhile; its newer state must win. */
    if (p->clipboard_revision==revision) p->clipboard_owns=1;
    pthread_mutex_unlock(&p->lock);
    return 0;
}
/* Hand the text this session is providing to a replacement session, so a session swap (cursor
 * mode, privacy screen, monitor switch) does not drop the viewer's clipboard. Transfers only
 * while this session still owns the selection. Returns 0 transferred, 1 nothing to transfer. */
int uu_portal_clipboard_copy(struct uu_portal *from, struct uu_portal *to) {
    if (!from || !to) return -1;
    pthread_mutex_lock(&from->lock);
    char *copy=(from->clipboard_owns && from->clipboard_text)?g_memdup2(from->clipboard_text,from->clipboard_length+1):NULL;
    size_t length=from->clipboard_length;
    pthread_mutex_unlock(&from->lock);
    if (!copy) return 1;
    int result=uu_portal_clipboard_set(to,(const uint8_t *)copy,length);
    g_free(copy);
    return result==0?0:-1;
}
int uu_portal_clipboard_poll(struct uu_portal *p,uint8_t *out,size_t capacity) {
    if (!out || !uu_portal_clipboard_ready(p) || capacity<CLIP_LIMIT) return -1;
    pthread_mutex_lock(&p->lock);
    char *mime=p->clipboard_changed?g_strdup(p->clipboard_mime):NULL; p->clipboard_changed=0;
    uint64_t revision=p->clipboard_revision;
    pthread_mutex_unlock(&p->lock);
    if (!mime) return -1;
    int fd=clipboard_fd(p,"SelectionRead",g_variant_new("(os)",p->session,mime)); g_free(mime);
    if (fd<0) { if (getenv("UU_CLIPBOARD_DIAGNOSTICS")) fprintf(stderr,"Portal clipboard read: fd unavailable\n"); return -1; }
    uint8_t *buffer=g_malloc(CLIP_LIMIT+1); int length=clipboard_io(fd,buffer,CLIP_LIMIT+1,0); close(fd);
    if (getenv("UU_CLIPBOARD_DIAGNOSTICS")) fprintf(stderr,"Portal clipboard read: length=%d\n",length);
    pthread_mutex_lock(&p->lock);
    if (p->clipboard_revision!=revision) length=-1;
    if (length>=0 && length<=CLIP_LIMIT && g_utf8_validate((const char *)buffer,length,NULL)) memcpy(out,buffer,(size_t)length);
    else length=-1;
    pthread_mutex_unlock(&p->lock);
    g_free(buffer); return length;
}
