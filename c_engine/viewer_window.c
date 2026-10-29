// GTK3 launcher window for the experimental native viewer (`uuremote-viewer ui`). Layout follows the
// official client's screens (see analysis notes: sidebar, device page, all devices, assistance,
// favorites, settings, QR sign-in, sign-out confirmation) without any NetEase assets. GTK runs on
// its own thread; Rust pushes state through g_idle_add and polls user commands. Account data is
// shown only in this local window and never logged.
#include <gtk/gtk.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/stat.h>
#include "i18n.h"

static const char *CSS =
    "window { background-color: #ffffff; }\n"
    "* { font-family: 'Noto Sans CJK SC', 'Noto Sans SC', sans-serif; }\n"
    ".sidebar { background-color: #e9e9ec; }\n"
    ".section { font-size: 16px; font-weight: 600; color: #1d1d1f; }\n"
    ".sidebar list, .sidebar row { background-color: transparent; }\n"
    ".sidebar row { border-radius: 8px; margin: 1px 10px; padding: 8px 10px; }\n"
    ".sidebar row:selected { background-color: #d4d4d9; color: #1d1d1f; }\n"
    ".devname { font-size: 15px; color: #1d1d1f; }\n"
    ".offline { color: #9ca3af; }\n"
    ".dot { color: #22c55e; font-size: 10px; }\n"
    ".dot.off { color: transparent; }\n"
    ".icon { background-color: #2f7bff; color: #ffffff; border-radius: 6px; font-size: 11px; font-weight: 700; padding: 2px 5px; }\n"
    ".icon.off { background-color: #c4c7cf; }\n"
    ".tag { background-color: #e8f0ff; color: #2f6bff; border-radius: 4px; font-size: 12px; padding: 1px 6px; }\n"
    ".title { font-size: 20px; font-weight: 700; color: #1d1d1f; }\n"
    ".header { border-bottom: 1px solid #e5e7eb; padding: 12px 22px; background-color: #f7f7f8; }\n"
    ".badge { background-color: #1d1d1f; color: #ffffff; border-radius: 12px; padding: 3px 12px; font-size: 13px; }\n"
    ".badge.off { background-color: #9ca3af; }\n"
    ".hbtn { background-image: none; background-color: transparent; border: none; font-size: 17px; color: #4b5563; padding: 4px 10px; }\n"
    ".preview { background-image: linear-gradient(135deg, #a8c0ff 0%, #e2c2f2 50%, #f6c8a8 100%); border-radius: 12px 12px 0 0; }\n"
    ".enter { background-image: none; background-color: rgba(0,0,0,0.18); color: #ffffff; border: none; font-size: 19px; padding: 8px 20px; border-radius: 8px; }\n"
    ".toolbar { border: 1px solid #e5e7eb; border-top: none; border-radius: 0 0 12px 12px; padding: 4px; }\n"
    ".tool { background-image: none; background-color: transparent; border: none; font-size: 15px; color: #374151; padding: 10px; }\n"
    ".dashed { border: 1px dashed #d1d5db; border-radius: 12px; padding: 36px; }\n"
    ".round { background-image: none; background-color: #f3f4f6; border: none; border-radius: 40px; font-size: 26px; min-width: 64px; min-height: 64px; }\n"
    ".card { background-color: #fafafa; border: 1px solid #e5e7eb; border-radius: 10px; padding: 14px 18px; }\n"
    ".cardbtn { background-image: none; background-color: #fafafa; border: 1px solid #e5e7eb; border-radius: 10px; padding: 12px 16px; }\n"
    ".banner { background-color: #f7f8fa; border: 1px solid #e5e7eb; border-radius: 10px; padding: 14px 18px; }\n"
    ".group { font-size: 16px; font-weight: 600; color: #1d1d1f; }\n"
    ".h2 { font-size: 19px; font-weight: 700; color: #1d1d1f; }\n"
    ".big { font-size: 30px; font-weight: 800; color: #111111; }\n"
    ".hint { font-size: 14px; color: #6b7280; }\n"
    ".primary { background-image: none; background-color: #3b82f6; color: #ffffff; border: none; border-radius: 8px; font-size: 16px; padding: 8px 26px; }\n"
    ".secondary { background-image: none; background-color: #ffffff; border: 1px solid #d1d5db; border-radius: 8px; padding: 6px 16px; }\n"
    ".qrbox { background-color: #ffffff; border-radius: 12px; border: 1px solid #e5e7eb; padding: 14px; }\n"
    ".status { font-size: 13px; color: #2f6bff; }\n"
    ".toast { background-color: #ffffff; border: 1px solid #e5e7eb; border-radius: 10px; padding: 10px 18px; font-size: 15px; }\n";

typedef struct { char id[64]; char name[128]; int platform, online, current; char thumb[512]; } Device;
static Device devices[64];
static int device_count;
static char queue[16][200];
static int q_head, q_len;
static pthread_mutex_t q_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t d_lock = PTHREAD_MUTEX_INITIALIZER;
static atomic_int closed;
static pthread_t ui_thread;
static int ui_started, init_state;
static pthread_mutex_t init_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t init_cond = PTHREAD_COND_INITIALIZER;

static GtkWidget *assist_list;
static GtkWidget *window, *stack, *title_label, *badge, *more_button, *device_list, *my_section, *all_box,
    *qr_image, *status_label, *toast, *banner, *fav_stack, *settings_window;
static int logged_in, selected = -1, banner_closed;
static char qr_path[512], status_text[256], user_text[128];

static int push(const char *cmd) {
    pthread_mutex_lock(&q_lock);
    int accepted = q_len < 16;
    if (accepted) { snprintf(queue[(q_head + q_len) % 16], sizeof queue[0], "%s", cmd); q_len++; }
    pthread_mutex_unlock(&q_lock);
    return accepted;
}
static void add_class(GtkWidget *w, const char *cls) { gtk_style_context_add_class(gtk_widget_get_style_context(w), cls); }
static GtkWidget *label(const char *text, const char *cls) {
    GtkWidget *l = gtk_label_new(text);
    if (cls) add_class(l, cls);
    gtk_label_set_xalign(GTK_LABEL(l), 0);
    gtk_label_set_line_wrap(GTK_LABEL(l), TRUE);
    return l;
}
static GtkWidget *button(const char *text, const char *cls) { GtkWidget *b = gtk_button_new_with_label(text); if (cls) add_class(b, cls); return b; }
static GtkWidget *hbox(int spacing) { return gtk_box_new(GTK_ORIENTATION_HORIZONTAL, spacing); }
static GtkWidget *vbox(int spacing) { return gtk_box_new(GTK_ORIENTATION_VERTICAL, spacing); }
static GtkWidget *platform_icon(int platform, int online) {
    GtkWidget *i = gtk_label_new(platform == 4 ? "Mac" : platform == 1 ? "Win" : "PC");
    add_class(i, "icon"); if (!online) add_class(i, "off");
    return i;
}
static gboolean hide_toast(gpointer d) { (void)d; gtk_widget_hide(toast); return G_SOURCE_REMOVE; }
static void show_toast(const char *text) { gtk_label_set_text(GTK_LABEL(toast), text); gtk_widget_show(toast); g_timeout_add(3500, hide_toast, NULL); }
static void unsupported(GtkButton *b, gpointer what) { (void)b; char t[128]; snprintf(t, sizeof t, L("%s：暂未支持", "%s: not supported yet"), (const char *)what); show_toast(t); }

static void connect_kind(const char *kind) {
    if (selected < 0) return;
    char cmd[200];
    pthread_mutex_lock(&d_lock);
    snprintf(cmd, sizeof cmd, "connect\t%s\t%s", devices[selected].id, kind);
    pthread_mutex_unlock(&d_lock);
    push(cmd);
    if (!strcmp(kind, "desktop") || !strcmp(kind, "watch")) show_toast(L("正在连接…", "Connecting…"));
}
static void on_tool(GtkButton *b, gpointer kind);
static void more_item(GtkButton *b, gpointer kind) {
    GtkWidget *dialog = gtk_widget_get_toplevel(GTK_WIDGET(b));
    const char *k = kind;
    if (!strcmp(k, "reboot") || !strcmp(k, "shutdown")) {
        GtkWidget *d = gtk_message_dialog_new(GTK_WINDOW(dialog), GTK_DIALOG_MODAL, GTK_MESSAGE_WARNING, GTK_BUTTONS_NONE,
            !strcmp(k, "reboot") ? L("确定要重启被控电脑吗？", "Restart the remote computer?") : L("确定要关闭被控电脑吗？", "Shut down the remote computer?"));
        gtk_message_dialog_format_secondary_text(GTK_MESSAGE_DIALOG(d), L("远程会话会断开。", "The remote session will be disconnected."));
        gtk_dialog_add_buttons(GTK_DIALOG(d), L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("确认", "Confirm"), GTK_RESPONSE_OK, NULL);
        int ok = gtk_dialog_run(GTK_DIALOG(d)) == GTK_RESPONSE_OK; gtk_widget_destroy(d);
        if (ok) { connect_kind(k); show_toast(L("正在发送电源指令…", "Sending power command…")); }
        return;
    }
    if (!strcmp(k, "unbind")) {
        GtkWidget *d = gtk_message_dialog_new(GTK_WINDOW(dialog), GTK_DIALOG_MODAL, GTK_MESSAGE_WARNING, GTK_BUTTONS_NONE,
            L("确定要从账号中删除该设备吗？", "Remove this device from your account?"));
        gtk_message_dialog_format_secondary_text(GTK_MESSAGE_DIALOG(d), L("删除后需要在该设备上重新登录才能再次远控。", "After removal you must sign in on that device again before you can control it."));
        gtk_dialog_add_buttons(GTK_DIALOG(d), L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("删除", "Remove"), GTK_RESPONSE_OK, NULL);
        int ok = gtk_dialog_run(GTK_DIALOG(d)) == GTK_RESPONSE_OK; gtk_widget_destroy(d);
        if (ok) { gtk_widget_destroy(dialog); connect_kind(k); show_toast(L("正在删除设备…", "Removing device…")); }
        return;
    }
    gtk_widget_destroy(dialog);
    on_tool(NULL, kind);
}
static void open_more(void) {
    GtkWidget *dialog = gtk_dialog_new_with_buttons(L("更多工具", "More tools"), GTK_WINDOW(window), GTK_DIALOG_MODAL, NULL, NULL);
    gtk_window_set_default_size(GTK_WINDOW(dialog), 520, -1);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(dialog)), *box = vbox(10);
    gtk_container_set_border_width(GTK_CONTAINER(box), 22);
    gtk_box_pack_start(GTK_BOX(box), label(L("更多工具", "More tools"), "h2"), FALSE, FALSE, 6);
    const char *names[] = {L("🗂   文件传输", "🗂   File transfer"), L("▶   观看模式", "▶   Watch mode"), L("⌨   终端", "⌨   Terminal"), L("⇄   端口映射", "⇄   Port mapping"), L("⏻   远程开机", "⏻   Wake on LAN"), L("↻   重启", "↻   Restart"), L("⏻   关机", "⏻   Shut down"), L("🗑   删除设备", "🗑   Remove device")};
    const char *kinds[] = {"file", "watch", "terminal", "portmap", "wol", "reboot", "shutdown", "unbind"};
    for (int i = 0; i < 8; i++) {
        GtkWidget *b = gtk_button_new(), *line = hbox(10); add_class(b, "cardbtn");
        gtk_box_pack_start(GTK_BOX(line), label(names[i], "devname"), TRUE, TRUE, 0);
        gtk_box_pack_end(GTK_BOX(line), label("›", "hint"), FALSE, FALSE, 0);
        gtk_container_add(GTK_CONTAINER(b), line);
        if (i == 4) gtk_widget_set_sensitive(b, FALSE); // 远程开机 is greyed for devices without Wake-on-LAN
        else g_signal_connect(b, "clicked", G_CALLBACK(more_item), (gpointer)kinds[i]);
        gtk_box_pack_start(GTK_BOX(box), b, FALSE, FALSE, 0);
    }
    GtkWidget *foot = hbox(10), *order = button(L("调整顺序", "Reorder"), "secondary"), *close = button(L("关闭", "Close"), "secondary");
    g_signal_connect(order, "clicked", G_CALLBACK(unsupported), L("调整顺序", "Reorder"));
    g_signal_connect_swapped(close, "clicked", G_CALLBACK(gtk_widget_destroy), dialog);
    gtk_box_pack_start(GTK_BOX(foot), order, FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(foot), close, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(box), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 8);
    gtk_box_pack_start(GTK_BOX(box), foot, FALSE, FALSE, 0);
    gtk_container_add(GTK_CONTAINER(area), box);
    gtk_widget_show_all(dialog);
}
static void on_tool(GtkButton *b, gpointer kind) {
    (void)b;
    if (!strcmp(kind, "more")) open_more();
    else if (!strcmp(kind, "wol") || !strcmp(kind, "reboot") || !strcmp(kind, "shutdown"))
        unsupported(NULL, (gpointer)(!strcmp(kind, "wol") ? L("远程开机", "Wake on LAN") : !strcmp(kind, "reboot") ? L("重启", "Restart") : L("关机", "Shut down")));
    else connect_kind(kind);
}
static void on_login(GtkButton *b, gpointer d) { (void)b; (void)d; push("login"); gtk_stack_set_visible_child_name(GTK_STACK(stack), "login"); gtk_label_set_text(GTK_LABEL(title_label), L("登录", "Sign in")); }

static void set_header(const char *title, int device_page) {
    gtk_label_set_text(GTK_LABEL(title_label), title);
    gtk_widget_set_visible(badge, device_page); gtk_widget_set_visible(more_button, device_page);
}
static void show_page(const char *name, const char *title) {
    int need_login = strcmp(name, "assist") && strcmp(name, "login");
    if (!logged_in && need_login) { set_header(L("全部设备", "All devices"), 0); gtk_stack_set_visible_child_name(GTK_STACK(stack), "welcome"); return; }
    set_header(title, 0);
    gtk_stack_set_visible_child_name(GTK_STACK(stack), name);
}
// ---- device preview: the last remote picture saved by the desktop window (src/viewer/thumbs.rs) ----
static GtkWidget *preview_bg;
static GdkPixbuf *thumb_pixbuf;
static char thumb_loaded[512];
static gint64 thumb_mtime;
static void load_thumb(const char *path) {
    struct stat st; gint64 mtime = path[0] && stat(path, &st) == 0 ? (gint64)st.st_mtime : 0;
    if (!strcmp(path, thumb_loaded) && mtime == thumb_mtime) return;
    if (thumb_pixbuf) { g_object_unref(thumb_pixbuf); thumb_pixbuf = NULL; }
    snprintf(thumb_loaded, sizeof thumb_loaded, "%s", path); thumb_mtime = mtime;
    if (mtime) thumb_pixbuf = gdk_pixbuf_new_from_file(path, NULL);
    if (preview_bg) gtk_widget_queue_draw(preview_bg);
}
static gboolean draw_preview(GtkWidget *w, cairo_t *cr, gpointer d) {
    (void)d; if (!thumb_pixbuf) return FALSE;
    double W = gtk_widget_get_allocated_width(w), H = gtk_widget_get_allocated_height(w), r = 12;
    double pw = gdk_pixbuf_get_width(thumb_pixbuf), ph = gdk_pixbuf_get_height(thumb_pixbuf);
    double scale = W / pw > H / ph ? W / pw : H / ph; // cover, centered
    cairo_save(cr);
    cairo_new_path(cr); cairo_arc(cr, r, r, r, G_PI, 1.5 * G_PI); cairo_arc(cr, W - r, r, r, 1.5 * G_PI, 2 * G_PI);
    cairo_line_to(cr, W, H); cairo_line_to(cr, 0, H); cairo_close_path(cr); cairo_clip(cr);
    cairo_translate(cr, (W - pw * scale) / 2, (H - ph * scale) / 2); cairo_scale(cr, scale, scale);
    gdk_cairo_set_source_pixbuf(cr, thumb_pixbuf, 0, 0);
    cairo_pattern_set_filter(cairo_get_source(cr), CAIRO_FILTER_GOOD); cairo_paint(cr);
    cairo_restore(cr);
    // Soft shade at the bottom keeps the "Open desktop" button readable on bright desktops.
    cairo_pattern_t *g = cairo_pattern_create_linear(0, H * 0.55, 0, H);
    cairo_pattern_add_color_stop_rgba(g, 0, 0, 0, 0, 0); cairo_pattern_add_color_stop_rgba(g, 1, 0, 0, 0, 0.35);
    cairo_rectangle(cr, 0, 0, W, H); cairo_set_source(cr, g); cairo_fill(cr); cairo_pattern_destroy(g);
    return FALSE;
}
static void show_device(int index) {
    pthread_mutex_lock(&d_lock);
    if (index < 0 || index >= device_count) { pthread_mutex_unlock(&d_lock); return; }
    selected = index;
    char name[128]; snprintf(name, sizeof name, "%s", devices[index].name);
    int online = devices[index].online;
    char thumb[512]; snprintf(thumb, sizeof thumb, "%s", devices[index].thumb);
    pthread_mutex_unlock(&d_lock);
    load_thumb(thumb);
    set_header(name, 1);
    gtk_label_set_markup(GTK_LABEL(badge), online ? L("<span foreground='#22c55e'>●</span> 在线", "<span foreground='#22c55e'>●</span> Online") : L("离线", "Offline"));
    GtkStyleContext *ctx = gtk_widget_get_style_context(badge);
    if (online) gtk_style_context_remove_class(ctx, "off"); else gtk_style_context_add_class(ctx, "off");
    gtk_stack_set_visible_child_name(GTK_STACK(stack), "device");
}
static void on_row(GtkListBox *box, GtkListBoxRow *row, gpointer d) {
    (void)d;
    if (!row) return;
    // The two sidebar lists act as one selection.
    gtk_list_box_unselect_all(GTK_LIST_BOX(box == GTK_LIST_BOX(device_list) ? assist_list : device_list));
    const char *kind = g_object_get_data(G_OBJECT(row), "kind");
    if (!kind) { show_device(GPOINTER_TO_INT(g_object_get_data(G_OBJECT(row), "index"))); return; }
    if (!strcmp(kind, "all")) show_page("all", L("全部设备", "All devices"));
    else if (!strcmp(kind, "assist")) show_page("assist", L("远程协助", "Remote assistance"));
    else if (!strcmp(kind, "favorite")) show_page("favorite", L("收藏设备", "Favorites"));
}
static void on_card(GtkButton *b, gpointer index) { (void)b; show_device(GPOINTER_TO_INT(index)); }

static GtkWidget *nav_row(const char *icon, const char *text, const char *kind) {
    GtkWidget *row = gtk_list_box_row_new(), *line = hbox(10);
    gtk_box_pack_start(GTK_BOX(line), label(icon, "devname"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(line), label(text, "devname"), TRUE, TRUE, 0);
    gtk_container_add(GTK_CONTAINER(row), line);
    g_object_set_data_full(G_OBJECT(row), "kind", g_strdup(kind), g_free);
    return row;
}
static void clear(GtkWidget *container) {
    GList *children = gtk_container_get_children(GTK_CONTAINER(container));
    for (GList *c = children; c; c = c->next) gtk_widget_destroy(c->data);
    g_list_free(children);
}

// ---- device info dialog (设备信息) ----
static void on_info(GtkButton *b, gpointer index) {
    (void)b;
    int i = GPOINTER_TO_INT(index);
    char name[128] = "";
    pthread_mutex_lock(&d_lock); if (i >= 0 && i < device_count) snprintf(name, sizeof name, "%s", devices[i].name); pthread_mutex_unlock(&d_lock);
    GtkWidget *dialog = gtk_dialog_new_with_buttons(L("设备信息", "Device info"), GTK_WINDOW(window), GTK_DIALOG_MODAL, L("完成", "Done"), GTK_RESPONSE_OK, NULL);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(dialog)), *box = vbox(12);
    gtk_container_set_border_width(GTK_CONTAINER(box), 20);
    GtkWidget *c1 = vbox(8); add_class(c1, "card");
    GtkWidget *r = hbox(8); gtk_box_pack_start(GTK_BOX(r), label(L("名称", "Name"), "devname"), TRUE, TRUE, 0); gtk_box_pack_end(GTK_BOX(r), label(name, "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(c1), r, FALSE, FALSE, 0);
    GtkWidget *rename = button(L("重命名", "Rename"), "secondary"); gtk_widget_set_halign(rename, GTK_ALIGN_END);
    g_signal_connect(rename, "clicked", G_CALLBACK(unsupported), L("重命名", "Rename"));
    gtk_box_pack_start(GTK_BOX(c1), rename, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(box), c1, FALSE, FALSE, 0);
    GtkWidget *c2 = vbox(10); add_class(c2, "card");
    const char *rows[] = {L("处理器", "Processor"), L("网卡", "Network adapter"), L("总内存", "Total memory"), L("系统版本", "OS version"), L("显卡", "Graphics")};
    for (int k = 0; k < 5; k++) {
        GtkWidget *line = hbox(8);
        gtk_box_pack_start(GTK_BOX(line), label(rows[k], "devname"), TRUE, TRUE, 0);
        gtk_box_pack_end(GTK_BOX(line), label("—", "hint"), FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(c2), line, FALSE, FALSE, 0);
    }
    gtk_box_pack_start(GTK_BOX(box), c2, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(box), label(L("硬件信息暂未获取。", "Hardware info not available yet."), "hint"), FALSE, FALSE, 0);
    gtk_container_add(GTK_CONTAINER(area), box);
    gtk_widget_show_all(dialog); gtk_dialog_run(GTK_DIALOG(dialog)); gtk_widget_destroy(dialog);
}
static void on_more(GtkButton *b, gpointer d) { (void)b; (void)d; on_info(NULL, GINT_TO_POINTER(selected)); }

static gboolean apply_devices(gpointer d) {
    (void)d;
    clear(device_list); clear(all_box);
    pthread_mutex_lock(&d_lock);
    if (selected >= 0 && selected < device_count) load_thumb(devices[selected].thumb);
    if (!banner_closed) gtk_widget_show_all(banner);
    char group[64]; snprintf(group, sizeof group, L("⌄  电脑 %d", "⌄  Computers %d"), device_count);
    gtk_box_pack_start(GTK_BOX(all_box), label(group, "group"), FALSE, FALSE, 8);
    for (int i = 0; i < device_count; i++) {
        GtkWidget *row = gtk_list_box_row_new(), *line = hbox(8);
        GtkWidget *dot = gtk_label_new("●"); add_class(dot, "dot"); if (!devices[i].online) add_class(dot, "off");
        GtkWidget *name = label(devices[i].name, "devname"); if (!devices[i].online) add_class(name, "offline");
        gtk_label_set_line_wrap(GTK_LABEL(name), FALSE); gtk_label_set_ellipsize(GTK_LABEL(name), PANGO_ELLIPSIZE_END);
        gtk_box_pack_start(GTK_BOX(line), dot, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(line), platform_icon(devices[i].platform, devices[i].online), FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(line), name, TRUE, TRUE, 0);
        gtk_container_add(GTK_CONTAINER(row), line);
        g_object_set_data(G_OBJECT(row), "index", GINT_TO_POINTER(i));
        if (!devices[i].current) gtk_list_box_insert(GTK_LIST_BOX(device_list), row, -1); else gtk_widget_destroy(row);
        GtkWidget *card = hbox(12); add_class(card, "card");
        gtk_box_pack_start(GTK_BOX(card), platform_icon(devices[i].platform, 1), FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(card), label(devices[i].name, "devname"), FALSE, FALSE, 0);
        if (devices[i].current) gtk_box_pack_start(GTK_BOX(card), label(L("本机", "This device"), "tag"), FALSE, FALSE, 0);
        GtkWidget *open = button("›", "hbtn"), *info = button("ⓘ", "hbtn");
        g_signal_connect(info, "clicked", G_CALLBACK(on_info), GINT_TO_POINTER(i));
        if (!devices[i].current) { g_signal_connect(open, "clicked", G_CALLBACK(on_card), GINT_TO_POINTER(i)); gtk_box_pack_end(GTK_BOX(card), open, FALSE, FALSE, 0); }
        else gtk_widget_destroy(open);
        gtk_box_pack_end(GTK_BOX(card), info, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(all_box), card, FALSE, FALSE, 4);
    }
    pthread_mutex_unlock(&d_lock);
    gtk_list_box_insert(GTK_LIST_BOX(device_list), nav_row("◼", L("全部设备", "All devices"), "all"), -1);
    gtk_widget_show_all(device_list); gtk_widget_show_all(all_box);
    return G_SOURCE_REMOVE;
}
static GtkWidget *account_who, *account_out;
/* The settings window is built once and kept; refresh its account card on every login change. */
static void refresh_account(void) {
    if (!account_who) return;
    gtk_label_set_text(GTK_LABEL(account_who), logged_in ? (user_text[0] ? user_text : L("已登录", "Signed in")) : L("未登录", "Not signed in"));
    gtk_widget_set_sensitive(account_out, logged_in);
}
static gboolean apply_login(gpointer d) {
    (void)d;
    refresh_account();
    gtk_label_set_text(GTK_LABEL(status_label), status_text);
    if (qr_path[0]) {
        GdkPixbuf *pix = gdk_pixbuf_new_from_file_at_size(qr_path, 240, 240, NULL);
        if (pix) { gtk_image_set_from_pixbuf(GTK_IMAGE(qr_image), pix); g_object_unref(pix); }
    } else gtk_image_clear(GTK_IMAGE(qr_image));
    gtk_widget_set_visible(my_section, logged_in);
    const char *page = gtk_stack_get_visible_child_name(GTK_STACK(stack));
    if (logged_in && (!strcmp(page, "login") || !strcmp(page, "welcome"))) show_page("all", L("全部设备", "All devices"));
    if (!logged_in && strcmp(page, "login") && strcmp(page, "assist")) show_page("welcome", L("全部设备", "All devices"));
    if (!logged_in && qr_path[0]) { gtk_stack_set_visible_child_name(GTK_STACK(stack), "login"); set_header(L("登录", "Sign in"), 0); }
    return G_SOURCE_REMOVE;
}

// ---- pages ----
static GtkWidget *build_welcome(void) {
    GtkWidget *page = vbox(22); gtk_widget_set_valign(page, GTK_ALIGN_CENTER); gtk_widget_set_halign(page, GTK_ALIGN_CENTER);
    GtkWidget *art = gtk_event_box_new(); add_class(art, "preview"); gtk_widget_set_size_request(art, 520, 300);
    gtk_box_pack_start(GTK_BOX(page), art, FALSE, FALSE, 0);
    GtkWidget *t = label(L("随时访问个人设备，可用于办公、游戏、文件传输。", "Access your devices anytime for work, gaming and file transfer."), "h2"); gtk_widget_set_halign(t, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(page), t, FALSE, FALSE, 0);
    GtkWidget *b = button(L("立即登录", "Sign in now"), "primary"); gtk_widget_set_halign(b, GTK_ALIGN_CENTER);
    g_signal_connect(b, "clicked", G_CALLBACK(on_login), NULL);
    gtk_box_pack_start(GTK_BOX(page), b, FALSE, FALSE, 0);
    return page;
}
static GtkWidget *build_login(void) {
    GtkWidget *page = vbox(16); gtk_widget_set_valign(page, GTK_ALIGN_CENTER); gtk_widget_set_halign(page, GTK_ALIGN_CENTER);
    GtkWidget *brand = label(L("UU 远程 Linux 控制端（非官方）", "UU Remote Linux viewer (unofficial)"), "h2"); gtk_widget_set_halign(brand, GTK_ALIGN_CENTER);
    GtkWidget *t = label(L("扫码登录", "Scan to sign in"), "group"); gtk_widget_set_halign(t, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(page), brand, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), t, FALSE, FALSE, 0);
    GtkWidget *box = vbox(0); add_class(box, "qrbox"); gtk_widget_set_halign(box, GTK_ALIGN_CENTER);
    qr_image = gtk_image_new(); gtk_widget_set_size_request(qr_image, 240, 240);
    gtk_container_add(GTK_CONTAINER(box), qr_image);
    gtk_box_pack_start(GTK_BOX(page), box, FALSE, FALSE, 0);
    GtkWidget *h = label(L("请使用网易 UU 远程 App 扫一扫\n可快速登录", "Scan with the NetEase UU Remote app\nto sign in quickly"), "hint"); gtk_label_set_justify(GTK_LABEL(h), GTK_JUSTIFY_CENTER); gtk_widget_set_halign(h, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(page), h, FALSE, FALSE, 0);
    status_label = label("", "status"); gtk_widget_set_halign(status_label, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(page), status_label, FALSE, FALSE, 0);
    GtkWidget *again = button(L("刷新二维码", "Refresh QR code"), "secondary"); gtk_widget_set_halign(again, GTK_ALIGN_CENTER);
    g_signal_connect(again, "clicked", G_CALLBACK(on_login), NULL);
    gtk_box_pack_start(GTK_BOX(page), again, FALSE, FALSE, 0);
    return page;
}
static GtkWidget *build_device(void) {
    GtkWidget *page = vbox(0); gtk_container_set_border_width(GTK_CONTAINER(page), 26);
    GtkWidget *preview = gtk_overlay_new(), *bg = gtk_event_box_new(); add_class(bg, "preview"); gtk_widget_set_size_request(bg, -1, 320);
    preview_bg = bg; g_signal_connect_after(bg, "draw", G_CALLBACK(draw_preview), NULL);
    gtk_container_add(GTK_CONTAINER(preview), bg);
    GtkWidget *enter = button(L("进入桌面   →", "Open desktop   →"), "enter");
    gtk_widget_set_halign(enter, GTK_ALIGN_CENTER); gtk_widget_set_valign(enter, GTK_ALIGN_END); gtk_widget_set_margin_bottom(enter, 26);
    g_signal_connect(enter, "clicked", G_CALLBACK(on_tool), "desktop");
    gtk_overlay_add_overlay(GTK_OVERLAY(preview), enter);
    gtk_box_pack_start(GTK_BOX(page), preview, FALSE, FALSE, 0);
    GtkWidget *tools = hbox(0); add_class(tools, "toolbar");
    const char *names[] = {L("🗂  文件传输", "🗂  File transfer"), L("▶  观看模式", "▶  Watch mode"), L("⌨  终端", "⌨  Terminal"), L("⇄  端口映射", "⇄  Port mapping"), "▦"}, *kinds[] = {"file", "watch", "terminal", "portmap", "more"};
    for (int i = 0; i < 5; i++) {
        GtkWidget *b = button(names[i], "tool");
        g_signal_connect(b, "clicked", G_CALLBACK(on_tool), (gpointer)kinds[i]);
        gtk_box_pack_start(GTK_BOX(tools), b, i < 4, TRUE, 0);
        if (i < 4) gtk_box_pack_start(GTK_BOX(tools), gtk_separator_new(GTK_ORIENTATION_VERTICAL), FALSE, FALSE, 0);
    }
    gtk_box_pack_start(GTK_BOX(page), tools, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), label(L("快速启动", "Quick launch"), "h2"), FALSE, FALSE, 22);
    GtkWidget *quick = vbox(10); add_class(quick, "dashed");
    GtkWidget *add = button("+", "round"); gtk_widget_set_halign(add, GTK_ALIGN_CENTER);
    g_signal_connect(add, "clicked", G_CALLBACK(unsupported), L("快速启动", "Quick launch"));
    GtkWidget *al = label(L("添加", "Add"), "hint"); gtk_widget_set_halign(al, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(quick), add, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(quick), al, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), quick, FALSE, FALSE, 0);
    return page;
}
static void close_banner(GtkButton *b, gpointer d) { (void)b; (void)d; banner_closed = 1; gtk_widget_hide(banner); }
static GtkWidget *build_all(void) {
    GtkWidget *page = vbox(8); gtk_container_set_border_width(GTK_CONTAINER(page), 26);
    banner = hbox(10); add_class(banner, "banner");
    GtkWidget *text = vbox(4);
    gtk_box_pack_start(GTK_BOX(text), label(L("ⓘ  添加你的其他设备", "ⓘ  Add your other devices"), "group"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(text), label(L("在其他电脑上安装 UU 远程官方客户端或本项目主机端，登录同一账号后即可在这里远程访问。", "Install the official UU Remote client or this project's host on another computer and sign in with the same account to access it here."), "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(banner), text, TRUE, TRUE, 0);
    GtkWidget *x = button("×", "hbtn"); gtk_widget_set_valign(x, GTK_ALIGN_START);
    g_signal_connect(x, "clicked", G_CALLBACK(close_banner), NULL);
    gtk_box_pack_end(GTK_BOX(banner), x, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), banner, FALSE, FALSE, 0);
    all_box = vbox(4);
    gtk_box_pack_start(GTK_BOX(page), all_box, FALSE, FALSE, 8);
    GtkWidget *scroll = gtk_scrolled_window_new(NULL, NULL); gtk_container_add(GTK_CONTAINER(scroll), page);
    return scroll;
}
static void on_assist_connect(GtkButton *b, gpointer entry) {
    (void)b;
    const char *id = gtk_entry_get_text(GTK_ENTRY(entry));
    if (!id[0]) { show_toast(L("请输入伙伴的设备 ID", "Enter your partner's device ID")); return; }
    GtkWidget *code_entry = g_object_get_data(G_OBJECT(entry), "assist-code");
    const char *code = gtk_entry_get_text(GTK_ENTRY(code_entry));
    if (!code[0]) { show_toast(L("请输入伙伴的设备验证码", "Enter your partner's verification code")); return; }
    if (strpbrk(id, "\t\r\n") || strpbrk(code, "\t\r\n")) { show_toast(L("设备 ID 和验证码不能包含换行或制表符", "Device ID and code cannot contain line breaks or tabs")); return; }
    char cmd[200];
    int n = snprintf(cmd, sizeof cmd, "assist\t%s\t%s", id, code);
    if (n < 0 || (size_t)n >= sizeof cmd) { show_toast(L("设备 ID 或验证码过长", "Device ID or code is too long")); return; }
    int accepted = push(cmd);
    memset(cmd, 0, sizeof cmd);
    if (!accepted) { show_toast(L("连接请求过多，请稍后重试", "Too many connection requests, try again later")); return; }
    gtk_entry_set_text(GTK_ENTRY(code_entry), "");
    show_toast(L("正在连接…", "Connecting…"));
}
static GtkWidget *build_assist(void) {
    GtkWidget *page = vbox(20); gtk_container_set_border_width(GTK_CONTAINER(page), 26);
    GtkWidget *mine = vbox(14); add_class(mine, "card");
    GtkWidget *top = hbox(10); gtk_box_pack_start(GTK_BOX(top), label(L("本设备", "This device"), "h2"), TRUE, TRUE, 0);
    gtk_box_pack_start(GTK_BOX(top), label(L("允许他人远程协助", "Allow remote assistance"), "devname"), FALSE, FALSE, 0);
    GtkWidget *sw = gtk_switch_new(); gtk_widget_set_sensitive(sw, FALSE); gtk_box_pack_start(GTK_BOX(top), sw, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(mine), top, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(mine), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0);
    GtkWidget *grid = hbox(40), *c1 = vbox(6), *c2 = vbox(6);
    gtk_box_pack_start(GTK_BOX(c1), label(L("本设备 ID", "This device's ID"), "devname"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(c1), label("— — —", "big"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(c2), label(L("验证方式：仅使用临时验证码", "Verification: temporary code only"), "devname"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(c2), label("—", "big"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(c2), label(L("每次远控结束刷新", "Refreshed after every remote session"), "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(grid), c1, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(grid), c2, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(mine), grid, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(mine), label(L("本机被协助由 uuremote-server 主机端提供（uuremote-server assist）。", "Assistance for this computer is provided by the uuremote-server host (uuremote-server assist)."), "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), mine, FALSE, FALSE, 0);
    GtkWidget *peer = vbox(12); add_class(peer, "card");
    gtk_box_pack_start(GTK_BOX(peer), label(L("远控伙伴设备", "Control a partner's device"), "h2"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(peer), label(L("通过其他设备的【设备 ID】及【设备验证码】启动远程控制", "Start remote control with another device's [Device ID] and [Verification code]"), "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(peer), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(peer), label(L("伙伴的设备 ID", "Partner's device ID"), "devname"), FALSE, FALSE, 0);
    GtkWidget *row = hbox(20), *entry = gtk_entry_new(); gtk_entry_set_placeholder_text(GTK_ENTRY(entry), L("请输入设备 ID", "Enter device ID")); gtk_widget_set_size_request(entry, 320, -1);
    GtkWidget *code = gtk_entry_new(); gtk_entry_set_placeholder_text(GTK_ENTRY(code), L("请输入设备验证码", "Enter verification code"));
    gtk_entry_set_visibility(GTK_ENTRY(code), FALSE);
    gtk_entry_set_input_purpose(GTK_ENTRY(code), GTK_INPUT_PURPOSE_PASSWORD);
    gtk_entry_set_max_length(GTK_ENTRY(entry), 64); gtk_entry_set_max_length(GTK_ENTRY(code), 64);
    g_object_set_data(G_OBJECT(entry), "assist-code", code);
    g_signal_connect(code, "activate", G_CALLBACK(on_assist_connect), entry);
    GtkWidget *go = button(L("连接", "Connect"), "primary"); g_signal_connect(go, "clicked", G_CALLBACK(on_assist_connect), entry);
    gtk_box_pack_start(GTK_BOX(row), entry, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(row), code, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(row), go, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(peer), row, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(page), peer, FALSE, FALSE, 0);
    return page;
}
static void on_add_favorite(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    GtkWidget *dialog = gtk_dialog_new_with_buttons(L("添加收藏设备", "Add favorite device"), GTK_WINDOW(window), GTK_DIALOG_MODAL, L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("添加", "Add"), GTK_RESPONSE_OK, NULL);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(dialog)), *card = vbox(12); add_class(card, "card");
    gtk_container_set_border_width(GTK_CONTAINER(area), 16);
    const char *names[] = {L("伙伴的设备 ID", "Partner's device ID"), L("设备验证码（可选）", "Verification code (optional)"), L("备注名称（可选）", "Nickname (optional)")}, *hints[] = {L("请输入设备 ID", "Enter device ID"), L("请输入设备验证码", "Enter verification code"), L("请输入名称", "Enter a name")};
    GtkWidget *first = NULL;
    for (int i = 0; i < 3; i++) {
        GtkWidget *line = hbox(20), *e = gtk_entry_new(); gtk_entry_set_placeholder_text(GTK_ENTRY(e), hints[i]); gtk_widget_set_size_request(e, 280, -1);
        if (!first) first = e;
        gtk_box_pack_start(GTK_BOX(line), label(names[i], "devname"), TRUE, TRUE, 0); gtk_box_pack_end(GTK_BOX(line), e, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(card), line, FALSE, FALSE, 0);
    }
    gtk_container_add(GTK_CONTAINER(area), card);
    gtk_widget_show_all(dialog);
    int response = gtk_dialog_run(GTK_DIALOG(dialog));
    int has_id = first && gtk_entry_get_text(GTK_ENTRY(first))[0];
    gtk_widget_destroy(dialog);
    if (response == GTK_RESPONSE_OK) show_toast(has_id ? L("收藏设备：暂未支持", "Favorite devices: not supported yet") : L("请输入伙伴的设备 ID", "Enter your partner's device ID"));
}
static GtkWidget *build_favorite(void) {
    GtkWidget *page = vbox(10); gtk_container_set_border_width(GTK_CONTAINER(page), 26);
    GtkWidget *add = button(L("添加收藏设备", "Add favorite device"), "secondary"); gtk_widget_set_halign(add, GTK_ALIGN_END);
    g_signal_connect(add, "clicked", G_CALLBACK(on_add_favorite), NULL);
    gtk_box_pack_start(GTK_BOX(page), add, FALSE, FALSE, 0);
    fav_stack = gtk_stack_new();
    GtkWidget *empty = vbox(10); gtk_widget_set_valign(empty, GTK_ALIGN_CENTER);
    GtkWidget *box = label("📦", "big"); gtk_widget_set_halign(box, GTK_ALIGN_CENTER);
    GtkWidget *t = label(L("暂无收藏", "No favorites yet"), "devname"); gtk_widget_set_halign(t, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(empty), box, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(empty), t, FALSE, FALSE, 0);
    gtk_stack_add_named(GTK_STACK(fav_stack), empty, "empty");
    gtk_box_pack_start(GTK_BOX(page), fav_stack, TRUE, TRUE, 0);
    return page;
}

// ---- settings window (常规 / 安全 / 键盘 / 网络) ----
static GtkWidget *setting_row(const char *title, const char *detail, GtkWidget *control) {
    GtkWidget *row = hbox(12), *text = vbox(2);
    gtk_box_pack_start(GTK_BOX(text), label(title, "devname"), FALSE, FALSE, 0);
    if (detail) gtk_box_pack_start(GTK_BOX(text), label(detail, "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(row), text, TRUE, TRUE, 0);
    if (control) { gtk_widget_set_valign(control, GTK_ALIGN_CENTER); gtk_box_pack_end(GTK_BOX(row), control, FALSE, FALSE, 0); }
    return row;
}
static GtkWidget *combo(const char *const *items, int n) {
    GtkWidget *c = gtk_combo_box_text_new();
    for (int i = 0; i < n; i++) gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(c), items[i]);
    gtk_combo_box_set_active(GTK_COMBO_BOX(c), 0);
    return c;
}
static GtkWidget *settings_card(GtkWidget **rows, int n) {
    GtkWidget *card = vbox(10); add_class(card, "card");
    for (int i = 0; i < n; i++) { if (i) gtk_box_pack_start(GTK_BOX(card), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(card), rows[i], FALSE, FALSE, 0); }
    return card;
}
static GtkWidget *settings_page(void) { GtkWidget *p = vbox(14); gtk_container_set_border_width(GTK_CONTAINER(p), 22); return p; }
static GtkWidget *edit_button(const char *what) { GtkWidget *b = button(L("编辑", "Edit"), "secondary"); g_signal_connect(b, "clicked", G_CALLBACK(unsupported), (gpointer)what); return b; }
static gboolean settings_hidden(GtkWidget *w, GdkEvent *e, gpointer d) { (void)e; (void)d; gtk_widget_hide(w); return TRUE; }
static gboolean settings_key(GtkWidget *w, GdkEventKey *e, gpointer d) { (void)d; if (e->keyval == GDK_KEY_Escape) { gtk_widget_hide(w); return TRUE; } return FALSE; }
// Compositors such as Hyprland draw no title bar, so windows carry their own (with a close button).
static void own_titlebar(GtkWidget *w, const char *title) {
    GtkWidget *bar = gtk_header_bar_new();
    gtk_header_bar_set_title(GTK_HEADER_BAR(bar), title);
    gtk_header_bar_set_show_close_button(GTK_HEADER_BAR(bar), TRUE);
    gtk_window_set_titlebar(GTK_WINDOW(w), bar);
}
static atomic_int host_active;
static GtkWidget *host_switch;
static gboolean host_idle(gpointer d) { (void)d; if (host_switch) gtk_switch_set_active(GTK_SWITCH(host_switch), atomic_load(&host_active)); return G_SOURCE_REMOVE; }
void uu_viewer_ui_set_host(int active) { atomic_store(&host_active, active != 0); g_idle_add(host_idle, NULL); }
static void on_host(GObject *o, GParamSpec *p, gpointer d) {
    (void)p; (void)d; int on = gtk_switch_get_active(GTK_SWITCH(o));
    if (on != atomic_load(&host_active)) { atomic_store(&host_active, on); push(on ? "host\ton" : "host\toff"); }
}
static void on_language(GtkComboBox *c, gpointer d) {
    (void)d; int english = gtk_combo_box_get_active(c) == 1;
    if (english == uu_i18n_english()) return;
    if (!uu_i18n_save(english)) show_toast(L("无法保存语言设置", "Could not save the language"));
    else push("relaunch");
}
static void confirm_logout(void);
static void on_sign_out(GtkButton *b, gpointer d) { (void)b; (void)d; if (logged_in) confirm_logout(); }
static void open_settings(void) {
    if (settings_window) { refresh_account(); gtk_window_present(GTK_WINDOW(settings_window)); return; }
    settings_window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(settings_window), L("设置", "Settings"));
    gtk_window_set_transient_for(GTK_WINDOW(settings_window), GTK_WINDOW(window));
    gtk_window_set_default_size(GTK_WINDOW(settings_window), 680, 600);
    g_signal_connect(settings_window, "delete-event", G_CALLBACK(settings_hidden), NULL);
    g_signal_connect(settings_window, "key-press-event", G_CALLBACK(settings_key), NULL);
    own_titlebar(settings_window, L("设置", "Settings"));
    GtkWidget *root = vbox(0), *tabs = gtk_stack_new(), *switcher = gtk_stack_switcher_new();
    gtk_stack_switcher_set_stack(GTK_STACK_SWITCHER(switcher), GTK_STACK(tabs));
    gtk_widget_set_halign(switcher, GTK_ALIGN_CENTER); gtk_widget_set_margin_top(switcher, 10); gtk_widget_set_margin_bottom(switcher, 6);
    gtk_box_pack_start(GTK_BOX(root), switcher, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(root), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0);
    // 常规
    GtkWidget *general = settings_page();
    GtkWidget *acct = hbox(12), *acct_text = vbox(2);
    gtk_box_pack_start(GTK_BOX(acct_text), label(L("账号", "Account"), "devname"), FALSE, FALSE, 0);
    account_who = label("", "hint"); gtk_box_pack_start(GTK_BOX(acct_text), account_who, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(acct), acct_text, TRUE, TRUE, 0);
    account_out = button(L("退出登录", "Sign out"), "secondary"); gtk_widget_set_valign(account_out, GTK_ALIGN_CENTER);
    g_signal_connect(account_out, "clicked", G_CALLBACK(on_sign_out), NULL); gtk_box_pack_end(GTK_BOX(acct), account_out, FALSE, FALSE, 0);
    GtkWidget *g0[] = { acct };
    gtk_box_pack_start(GTK_BOX(general), settings_card(g0, 1), FALSE, FALSE, 0);
    refresh_account();
    const char *wake[] = {L("永不", "Never"), L("接通电源时", "When plugged in"), L("始终", "Always")}, *close_opts[] = {L("最小化", "Minimize"), L("退出程序", "Quit")};
    GtkWidget *g1[] = { setting_row(L("开机自启动", "Start at login"), NULL, gtk_switch_new()),
        setting_row(L("保持唤醒状态", "Stay awake"), L("防止电脑进入睡眠状态，导致无法远程访问。", "Keep the computer from sleeping so it stays reachable."), combo(wake, 3)),
        setting_row(L("关闭窗口时", "When closing the window"), NULL, combo(close_opts, 2)) };
    gtk_box_pack_start(GTK_BOX(general), settings_card(g1, 3), FALSE, FALSE, 0);
    GtkWidget *path = hbox(8); GtkWidget *change = button(L("修改路径", "Change path"), "secondary"), *open = button(L("打开文件夹", "Open folder"), "secondary");
    g_signal_connect(change, "clicked", G_CALLBACK(unsupported), L("修改路径", "Change path")); g_signal_connect(open, "clicked", G_CALLBACK(unsupported), L("打开文件夹", "Open folder"));
    gtk_box_pack_end(GTK_BOX(path), open, FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(path), change, FALSE, FALSE, 0);
    GtkWidget *g2[] = { setting_row(L("传输文件存储路径", "Received files folder"), "~/Downloads", path) };
    gtk_box_pack_start(GTK_BOX(general), settings_card(g2, 1), FALSE, FALSE, 0);
    GtkWidget *real = button(L("在手机 App 中查看", "View in the mobile app"), "secondary"); gtk_widget_set_sensitive(real, FALSE);
    GtkWidget *g3[] = { setting_row(L("实名认证", "Identity verification"), NULL, real) };
    gtk_box_pack_start(GTK_BOX(general), settings_card(g3, 1), FALSE, FALSE, 0);
    const char *languages[] = {"中文", "English"}; GtkWidget *language = combo(languages, 2);
    gtk_combo_box_set_active(GTK_COMBO_BOX(language), uu_i18n_english()); g_signal_connect(language, "changed", G_CALLBACK(on_language), NULL);
    GtkWidget *g4[] = { setting_row("语言 / Language", NULL, language) };
    gtk_box_pack_start(GTK_BOX(general), settings_card(g4, 1), FALSE, FALSE, 0);
    gtk_stack_add_titled(GTK_STACK(tabs), general, "general", L("⚙ 常规", "⚙ General"));
    // 安全
    GtkWidget *security = settings_page();
    const char *verify[] = {L("仅使用临时验证码", "Temporary code only"), L("仅使用自定义验证码", "Custom code only"), L("临时与自定义验证码", "Temporary and custom codes")};
    host_switch = gtk_switch_new(); gtk_switch_set_active(GTK_SWITCH(host_switch), atomic_load(&host_active));
    g_signal_connect(host_switch, "notify::active", G_CALLBACK(on_host), NULL);
    GtkWidget *s1[] = { setting_row(L("允许同账号控制本设备", "Allow the same account to control this device"), L("默认关闭。开启后本机会出现在你的设备列表中，可被远程控制（uuremote-server 用户服务）", "Off by default. When on, this computer appears in your device list and can be controlled (uuremote-server user service)"), host_switch), setting_row(L("允许他人远程协助", "Allow remote assistance"), NULL, gtk_switch_new()) };
    GtkWidget *s2[] = { setting_row(L("远程协助连接本设备的验证方式", "Verification for remote assistance"), NULL, combo(verify, 3)) };
    GtkWidget *s3[] = { setting_row(L("客户端锁定", "Lock client"), L("防止客户端设置被修改，保障数据与使用安全。", "Prevents client settings from being changed, keeping data and usage safe."), gtk_switch_new()) };
    gtk_box_pack_start(GTK_BOX(security), settings_card(s1, 2), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(security), settings_card(s2, 1), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(security), settings_card(s3, 1), FALSE, FALSE, 0);
    gtk_stack_add_titled(GTK_STACK(tabs), security, "security", L("🔒 安全", "🔒 Security"));
    // 键盘
    GtkWidget *keys = settings_page();
    gtk_box_pack_start(GTK_BOX(keys), label(L("仅控制端响应的快捷键", "Viewer-only shortcuts"), "h2"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(keys), label(L("远控时按下以下快捷键，仅在控制端本地响应，不会发送到被控端。", "These shortcuts are handled locally on the viewer and are not sent to the remote device."), "hint"), FALSE, FALSE, 0);
    GtkWidget *addk = button(L("添加控制端快捷键", "Add viewer shortcut"), "secondary"); g_signal_connect(addk, "clicked", G_CALLBACK(unsupported), L("添加快捷键", "Add shortcut"));
    GtkWidget *k1[] = { setting_row(L("当前暂无设置", "Nothing set yet"), NULL, NULL), setting_row("", NULL, addk) };
    gtk_box_pack_start(GTK_BOX(keys), settings_card(k1, 2), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(keys), label(L("功能快捷键", "Function shortcuts"), "h2"), FALSE, FALSE, 6);
    gtk_box_pack_start(GTK_BOX(keys), label(L("按下以下快捷键时，仅在本地触发对应功能，不会发送到被控端。", "These shortcuts trigger local functions only and are not sent to the remote device."), "hint"), FALSE, FALSE, 0);
    const char *fns[] = {L("使用被控端鼠标/解锁鼠标", "Use remote mouse / release mouse"), L("被控时解除防窥模式", "Turn off privacy screen while controlled"), L("切换上一个屏幕", "Previous screen"), L("切换下一个屏幕", "Next screen"), L("显示/隐藏悬浮球", "Show/hide floating ball"), L("静音/取消静音", "Mute/unmute"), L("老板键（隐藏/显示所有客户端窗口）", "Boss key (hide/show all client windows)"), L("发送 Ctrl+Alt+Delete 组合键", "Send Ctrl+Alt+Delete")};
    GtkWidget *k2[8];
    for (int i = 0; i < 8; i++) k2[i] = setting_row(fns[i], NULL, edit_button(fns[i]));
    gtk_box_pack_start(GTK_BOX(keys), settings_card(k2, 8), FALSE, FALSE, 0);
    GtkWidget *kscroll = gtk_scrolled_window_new(NULL, NULL); gtk_container_add(GTK_CONTAINER(kscroll), keys);
    gtk_stack_add_titled(GTK_STACK(tabs), kscroll, "keys", L("⌨ 键盘", "⌨ Keyboard"));
    // 网络
    GtkWidget *network = settings_page();
    const char *proxy[] = {L("使用系统环境代理", "Use system proxy"), L("不使用代理", "No proxy")};
    GtkWidget *n1[] = { setting_row(L("代理设置", "Proxy"), NULL, combo(proxy, 2)) };
    gtk_box_pack_start(GTK_BOX(network), settings_card(n1, 1), FALSE, FALSE, 0);
    gtk_stack_add_titled(GTK_STACK(tabs), network, "network", L("🌐 网络", "🌐 Network"));
    gtk_box_pack_start(GTK_BOX(root), tabs, TRUE, TRUE, 0);
    gtk_container_add(GTK_CONTAINER(settings_window), root);
    gtk_widget_show_all(settings_window);
}
static void confirm_logout(void) {
    GtkWidget *d = gtk_message_dialog_new(GTK_WINDOW(window), GTK_DIALOG_MODAL, GTK_MESSAGE_WARNING, GTK_BUTTONS_NONE, L("确定要退出登录吗？", "Sign out?"));
    gtk_message_dialog_format_secondary_text(GTK_MESSAGE_DIALOG(d), L("退出登录会中断正在进行中的连接，你的设备也将从本账户删除。请谨慎操作！", "Signing out ends active connections and removes your device from this account. Be careful!"));
    gtk_dialog_add_buttons(GTK_DIALOG(d), L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("确认", "Confirm"), GTK_RESPONSE_OK, NULL);
    if (gtk_dialog_run(GTK_DIALOG(d)) == GTK_RESPONSE_OK) push("logout");
    gtk_widget_destroy(d);
}
static void on_menu(GtkMenuItem *item, gpointer what) {
    (void)item;
    if (!strcmp(what, "settings")) open_settings();
    else if (!strcmp(what, "logout")) { if (logged_in) confirm_logout(); }
    else if (!strcmp(what, "refresh")) push("refresh");
    else if (!strcmp(what, "quit")) { atomic_store(&closed, 1); push("quit"); gtk_main_quit(); }
}
static void popup(GtkButton *b, gpointer kind) {
    GtkWidget *menu = gtk_menu_new();
    if (!strcmp(kind, "account")) {
        GtkWidget *who = gtk_menu_item_new_with_label(logged_in ? (user_text[0] ? user_text : L("已登录", "Signed in")) : L("未登录", "Not signed in")); gtk_widget_set_sensitive(who, FALSE);
        gtk_menu_shell_append(GTK_MENU_SHELL(menu), who);
        GtkWidget *out = gtk_menu_item_new_with_label(L("退出登录", "Sign out")); g_signal_connect(out, "activate", G_CALLBACK(on_menu), "logout");
        gtk_widget_set_sensitive(out, logged_in); gtk_menu_shell_append(GTK_MENU_SHELL(menu), out);
    } else if (!strcmp(kind, "notice")) {
        GtkWidget *none = gtk_menu_item_new_with_label(L("暂无通知", "No notifications")); gtk_widget_set_sensitive(none, FALSE);
        gtk_menu_shell_append(GTK_MENU_SHELL(menu), none);
    } else {
        const char *labels[] = {L("刷新设备列表", "Refresh device list"), L("设置", "Settings"), L("退出程序", "Quit")}, *whats[] = {"refresh", "settings", "quit"};
        for (int i = 0; i < 3; i++) { GtkWidget *it = gtk_menu_item_new_with_label(labels[i]); g_signal_connect(it, "activate", G_CALLBACK(on_menu), (gpointer)whats[i]); gtk_menu_shell_append(GTK_MENU_SHELL(menu), it); }
    }
    gtk_widget_show_all(menu);
    gtk_menu_popup_at_widget(GTK_MENU(menu), GTK_WIDGET(b), GDK_GRAVITY_SOUTH_EAST, GDK_GRAVITY_NORTH_EAST, NULL);
}
static gboolean on_delete(GtkWidget *w, GdkEvent *e, gpointer d) { (void)w; (void)e; (void)d; atomic_store(&closed, 1); push("quit"); gtk_main_quit(); return FALSE; }

static void *run(void *arg) {
    (void)arg;
    int ok = gtk_init_check(NULL, NULL);
    if (ok) { gtk_window_set_default_icon_name("uu-remote-linux"); } // same taskbar icon as the host (#13)
    // The CSS hard-codes a light palette; a dark system theme would turn unstyled text white on white (#43).
    if (ok) g_object_set(gtk_settings_get_default(), "gtk-theme-name", "Adwaita", "gtk-application-prefer-dark-theme", FALSE, NULL);
    if (ok) {
        GtkCssProvider *css = gtk_css_provider_new();
        gtk_css_provider_load_from_data(css, CSS, -1, NULL);
        gtk_style_context_add_provider_for_screen(gdk_screen_get_default(), GTK_STYLE_PROVIDER(css), GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
        window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
        gtk_window_set_title(GTK_WINDOW(window), L("UU 远程 Linux 控制端（非官方）", "UU Remote Linux viewer (unofficial)"));
        gtk_window_set_default_size(GTK_WINDOW(window), 1180, 780);
        g_signal_connect(window, "delete-event", G_CALLBACK(on_delete), NULL);
        own_titlebar(window, L("UU 远程 Linux 控制端（非官方）", "UU Remote Linux viewer (unofficial)"));
        GtkWidget *root = hbox(0);
        // Sidebar
        GtkWidget *side = vbox(0); add_class(side, "sidebar"); gtk_widget_set_size_request(side, 270, -1);
        my_section = vbox(0);
        GtkWidget *mh = hbox(8); gtk_widget_set_margin_start(mh, 18); gtk_widget_set_margin_top(mh, 22); gtk_widget_set_margin_bottom(mh, 6);
        gtk_box_pack_start(GTK_BOX(mh), label(L("⧉  我的设备", "⧉  My devices"), "section"), TRUE, TRUE, 0);
        gtk_box_pack_start(GTK_BOX(my_section), mh, FALSE, FALSE, 0);
        device_list = gtk_list_box_new(); g_signal_connect(device_list, "row-activated", G_CALLBACK(on_row), NULL);
        gtk_box_pack_start(GTK_BOX(my_section), device_list, FALSE, FALSE, 0);
        GtkWidget *scroll = gtk_scrolled_window_new(NULL, NULL); gtk_scrolled_window_set_policy(GTK_SCROLLED_WINDOW(scroll), GTK_POLICY_NEVER, GTK_POLICY_AUTOMATIC);
        GtkWidget *side_inner = vbox(0); gtk_box_pack_start(GTK_BOX(side_inner), my_section, FALSE, FALSE, 0);
        GtkWidget *welcome_nav = gtk_list_box_new(); g_signal_connect(welcome_nav, "row-activated", G_CALLBACK(on_row), NULL);
        GtkWidget *ah = label(L("⧉  远程协助", "⧉  Remote assistance"), "section"); gtk_widget_set_margin_start(ah, 18); gtk_widget_set_margin_top(ah, 16); gtk_widget_set_margin_bottom(ah, 6);
        gtk_box_pack_start(GTK_BOX(side_inner), ah, FALSE, FALSE, 0);
        GtkWidget *assist = assist_list = gtk_list_box_new(); g_signal_connect(assist, "row-activated", G_CALLBACK(on_row), NULL);
        gtk_list_box_insert(GTK_LIST_BOX(assist), nav_row("◧", L("开始协助", "Start assistance"), "assist"), -1);
        gtk_list_box_insert(GTK_LIST_BOX(assist), nav_row("★", L("收藏设备", "Favorites"), "favorite"), -1);
        gtk_box_pack_start(GTK_BOX(side_inner), assist, FALSE, FALSE, 0);
        gtk_list_box_unselect_all(GTK_LIST_BOX(assist));
        (void)welcome_nav;
        gtk_container_add(GTK_CONTAINER(scroll), side_inner);
        gtk_box_pack_start(GTK_BOX(side), scroll, TRUE, TRUE, 0);
        gtk_box_pack_start(GTK_BOX(root), side, FALSE, FALSE, 0);
        // Main area
        GtkWidget *main = vbox(0), *header = hbox(10); add_class(header, "header");
        badge = gtk_label_new(""); add_class(badge, "badge");
        title_label = label(L("全部设备", "All devices"), "title");
        more_button = button("⋮", "hbtn"); g_signal_connect(more_button, "clicked", G_CALLBACK(on_more), NULL);
        gtk_box_pack_start(GTK_BOX(header), badge, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(header), title_label, FALSE, FALSE, 0);
        gtk_box_pack_start(GTK_BOX(header), more_button, FALSE, FALSE, 0);
        const char *icons[] = {"☰", "◉", "🔔"}, *kinds[] = {"menu", "account", "notice"};
        for (int i = 0; i < 3; i++) { GtkWidget *b = button(icons[i], "hbtn"); g_signal_connect(b, "clicked", G_CALLBACK(popup), (gpointer)kinds[i]); gtk_box_pack_end(GTK_BOX(header), b, FALSE, FALSE, 0); }
        gtk_box_pack_start(GTK_BOX(main), header, FALSE, FALSE, 0);
        GtkWidget *overlay = gtk_overlay_new();
        stack = gtk_stack_new();
        gtk_stack_add_named(GTK_STACK(stack), build_welcome(), "welcome");
        gtk_stack_add_named(GTK_STACK(stack), build_login(), "login");
        gtk_stack_add_named(GTK_STACK(stack), build_device(), "device");
        gtk_stack_add_named(GTK_STACK(stack), build_all(), "all");
        gtk_stack_add_named(GTK_STACK(stack), build_assist(), "assist");
        gtk_stack_add_named(GTK_STACK(stack), build_favorite(), "favorite");
        gtk_container_add(GTK_CONTAINER(overlay), stack);
        toast = gtk_label_new(""); add_class(toast, "toast"); gtk_widget_set_halign(toast, GTK_ALIGN_CENTER); gtk_widget_set_valign(toast, GTK_ALIGN_START); gtk_widget_set_margin_top(toast, 30);
        gtk_overlay_add_overlay(GTK_OVERLAY(overlay), toast);
        gtk_box_pack_start(GTK_BOX(main), overlay, TRUE, TRUE, 0);
        gtk_box_pack_start(GTK_BOX(root), main, TRUE, TRUE, 0);
        gtk_container_add(GTK_CONTAINER(window), root);
        gtk_widget_show_all(window);
        gtk_widget_hide(badge); gtk_widget_hide(more_button); gtk_widget_hide(toast); gtk_widget_hide(my_section);
        gtk_stack_set_visible_child_name(GTK_STACK(stack), "welcome");
    }
    pthread_mutex_lock(&init_lock); init_state = ok ? 1 : -1; pthread_cond_signal(&init_cond); pthread_mutex_unlock(&init_lock);
    if (ok) gtk_main();
    atomic_store(&closed, 1);
    return NULL;
}

int uu_viewer_ui_start(void) {
    if (ui_started) return init_state == 1 ? 0 : -1;
    ui_started = 1;
    if (pthread_create(&ui_thread, NULL, run, NULL)) return -1;
    pthread_mutex_lock(&init_lock); while (!init_state) pthread_cond_wait(&init_cond, &init_lock); pthread_mutex_unlock(&init_lock);
    return init_state == 1 ? 0 : -1;
}
/// Lines of "id\tname\tplatform\tonline\tcurrent".
void uu_viewer_ui_set_devices(const char *tsv) {
    pthread_mutex_lock(&d_lock);
    /* A refresh (every 30 s) must keep the open device page usable: re-select by id, not reset. */
    char keep[sizeof devices[0].id] = "";
    if (selected >= 0 && selected < device_count) snprintf(keep, sizeof keep, "%s", devices[selected].id);
    device_count = 0;
    char *copy = strdup(tsv ? tsv : ""), *save = NULL;
    for (char *line = strtok_r(copy, "\n", &save); line && device_count < 64; line = strtok_r(NULL, "\n", &save)) {
        char *f[6] = {0}, *s2 = NULL; int n = 0;
        for (char *p = strtok_r(line, "\t", &s2); p && n < 6; p = strtok_r(NULL, "\t", &s2)) f[n++] = p;
        if (n < 5) continue;
        Device *d = &devices[device_count++];
        snprintf(d->id, sizeof d->id, "%s", f[0]); snprintf(d->name, sizeof d->name, "%s", f[1]);
        d->platform = atoi(f[2]); d->online = atoi(f[3]); d->current = atoi(f[4]);
        snprintf(d->thumb, sizeof d->thumb, "%s", n > 5 && f[5][0] == '/' ? f[5] : "");
    }
    free(copy);
    selected = -1;
    for (int i = 0; keep[0] && i < device_count; i++) if (!strcmp(devices[i].id, keep)) { selected = i; break; }
    pthread_mutex_unlock(&d_lock);
    if (init_state == 1) g_idle_add(apply_devices, NULL);
}
void uu_viewer_ui_set_login(int signed_in, const char *qr_png, const char *status, const char *user) {
    logged_in = signed_in;
    snprintf(qr_path, sizeof qr_path, "%s", qr_png ? qr_png : "");
    snprintf(status_text, sizeof status_text, "%s", status ? status : "");
    snprintf(user_text, sizeof user_text, "%s", user ? user : "");
    if (init_state == 1) g_idle_add(apply_login, NULL);
}
int uu_viewer_ui_take_command(char *out, size_t len) {
    pthread_mutex_lock(&q_lock);
    int has = q_len > 0;
    if (has) { snprintf(out, len, "%s", queue[q_head]); memset(queue[q_head], 0, sizeof queue[q_head]); q_head = (q_head + 1) % 16; q_len--; }
    pthread_mutex_unlock(&q_lock);
    return has;
}
int uu_viewer_ui_closed(void) { return atomic_load(&closed); }
static char toast_text[256];
static gboolean apply_toast(gpointer d) { (void)d; show_toast(toast_text); return G_SOURCE_REMOVE; }
void uu_viewer_ui_toast(const char *text) {
    snprintf(toast_text, sizeof toast_text, "%s", text ? text : "");
    if (init_state == 1) g_idle_add(apply_toast, NULL);
}

static int replace_result;
static gboolean confirm_replace(gpointer d) {
    (void)d;
    GtkWidget *dialog = gtk_message_dialog_new(GTK_WINDOW(window), GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT,
        GTK_MESSAGE_QUESTION, GTK_BUTTONS_NONE, L("打开后将断开当前与该设备的连接，继续吗？", "Opening this disconnects your current connection to this device. Continue?"));
    gtk_dialog_add_buttons(GTK_DIALOG(dialog), L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("继续", "Continue"), GTK_RESPONSE_OK, NULL);
    int accepted = gtk_dialog_run(GTK_DIALOG(dialog)) == GTK_RESPONSE_OK;
    gtk_widget_destroy(dialog);
    pthread_mutex_lock(&init_lock); replace_result = accepted; pthread_cond_signal(&init_cond); pthread_mutex_unlock(&init_lock);
    return G_SOURCE_REMOVE;
}
int uu_viewer_ui_confirm_replace(void) {
    pthread_mutex_lock(&init_lock); replace_result = -1; g_idle_add(confirm_replace, NULL);
    while (replace_result < 0 && !atomic_load(&closed)) {
        struct timespec until; clock_gettime(CLOCK_REALTIME, &until); until.tv_sec++;
        pthread_cond_timedwait(&init_cond, &init_lock, &until);
    }
    int result = replace_result; pthread_mutex_unlock(&init_lock); return result;
}
