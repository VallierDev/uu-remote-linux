// GTK3 window for guest remote assistance (`uuremote-server assist`). Layout follows the official
// "本设备" card (allow switch, device ID, temporary code with show/hide, copy & share) without any
// NetEase assets. GTK runs on its own thread; Rust pushes state through g_idle_add and polls the
// user's commands. The code is only drawn in this local window and copied on explicit request.
#include <gtk/gtk.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include "i18n.h"
static void on_language(GtkComboBox *c, gpointer note);

static const char *CSS =
    "window { background-color: #f5f6f8; }\n"
    "* { font-family: 'Noto Sans CJK SC', 'Noto Sans SC', sans-serif; }\n"
    ".card { background-color: #ffffff; border-radius: 10px; border: 1px solid #e5e7eb; }\n"
    ".heading { font-size: 20px; font-weight: 700; color: #1d1d1f; }\n"
    ".key { font-size: 14px; color: #4b5563; }\n"
    ".big { font-size: 32px; font-weight: 800; color: #111111; }\n"
    ".note { font-size: 13px; color: #9ca3af; }\n"
    ".hint { font-size: 13px; color: #6b7280; }\n"
    ".footer { font-size: 12px; color: #9ca3af; }\n"
    "button.flat { padding: 4px 8px; }\n"
    "button.share { background-image: none; background-color: #ffffff; border: 1px solid #d1d5db;"
    " border-radius: 6px; padding: 6px 14px; font-size: 14px; }\n"
    "separator { background-color: #e5e7eb; min-height: 1px; }\n"
    ".user { font-size: 22px; font-weight: 700; color: #111111; }\n"
    ".status { font-size: 13px; color: #2f6bff; }\n"
    ".status.ok { color: #16a34a; }\n"
    ".status.error { color: #dc2626; }\n";

enum { CMD_NONE = 0, CMD_ALLOW = 1, CMD_DENY = 2, CMD_RESET = 3, CMD_STOP = 4, CMD_VERIFY = 5, CMD_REFRESH = 6, CMD_CUSTOM = 7, CMD_LOGOUT = 8, CMD_QUIT = 9 };
enum { VERIFY_TEMPORARY = 0, VERIFY_CUSTOM = 1, VERIFY_BOTH = 2 };

typedef struct { char *id; char *code; int allowed, verify, refresh, has_custom; } State;
typedef struct { int cmd; char arg[40]; } Cmd;

// Small FIFO so quick successive UI actions (e.g. set code, then mode) are all delivered.
static Cmd queue[16];
static int q_head, q_len;
static pthread_mutex_t q_lock = PTHREAD_MUTEX_INITIALIZER;
static atomic_int closed;
static pthread_t ui_thread;
static int ui_started, init_state; // 0 pending, 1 ok, -1 failed
static pthread_mutex_t init_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t init_cond = PTHREAD_COND_INITIALIZER;
static GtkWidget *window, *allow_switch, *id_label, *code_label, *eye_button, *share_button, *hint_label, *verify_combo, *note_label;
static GtkWidget *refresh_items[5], *footer_label;
// Account card: QR login before sign-in, user info (+ logout) after.
static GtkWidget *assist_card, *account_card, *account_stack, *qr_stack, *qr_image, *login_status, *user_label, *qr_refresh;
/* Like the official client, a failed or timed-out QR stays until the user asks for a new one. */
static atomic_int qr_refresh_requested;
static void on_qr_refresh(GtkButton *b, gpointer d) { (void)b; (void)d; atomic_store(&qr_refresh_requested, 1); gtk_widget_hide(qr_refresh); }
static int show_assist = 1, show_account;
// Tray (host service only): AppIndicator via dlopen (no build dependency), else GtkStatusIcon.
static GtkWidget *tray_menu, *tray_allow;
static int suppress_tray;
static int cur_verify, cur_refresh, cur_has_custom;
static char current_id[64], current_code[64];
static int code_visible = 1, suppress_switch, start_hidden, close_hides;
// A problem only the person at the desktop can fix (e.g. FFmpeg missing): red line at the top.
static GtkWidget *problem_label;
static gchar *problem_text;
static pthread_mutex_t problem_lock = PTHREAD_MUTEX_INITIALIZER;

static GtkWidget *label(const char *text, const char *cls) {
    GtkWidget *l = gtk_label_new(text);
    gtk_style_context_add_class(gtk_widget_get_style_context(l), cls);
    gtk_widget_set_halign(l, GTK_ALIGN_START);
    return l;
}

static void post_arg(int cmd, const char *arg) {
    pthread_mutex_lock(&q_lock);
    if (q_len < 16) {
        Cmd *c = &queue[(q_head + q_len) % 16];
        c->cmd = cmd; g_strlcpy(c->arg, arg ? arg : "", sizeof c->arg); q_len++;
    }
    pthread_mutex_unlock(&q_lock);
}
static void post(int cmd) { post_arg(cmd, NULL); }
static void post_int(int cmd, int v) { char b[8]; g_snprintf(b, sizeof b, "%d", v); post_arg(cmd, b); }

// 8-16 ASCII letters/digits including both, as the official custom code dialog requires.
static int valid_custom(const char *c) {
    size_t n = strlen(c); int alpha = 0, digit = 0;
    if (n < 8 || n > 16) return 0;
    for (size_t i = 0; i < n; i++) {
        if (g_ascii_isalpha(c[i])) alpha = 1; else if (g_ascii_isdigit(c[i])) digit = 1; else return 0;
    }
    return alpha && digit;
}

static void on_custom_changed(GtkEditable *e, gpointer ok) {
    gtk_widget_set_sensitive(GTK_WIDGET(ok), valid_custom(gtk_entry_get_text(GTK_ENTRY(e))));
}

// Returns 1 when a new custom code was entered and posted.
static int ask_custom_code(void) {
    GtkWidget *d = gtk_dialog_new_with_buttons(L("设置自定义验证码", "Set custom code"), GTK_WINDOW(window), GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT,
                                               L("取消", "Cancel"), GTK_RESPONSE_CANCEL, NULL);
    GtkWidget *ok = gtk_dialog_add_button(GTK_DIALOG(d), L("确定", "OK"), GTK_RESPONSE_OK);
    gtk_widget_set_sensitive(ok, FALSE);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(d));
    gtk_container_set_border_width(GTK_CONTAINER(area), 16);
    gtk_box_set_spacing(GTK_BOX(area), 8);
    GtkWidget *entry = gtk_entry_new();
    gtk_entry_set_max_length(GTK_ENTRY(entry), 16);
    gtk_entry_set_visibility(GTK_ENTRY(entry), FALSE);
    gtk_entry_set_placeholder_text(GTK_ENTRY(entry), L("请输入自定义验证码", "Enter custom code"));
    g_signal_connect(entry, "changed", G_CALLBACK(on_custom_changed), ok);
    gtk_box_pack_start(GTK_BOX(area), entry, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(area), label(L("8-16 位，需同时包含字母和数字；不会自动刷新，请妥善保管", "8-16 characters with both letters and digits; never refreshed automatically, keep it safe"), "note"), FALSE, FALSE, 0);
    gtk_widget_show_all(d);
    int done = 0;
    if (gtk_dialog_run(GTK_DIALOG(d)) == GTK_RESPONSE_OK) {
        const char *code = gtk_entry_get_text(GTK_ENTRY(entry));
        if (valid_custom(code)) { post_arg(CMD_CUSTOM, code); done = 1; }
    }
    gtk_widget_destroy(d);
    return done;
}

static int suppress_combo;
static void on_verify_changed(GtkComboBox *c, gpointer d) {
    (void)d;
    if (suppress_combo) return;
    int v = gtk_combo_box_get_active(c);
    if (v != VERIFY_TEMPORARY && !cur_has_custom && !ask_custom_code()) {
        suppress_combo = 1; gtk_combo_box_set_active(c, cur_verify); suppress_combo = 0;
        return;
    }
    post_int(CMD_VERIFY, v);
}
static void on_set_custom(GtkButton *b, gpointer d) { (void)b; (void)d; ask_custom_code(); }

static int suppress_refresh;
static void on_refresh_item(GtkCheckMenuItem *item, gpointer data) {
    if (suppress_refresh || !gtk_check_menu_item_get_active(item)) return;
    post_int(CMD_REFRESH, GPOINTER_TO_INT(data));
}
static void on_refresh_now(GtkMenuItem *i, gpointer d) { (void)i; (void)d; post(CMD_RESET); }

static const char *refresh_note(int i) { const char *notes[5] = { L("每次远控结束刷新", "Refreshed after every remote session"), L("每小时自动刷新", "Refreshed every hour"), L("每 12 小时自动刷新", "Refreshed every 12 hours"), L("每 24 小时自动刷新", "Refreshed every 24 hours"), L("不自动刷新", "Never refreshed automatically") }; return notes[i >= 0 && i < 5 ? i : 0]; }

static void render_code(void) {
    if (!code_label) return;
    gtk_label_set_text(GTK_LABEL(code_label), current_code[0] ? (code_visible ? current_code : "********") : "--------");
    gtk_button_set_label(GTK_BUTTON(eye_button), code_visible ? L("隐藏", "Hide") : L("显示", "Show"));
}

static void copy_text(const char *text, const char *hint) {
    if (!text || !text[0]) return;
    gtk_clipboard_set_text(gtk_clipboard_get(GDK_SELECTION_CLIPBOARD), text, -1);
    gtk_label_set_text(GTK_LABEL(hint_label), hint);
}
// The ID is copied without the display spaces so it pastes straight into the viewer's ID field.
static void on_copy_id(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    char digits[64]; size_t n = 0;
    for (const char *p = current_id; *p && n + 1 < sizeof digits; p++) if (*p != ' ') digits[n++] = *p;
    digits[n] = 0;
    copy_text(digits, L("已复制设备 ID", "Device ID copied"));
}
static void on_copy_code(GtkButton *b, gpointer d) { (void)b; (void)d; copy_text(current_code, L("已复制验证码，请只发给信任的人", "Code copied. Only share it with people you trust")); }

static void on_eye(GtkButton *b, gpointer d) { (void)b; (void)d; code_visible = !code_visible; render_code(); }

static void on_share(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    if (!current_id[0] || !current_code[0]) return;
    char *text = g_strdup_printf(L("UU 远程协助\n设备 ID：%s\n验证码：%s", "UU Remote assistance\nDevice ID: %s\nCode: %s"), current_id, current_code);
    gtk_clipboard_set_text(gtk_clipboard_get(GDK_SELECTION_CLIPBOARD), text, -1);
    g_free(text);
    gtk_label_set_text(GTK_LABEL(hint_label), L("已复制设备 ID 和验证码，请只发给信任的人", "Device ID and code copied. Only share them with people you trust"));
}

static gboolean on_switch(GtkSwitch *s, gboolean state, gpointer d) {
    (void)s; (void)d;
    if (!suppress_switch) post(state ? CMD_ALLOW : CMD_DENY);
    return FALSE;
}

// Host service: the close button only hides the window (assistance keeps its own on/off switch).
static gboolean on_delete(GtkWidget *w, GdkEvent *e, gpointer d) {
    (void)e; (void)d;
    if (!close_hides) return FALSE;
    gtk_widget_hide(w);
    return TRUE;
}

static void on_destroy(GtkWidget *w, gpointer d) {
    (void)w; (void)d;
    window = code_label = NULL;
    atomic_store(&closed, 1);
    post(CMD_STOP);
    gtk_main_quit();
}

static gboolean draw_avatar(GtkWidget *w, cairo_t *cr, gpointer d) {
    (void)w; (void)d;
    double s = 64;
    cairo_arc(cr, s / 2, s / 2, s / 2 - 1, 0, 2 * G_PI);
    cairo_set_source_rgb(cr, 0.84, 0.90, 1.0); cairo_fill_preserve(cr);
    cairo_set_source_rgb(cr, 0.36, 0.55, 1.0); cairo_set_line_width(cr, 2); cairo_stroke(cr);
    cairo_save(cr);
    cairo_arc(cr, s / 2, s / 2, s / 2 - 2, 0, 2 * G_PI); cairo_clip(cr);
    cairo_set_source_rgb(cr, 0.27, 0.51, 1.0);
    cairo_arc(cr, s / 2, s * 0.38, s * 0.16, 0, 2 * G_PI); cairo_fill(cr);
    cairo_arc(cr, s / 2, s * 0.92, s * 0.32, G_PI, 2 * G_PI); cairo_fill(cr);
    cairo_restore(cr);
    return TRUE;
}
static void on_logout(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    GtkWidget *dlg = gtk_message_dialog_new(GTK_WINDOW(window), GTK_DIALOG_MODAL, GTK_MESSAGE_QUESTION, GTK_BUTTONS_OK_CANCEL,
                                            L("退出登录后，本机将从你的设备列表中下线，远程协助会改用访客身份。确定退出吗？", "After signing out this computer leaves your device list and assistance switches to guest mode. Sign out?"));
    if (gtk_dialog_run(GTK_DIALOG(dlg)) == GTK_RESPONSE_OK) {
        post(CMD_LOGOUT);
        gtk_label_set_text(GTK_LABEL(user_label), L("正在退出登录…", "Signing out…"));
    }
    gtk_widget_destroy(dlg);
}

static GtkWidget *build_account_card(void) {
    GtkWidget *card = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_style_context_add_class(gtk_widget_get_style_context(card), "card");
    GtkWidget *header = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 10);
    gtk_container_set_border_width(GTK_CONTAINER(header), 20);
    gtk_box_pack_start(GTK_BOX(header), label(L("账号", "Account"), "heading"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(card), header, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(card), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0);
    account_stack = gtk_stack_new();
    gtk_stack_set_transition_type(GTK_STACK(account_stack), GTK_STACK_TRANSITION_TYPE_CROSSFADE);
    gtk_stack_set_vhomogeneous(GTK_STACK(account_stack), FALSE); // user page is shorter than the QR page

    // Not signed in: QR code + guidance.
    GtkWidget *qr_page = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 24);
    gtk_container_set_border_width(GTK_CONTAINER(qr_page), 20);
    qr_stack = gtk_stack_new();
    gtk_widget_set_size_request(qr_stack, 180, 180);
    GtkWidget *loading = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
    gtk_widget_set_valign(loading, GTK_ALIGN_CENTER);
    GtkWidget *spinner = gtk_spinner_new();
    gtk_spinner_start(GTK_SPINNER(spinner));
    gtk_box_pack_start(GTK_BOX(loading), spinner, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(loading), label(L("正在获取二维码…", "Loading QR code…"), "note"), FALSE, FALSE, 0);
    gtk_stack_add_named(GTK_STACK(qr_stack), loading, "loading");
    qr_image = gtk_image_new();
    gtk_stack_add_named(GTK_STACK(qr_stack), qr_image, "qr");
    gtk_box_pack_start(GTK_BOX(qr_page), qr_stack, FALSE, FALSE, 0);
    GtkWidget *qr_text = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
    gtk_widget_set_valign(qr_text, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(qr_text), label(L("扫码登录", "Scan to sign in"), "heading"), FALSE, FALSE, 0);
    GtkWidget *cap = label(L("请使用网易 UU 远程 App 扫一扫登录。\n登录后本机会出现在你的设备列表中，可直接远程控制；\n远程协助在登录前后都可以使用。", "Scan with the NetEase UU Remote app to sign in.\nAfter sign-in this computer appears in your device list and can be controlled directly;\nremote assistance works whether you are signed in or not."), "hint");
    gtk_label_set_xalign(GTK_LABEL(cap), 0);
    gtk_box_pack_start(GTK_BOX(qr_text), cap, FALSE, FALSE, 0);
    login_status = label("", "status");
    gtk_label_set_line_wrap(GTK_LABEL(login_status), TRUE);
    gtk_box_pack_start(GTK_BOX(qr_text), login_status, FALSE, FALSE, 0);
    qr_refresh = gtk_button_new_with_label(L("刷新二维码", "Refresh QR code"));
    gtk_widget_set_halign(qr_refresh, GTK_ALIGN_START);
    gtk_widget_set_no_show_all(qr_refresh, TRUE);
    g_signal_connect(qr_refresh, "clicked", G_CALLBACK(on_qr_refresh), NULL);
    gtk_box_pack_start(GTK_BOX(qr_text), qr_refresh, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(qr_page), qr_text, TRUE, TRUE, 0);
    gtk_stack_add_named(GTK_STACK(account_stack), qr_page, "qr");

    // Signed in: avatar + account name + logout.
    GtkWidget *user_page = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 16);
    gtk_container_set_border_width(GTK_CONTAINER(user_page), 20);
    GtkWidget *avatar = gtk_drawing_area_new();
    gtk_widget_set_size_request(avatar, 64, 64);
    gtk_widget_set_valign(avatar, GTK_ALIGN_CENTER);
    g_signal_connect(avatar, "draw", G_CALLBACK(draw_avatar), NULL);
    gtk_box_pack_start(GTK_BOX(user_page), avatar, FALSE, FALSE, 0);
    GtkWidget *user_text = gtk_box_new(GTK_ORIENTATION_VERTICAL, 4);
    gtk_widget_set_valign(user_text, GTK_ALIGN_CENTER);
    user_label = label("", "user");
    gtk_box_pack_start(GTK_BOX(user_text), user_label, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(user_text), label(L("已登录 · 本机已出现在你的设备列表中", "Signed in · this computer is in your device list"), "hint"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(user_page), user_text, TRUE, TRUE, 0);
    GtkWidget *logout = gtk_button_new_with_label(L("退出登录", "Sign out"));
    gtk_style_context_add_class(gtk_widget_get_style_context(logout), "share");
    gtk_widget_set_valign(logout, GTK_ALIGN_CENTER);
    g_signal_connect(logout, "clicked", G_CALLBACK(on_logout), NULL);
    gtk_box_pack_end(GTK_BOX(user_page), logout, FALSE, FALSE, 0);
    gtk_stack_add_named(GTK_STACK(account_stack), user_page, "user");

    gtk_box_pack_start(GTK_BOX(card), account_stack, FALSE, FALSE, 0);
    return card;
}

static void present_window(void) { if (window) { gtk_widget_show(window); gtk_window_present(GTK_WINDOW(window)); } }
static void on_tray_open(GtkMenuItem *i, gpointer d) { (void)i; (void)d; present_window(); }
static void on_tray_allow(GtkCheckMenuItem *i, gpointer d) {
    (void)d;
    if (!suppress_tray) post(gtk_check_menu_item_get_active(i) ? CMD_ALLOW : CMD_DENY);
}
static void on_tray_quit(GtkMenuItem *i, gpointer d) {
    (void)i; (void)d;
    GtkWidget *dlg = gtk_message_dialog_new(NULL, GTK_DIALOG_MODAL, GTK_MESSAGE_QUESTION, GTK_BUTTONS_OK_CANCEL,
                                            L("退出后本机将下线：设备列表和远程协助都无法连接，直到再次启动。确定退出吗？", "Quitting takes this computer offline: neither the device list nor remote assistance can connect until it starts again. Quit?"));
    gtk_window_set_title(GTK_WINDOW(dlg), L("UU 远程 Linux 主机", "UU Remote Linux host"));
    if (gtk_dialog_run(GTK_DIALOG(dlg)) == GTK_RESPONSE_OK) post(CMD_QUIT);
    gtk_widget_destroy(dlg);
}
static void on_status_activate(GtkStatusIcon *i, gpointer d) { (void)i; (void)d; present_window(); }
static void on_status_menu(GtkStatusIcon *i, guint button, guint time, gpointer d) {
    (void)i; (void)button; (void)time; (void)d;
    gtk_menu_popup_at_pointer(GTK_MENU(tray_menu), NULL);
}
// Monochrome tray icon that follows light/dark panels: GTK and the GNOME AppIndicator extension
// recolor "-symbolic" names, KDE recolors the ColorScheme-Text class. Installed into the user's
// hicolor theme (found by every panel's default lookup) and also offered as the icon theme path,
// so it needs no package install. KDE tries dash-trimmed names ("uu-remote-linux") in each theme
// before the next one, so the name must not trim into the colored app icon (#87).
static const char TRAY_ICON_NAME[] = "uuremote-tray-symbolic";
static const char TRAY_ICON_SVG[] =
    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" viewBox=\"0 0 16 16\">"
    "<style id=\"current-color-scheme\">.ColorScheme-Text{color:#232629}</style>"
    "<path class=\"ColorScheme-Text\" fill=\"currentColor\" fill-rule=\"evenodd\" d=\"M2.5 2h11A1.5 1.5 0 0 1 15 3.5v7a1.5 1.5 0 0 1-1.5 1.5h-11"
    "A1.5 1.5 0 0 1 1 10.5v-7A1.5 1.5 0 0 1 2.5 2zm0 1.5v7h11v-7z\"/>"
    "<path class=\"ColorScheme-Text\" fill=\"currentColor\" d=\"M7.25 12h1.5v1.5H11a.75.75 0 0 1 0 1.5H5a.75.75 0 0 1 0-1.5h2.25z"
    "M11.5 4.5v4l-1.35-1.35-3.1 3.1-1.2-1.2 3.1-3.1L7.5 4.5z\"/></svg>\n";
// Returns the icon theme root holding the icon ($XDG_DATA_HOME/icons), or NULL.
static gchar *write_tray_icon(void) {
    gchar *root = g_build_filename(g_get_user_data_dir(), "icons", NULL);
    gchar *apps = g_build_filename(root, "hicolor", "scalable", "apps", NULL);
    gchar *file = g_strconcat(TRAY_ICON_NAME, ".svg", NULL);
    gchar *path = g_build_filename(apps, file, NULL), *current = NULL;
    // Rewriting an unchanged file would only make icon caches rescan.
    int ok = g_file_get_contents(path, &current, NULL, NULL) && strcmp(current, TRAY_ICON_SVG) == 0;
    if (!ok) ok = g_mkdir_with_parents(apps, 0755) == 0 && g_file_set_contents(path, TRAY_ICON_SVG, -1, NULL);
    g_free(apps); g_free(file); g_free(path); g_free(current);
    if (!ok) { g_free(root); return NULL; }
    return root;
}
static void build_tray(void) {
    tray_menu = gtk_menu_new();
    GtkWidget *open = gtk_menu_item_new_with_label(L("打开主窗口", "Open main window"));
    g_signal_connect(open, "activate", G_CALLBACK(on_tray_open), NULL);
    tray_allow = gtk_check_menu_item_new_with_label(L("允许他人远程协助", "Allow remote assistance"));
    g_signal_connect(tray_allow, "toggled", G_CALLBACK(on_tray_allow), NULL);
    GtkWidget *quit = gtk_menu_item_new_with_label(L("退出", "Quit"));
    g_signal_connect(quit, "activate", G_CALLBACK(on_tray_quit), NULL);
    gtk_menu_shell_append(GTK_MENU_SHELL(tray_menu), open);
    gtk_menu_shell_append(GTK_MENU_SHELL(tray_menu), tray_allow);
    gtk_menu_shell_append(GTK_MENU_SHELL(tray_menu), gtk_separator_menu_item_new());
    gtk_menu_shell_append(GTK_MENU_SHELL(tray_menu), quit);
    gtk_widget_show_all(tray_menu);
    gchar *icon_path = write_tray_icon();
    if (icon_path) gtk_icon_theme_append_search_path(gtk_icon_theme_get_default(), icon_path);
    const char *fallback = gtk_icon_theme_has_icon(gtk_icon_theme_get_default(), "uu-remote-linux") ? "uu-remote-linux" : "computer";
    const char *icon = icon_path ? TRAY_ICON_NAME : fallback;

    // StatusNotifierItem (GNOME AppIndicator extension, KDE): libayatana-appindicator3 / libappindicator3.
    const char *libs[] = { "libayatana-appindicator3.so.1", "libappindicator3.so.1" };
    for (int i = 0; i < 2; i++) {
        void *h = dlopen(libs[i], RTLD_LAZY | RTLD_LOCAL);
        if (!h) continue;
        void *(*ind_new)(const char *, const char *, int) = (void *(*)(const char *, const char *, int))dlsym(h, "app_indicator_new");
        void (*set_status)(void *, int) = (void (*)(void *, int))dlsym(h, "app_indicator_set_status");
        void (*set_menu)(void *, GtkMenu *) = (void (*)(void *, GtkMenu *))dlsym(h, "app_indicator_set_menu");
        void (*set_title)(void *, const char *) = (void (*)(void *, const char *))dlsym(h, "app_indicator_set_title");
        void (*set_theme_path)(void *, const char *) = (void (*)(void *, const char *))dlsym(h, "app_indicator_set_icon_theme_path");
        if (ind_new && set_status && set_menu) {
            // Without a theme path the panel cannot find the cached icon; keep the installed one.
            void *ind = ind_new("uu-remote-linux", set_theme_path ? icon : fallback, 0 /* APPLICATION_STATUS */);
            if (icon_path && set_theme_path) set_theme_path(ind, icon_path);
            if (set_title) set_title(ind, L("UU 远程 Linux 主机", "UU Remote Linux host"));
            set_menu(ind, GTK_MENU(tray_menu));
            set_status(ind, 1 /* ACTIVE */);
            return; // keep the library loaded for the indicator's lifetime
        }
        dlclose(h);
    }
    // XEmbed system tray (Xfce, Cinnamon, MATE...).
    G_GNUC_BEGIN_IGNORE_DEPRECATIONS
    GtkStatusIcon *status = gtk_status_icon_new_from_icon_name(icon);
    gtk_status_icon_set_tooltip_text(status, L("UU 远程 Linux 主机", "UU Remote Linux host"));
    gtk_status_icon_set_title(status, L("UU 远程 Linux 主机", "UU Remote Linux host"));
    g_signal_connect(status, "activate", G_CALLBACK(on_status_activate), NULL);
    g_signal_connect(status, "popup-menu", G_CALLBACK(on_status_menu), NULL);
    G_GNUC_END_IGNORE_DEPRECATIONS
}

static void build_window(void) {
    GtkCssProvider *css = gtk_css_provider_new();
    gtk_css_provider_load_from_data(css, CSS, -1, NULL);
    gtk_style_context_add_provider_for_screen(gdk_screen_get_default(), GTK_STYLE_PROVIDER(css),
                                              GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    g_object_unref(css);

    gtk_window_set_default_icon_name("uu-remote-linux"); // contrib/desktop icon, when installed
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), L("UU 远程 Linux 主机（非官方）", "UU Remote Linux host (unofficial)"));
    gtk_window_set_default_size(GTK_WINDOW(window), 760, -1);
    gtk_window_set_resizable(GTK_WINDOW(window), FALSE);
    gtk_window_set_position(GTK_WINDOW(window), GTK_WIN_POS_CENTER);
    g_signal_connect(window, "delete-event", G_CALLBACK(on_delete), NULL);
    g_signal_connect(window, "destroy", G_CALLBACK(on_destroy), NULL);

    GtkWidget *root = gtk_box_new(GTK_ORIENTATION_VERTICAL, 10);
    gtk_container_set_border_width(GTK_CONTAINER(root), 24);
    gtk_container_add(GTK_CONTAINER(window), root);
    problem_label = label("", "status");
    gtk_style_context_add_class(gtk_widget_get_style_context(problem_label), "error");
    gtk_label_set_line_wrap(GTK_LABEL(problem_label), TRUE);
    gtk_label_set_max_width_chars(GTK_LABEL(problem_label), 40);
    gtk_box_pack_start(GTK_BOX(root), problem_label, FALSE, FALSE, 0);

    GtkWidget *card = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_style_context_add_class(gtk_widget_get_style_context(card), "card");
    gtk_box_pack_start(GTK_BOX(root), card, FALSE, FALSE, 0);
    assist_card = card;

    // Header: 本设备 ... 允许他人远程协助 [switch]
    GtkWidget *header = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 10);
    gtk_container_set_border_width(GTK_CONTAINER(header), 20);
    gtk_box_pack_start(GTK_BOX(header), label(L("本设备", "This device"), "heading"), FALSE, FALSE, 0);
    allow_switch = gtk_switch_new();
    gtk_widget_set_valign(allow_switch, GTK_ALIGN_CENTER);
    g_signal_connect(allow_switch, "state-set", G_CALLBACK(on_switch), NULL);
    gtk_box_pack_end(GTK_BOX(header), allow_switch, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(header), label(L("允许他人远程协助", "Allow remote assistance"), "key"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(card), header, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(card), gtk_separator_new(GTK_ORIENTATION_HORIZONTAL), FALSE, FALSE, 0);

    // Body: ID | code + controls
    GtkWidget *body = gtk_grid_new();
    gtk_container_set_border_width(GTK_CONTAINER(body), 20);
    gtk_grid_set_column_spacing(GTK_GRID(body), 48);
    gtk_grid_set_row_spacing(GTK_GRID(body), 6);
    gtk_grid_attach(GTK_GRID(body), label(L("本设备 ID", "This device's ID"), "key"), 0, 0, 1, 1);
    GtkWidget *id_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    id_label = label(L("获取中…", "Loading…"), "big");
    gtk_label_set_selectable(GTK_LABEL(id_label), TRUE);
    gtk_box_pack_start(GTK_BOX(id_row), id_label, FALSE, FALSE, 0);
    GtkWidget *copy_id = gtk_button_new_with_label(L("复制", "Copy"));
    gtk_style_context_add_class(gtk_widget_get_style_context(copy_id), "flat");
    gtk_widget_set_valign(copy_id, GTK_ALIGN_CENTER);
    g_signal_connect(copy_id, "clicked", G_CALLBACK(on_copy_id), NULL);
    gtk_box_pack_start(GTK_BOX(id_row), copy_id, FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(body), id_row, 0, 1, 1, 1);

    GtkWidget *verify_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 6);
    gtk_box_pack_start(GTK_BOX(verify_row), label(L("验证方式：", "Verification:"), "key"), FALSE, FALSE, 0);
    verify_combo = gtk_combo_box_text_new();
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(verify_combo), L("仅使用临时验证码", "Temporary code only"));
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(verify_combo), L("仅使用自定义验证码", "Custom code only"));
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(verify_combo), L("同时使用两种验证码", "Use both codes"));
    gtk_combo_box_set_active(GTK_COMBO_BOX(verify_combo), 0);
    g_signal_connect(verify_combo, "changed", G_CALLBACK(on_verify_changed), NULL);
    gtk_box_pack_start(GTK_BOX(verify_row), verify_combo, FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(body), verify_row, 1, 0, 1, 1);
    GtkWidget *code_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    code_label = label("--------", "big");
    gtk_box_pack_start(GTK_BOX(code_row), code_label, FALSE, FALSE, 0);
    eye_button = gtk_button_new_with_label(L("隐藏", "Hide"));
    gtk_style_context_add_class(gtk_widget_get_style_context(eye_button), "flat");
    g_signal_connect(eye_button, "clicked", G_CALLBACK(on_eye), NULL);
    // Gear menu: 立即刷新 / 自动刷新 ▸ (每次远控结束, 每小时, 每 12 小时, 每 24 小时, 关闭) / 设置自定义验证码
    GtkWidget *menu = gtk_menu_new();
    GtkWidget *now = gtk_menu_item_new_with_label(L("立即刷新", "Refresh now"));
    g_signal_connect(now, "activate", G_CALLBACK(on_refresh_now), NULL);
    gtk_menu_shell_append(GTK_MENU_SHELL(menu), now);
    GtkWidget *auto_item = gtk_menu_item_new_with_label(L("自动刷新", "Auto refresh"));
    GtkWidget *sub = gtk_menu_new();
    const char *names[5] = { L("每次远控结束", "After every remote session"), L("每小时", "Every hour"), L("每 12 小时", "Every 12 hours"), L("每 24 小时", "Every 24 hours"), L("关闭", "Close") };
    GSList *group = NULL;
    for (int i = 0; i < 5; i++) {
        refresh_items[i] = gtk_radio_menu_item_new_with_label(group, names[i]);
        group = gtk_radio_menu_item_get_group(GTK_RADIO_MENU_ITEM(refresh_items[i]));
        g_signal_connect(refresh_items[i], "toggled", G_CALLBACK(on_refresh_item), GINT_TO_POINTER(i));
        gtk_menu_shell_append(GTK_MENU_SHELL(sub), refresh_items[i]);
    }
    gtk_menu_item_set_submenu(GTK_MENU_ITEM(auto_item), sub);
    gtk_menu_shell_append(GTK_MENU_SHELL(menu), auto_item);
    GtkWidget *custom_item = gtk_menu_item_new_with_label(L("设置自定义验证码…", "Set custom code…"));
    g_signal_connect(custom_item, "activate", G_CALLBACK(on_set_custom), NULL);
    gtk_menu_shell_append(GTK_MENU_SHELL(menu), custom_item);
    gtk_widget_show_all(menu);
    GtkWidget *gear = gtk_menu_button_new();
    gtk_button_set_image(GTK_BUTTON(gear), gtk_image_new_from_icon_name("emblem-system-symbolic", GTK_ICON_SIZE_BUTTON));
    gtk_widget_set_tooltip_text(gear, L("刷新设置", "Refresh settings"));
    gtk_menu_button_set_popup(GTK_MENU_BUTTON(gear), menu);
    GtkWidget *copy_code = gtk_button_new_with_label(L("复制", "Copy"));
    gtk_style_context_add_class(gtk_widget_get_style_context(copy_code), "flat");
    gtk_widget_set_valign(copy_code, GTK_ALIGN_CENTER);
    g_signal_connect(copy_code, "clicked", G_CALLBACK(on_copy_code), NULL);
    gtk_widget_set_valign(eye_button, GTK_ALIGN_CENTER);
    gtk_widget_set_valign(gear, GTK_ALIGN_CENTER);
    gtk_box_pack_start(GTK_BOX(code_row), copy_code, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(code_row), eye_button, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(code_row), gear, FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(body), code_row, 1, 1, 1, 1);
    note_label = label(refresh_note(0), "note");
    gtk_grid_attach(GTK_GRID(body), note_label, 1, 2, 1, 1);

    share_button = gtk_button_new_with_label(L("复制并分享", "Copy and share"));
    gtk_style_context_add_class(gtk_widget_get_style_context(share_button), "share");
    gtk_widget_set_valign(share_button, GTK_ALIGN_CENTER);
    g_signal_connect(share_button, "clicked", G_CALLBACK(on_share), NULL);
    gtk_grid_attach(GTK_GRID(body), share_button, 2, 1, 1, 1);
    gtk_box_pack_start(GTK_BOX(card), body, FALSE, FALSE, 0);

    hint_label = label("", "hint");
    gtk_box_pack_start(GTK_BOX(root), hint_label, FALSE, FALSE, 0);
    account_card = build_account_card();
    gtk_widget_set_margin_top(account_card, 6);
    gtk_box_pack_start(GTK_BOX(root), account_card, FALSE, FALSE, 0);
    footer_label = label(L("开源 Linux 被控端 · 非网易官方客户端 · 关闭窗口即停止远程协助", "Open-source Linux host · not an official NetEase client · closing the window stops remote assistance"), "footer");
    gtk_box_pack_end(GTK_BOX(root), footer_label, FALSE, FALSE, 0);
    GtkWidget *lang_row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8), *lang_note = label("", "note");
    GtkWidget *lang = gtk_combo_box_text_new();
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(lang), "中文"); gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(lang), "English");
    gtk_combo_box_set_active(GTK_COMBO_BOX(lang), uu_i18n_english());
    g_signal_connect(lang, "changed", G_CALLBACK(on_language), lang_note);
    gtk_box_pack_start(GTK_BOX(lang_row), label("语言 / Language", "key"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(lang_row), lang, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(lang_row), lang_note, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(root), lang_row, FALSE, FALSE, 6);
    if (close_hides) gtk_label_set_text(GTK_LABEL(footer_label), L("开源 Linux 被控端 · 非网易官方客户端 · 关闭窗口后远程协助仍在后台运行", "Open-source Linux host · not an official NetEase client · remote assistance keeps running after the window closes"));
    render_code();
    if (close_hides) build_tray();
    gtk_widget_show_all(gtk_bin_get_child(GTK_BIN(window)));
    gtk_widget_set_visible(assist_card, show_assist);
    gtk_widget_set_visible(hint_label, show_assist);
    gtk_widget_set_visible(account_card, show_account);
    pthread_mutex_lock(&problem_lock);
    gtk_label_set_text(GTK_LABEL(problem_label), problem_text ? problem_text : "");
    gtk_widget_set_visible(problem_label, problem_text != NULL);
    pthread_mutex_unlock(&problem_lock);
    if (!start_hidden) gtk_widget_show(window);
}

static void on_language(GtkComboBox *c, gpointer note) {
    int english = gtk_combo_box_get_active(c) == 1;
    gtk_label_set_text(GTK_LABEL(note), english == uu_i18n_english() ? "" : !uu_i18n_save(english) ? L("无法保存语言设置", "Could not save the language")
        : english ? "Restart uuremote-server to switch to English" : "重新启动 uuremote-server 后切换为中文");
}
static void *ui_main(void *arg) {
    (void)arg;
    int ok = gtk_init_check(NULL, NULL);
    // The CSS hard-codes a light palette; a dark system theme would turn unstyled text white on white (#43).
    if (ok) g_object_set(gtk_settings_get_default(), "gtk-theme-name", "Adwaita", "gtk-application-prefer-dark-theme", FALSE, NULL);
    if (ok) build_window();
    pthread_mutex_lock(&init_lock);
    init_state = ok ? 1 : -1;
    pthread_cond_signal(&init_cond);
    pthread_mutex_unlock(&init_lock);
    if (ok) gtk_main();
    else atomic_store(&closed, 1);
    return NULL;
}

// Returns 0 when the window was created, -1 when no display is usable. hidden: create without
// showing (host service, opened later with uu_assist_ui_present); hide_on_close: closing hides.
static gboolean apply_sections(gpointer d) {
    (void)d;
    if (window) {
        gtk_widget_set_visible(assist_card, show_assist); gtk_widget_set_visible(hint_label, show_assist);
        gtk_widget_set_visible(account_card, show_account);
    }
    return G_SOURCE_REMOVE;
}
static int ui_start(int hidden, int hide_on_close);
int uu_assist_ui_start(int hidden, int hide_on_close) {
    if (ui_started) { show_assist = 1; g_idle_add(apply_sections, NULL); return init_state == 1 ? 0 : -1; }
    show_assist = 1;
    return ui_start(hidden, hide_on_close);
}
// Reveals the account card on the shared window; standalone `login` opens it with the account card only.
int uu_login_ui_start(const char *name, const char *os, const char *cpu, const char *gpu) {
    (void)name; (void)os; (void)cpu; (void)gpu;
    show_account = 1;
    if (ui_started) { g_idle_add(apply_sections, NULL); return init_state == 1 ? 0 : -1; }
    show_assist = 0;
    return ui_start(0, 0);
}
void uu_ui_show_account(void) { show_account = 1; if (ui_started && init_state == 1) g_idle_add(apply_sections, NULL); }
static int ui_start(int hidden, int hide_on_close) {
    if (ui_started) return init_state == 1 ? 0 : -1;
    start_hidden = hidden; close_hides = hide_on_close;
    if (pthread_create(&ui_thread, NULL, ui_main, NULL) != 0) return -1;
    ui_started = 1;
    pthread_mutex_lock(&init_lock);
    while (init_state == 0) pthread_cond_wait(&init_cond, &init_lock);
    pthread_mutex_unlock(&init_lock);
    if (init_state != 1) { pthread_join(ui_thread, NULL); return -1; }
    return 0;
}

static gboolean apply_state(gpointer data) {
    State *s = data;
    if (window) {
        g_strlcpy(current_id, s->id, sizeof current_id);
        g_strlcpy(current_code, s->code, sizeof current_code);
        gtk_label_set_text(GTK_LABEL(id_label), current_id[0] ? current_id : L("获取中…", "Loading…"));
        render_code();
        suppress_switch = 1;
        gtk_switch_set_active(GTK_SWITCH(allow_switch), s->allowed);
        suppress_switch = 0;
        if (tray_allow) { suppress_tray = 1; gtk_check_menu_item_set_active(GTK_CHECK_MENU_ITEM(tray_allow), s->allowed); suppress_tray = 0; }
        cur_verify = s->verify; cur_refresh = s->refresh; cur_has_custom = s->has_custom;
        suppress_combo = 1; gtk_combo_box_set_active(GTK_COMBO_BOX(verify_combo), s->verify); suppress_combo = 0;
        if (s->refresh >= 0 && s->refresh < 5) {
            suppress_refresh = 1; gtk_check_menu_item_set_active(GTK_CHECK_MENU_ITEM(refresh_items[s->refresh]), TRUE); suppress_refresh = 0;
        }
        gtk_label_set_text(GTK_LABEL(note_label), s->verify == VERIFY_CUSTOM ? L("自定义验证码不会自动刷新", "Custom code is never refreshed automatically")
            : s->verify == VERIFY_BOTH ? L("也可使用自定义验证码连接", "The custom code also works") : refresh_note(s->refresh));
        gtk_widget_set_sensitive(share_button, current_id[0] && current_code[0]);
        gtk_label_set_text(GTK_LABEL(hint_label), s->allowed
            ? L("对方在 UU 远程「开始协助」中输入设备 ID 和验证码即可连接", "Your partner enters the device ID and code under \"Start assistance\" in UU Remote to connect")
            : L("已关闭：他人暂时无法远程协助本机，打开右上角开关后才能连接", "Off: nobody can assist this computer until you turn on the switch at the top right"));
    }
    g_free(s->id); g_free(s->code); g_free(s);
    return G_SOURCE_REMOVE;
}

// id is already formatted for display (e.g. "123 456 789").
void uu_assist_ui_set(const char *id, const char *code, int allowed, int verify, int refresh, int has_custom) {
    if (!ui_started || init_state != 1) return;
    State *s = g_new0(State, 1);
    s->id = g_strdup(id ? id : ""); s->code = g_strdup(code ? code : ""); s->allowed = allowed;
    s->verify = verify; s->refresh = refresh; s->has_custom = has_custom;
    g_idle_add(apply_state, s);
}

// Pops the next user command (see CMD_*), copying its argument into arg.
int uu_assist_ui_take_command(char *arg, size_t len) {
    int cmd = CMD_NONE;
    if (arg && len) arg[0] = 0;
    pthread_mutex_lock(&q_lock);
    if (q_len) {
        Cmd *c = &queue[q_head];
        cmd = c->cmd; if (arg && len) g_strlcpy(arg, c->arg, len);
        memset(c->arg, 0, sizeof c->arg); // drop the custom code copy promptly
        q_head = (q_head + 1) % 16; q_len--;
    }
    pthread_mutex_unlock(&q_lock);
    return cmd;
}
int uu_assist_ui_closed(void) { return atomic_load(&closed); }

static gboolean apply_present(gpointer data) {
    (void)data;
    if (window) { gtk_widget_show(window); gtk_window_present(GTK_WINDOW(window)); }
    return G_SOURCE_REMOVE;
}
void uu_assist_ui_present(void) { if (ui_started && init_state == 1 && !atomic_load(&closed)) g_idle_add(apply_present, NULL); }

static gboolean apply_close(gpointer data) { (void)data; if (window) gtk_widget_destroy(window); return G_SOURCE_REMOVE; }
void uu_assist_ui_close(void) { if (ui_started && init_state == 1 && !atomic_load(&closed)) g_idle_add(apply_close, NULL); }

// ---- Account card API (QR login / signed-in user); same names as the former login window ----
typedef struct { unsigned char *rgba; int edge; } QrUpdate;
typedef struct { char *text; int kind; } StatusUpdate;

static gboolean apply_qr(gpointer data) {
    QrUpdate *u = data;
    if (window) {
        GdkPixbuf *pb = gdk_pixbuf_new_from_data(u->rgba, GDK_COLORSPACE_RGB, TRUE, 8, u->edge, u->edge, u->edge * 4, NULL, NULL);
        GdkPixbuf *scaled = gdk_pixbuf_scale_simple(pb, 180, 180, GDK_INTERP_NEAREST);
        gtk_image_set_from_pixbuf(GTK_IMAGE(qr_image), scaled);
        g_object_unref(scaled); g_object_unref(pb);
        gtk_stack_set_visible_child_name(GTK_STACK(qr_stack), "qr");
        gtk_stack_set_visible_child_name(GTK_STACK(account_stack), "qr");
    }
    g_free(u->rgba); g_free(u);
    return G_SOURCE_REMOVE;
}
void uu_login_ui_set_qr(const unsigned char *rgba, int edge) {
    if (!ui_started || init_state != 1 || atomic_load(&closed) || edge <= 0) return;
    QrUpdate *u = g_new(QrUpdate, 1);
    u->rgba = g_memdup2(rgba, (gsize)edge * edge * 4); u->edge = edge;
    g_idle_add(apply_qr, u);
}
static gboolean apply_login_status(gpointer data) {
    StatusUpdate *u = data;
    if (window) {
        GtkStyleContext *ctx = gtk_widget_get_style_context(login_status);
        gtk_style_context_remove_class(ctx, "ok"); gtk_style_context_remove_class(ctx, "error");
        if (u->kind == 1) gtk_style_context_add_class(ctx, "ok");
        if (u->kind == 2) gtk_style_context_add_class(ctx, "error");
        gtk_label_set_text(GTK_LABEL(login_status), u->text);
    }
    g_free(u->text); g_free(u);
    return G_SOURCE_REMOVE;
}
void uu_login_ui_set_status(const char *text, int kind) {
    if (!ui_started || init_state != 1 || atomic_load(&closed)) return;
    StatusUpdate *u = g_new(StatusUpdate, 1);
    u->text = g_strdup(text ? text : ""); u->kind = kind;
    g_idle_add(apply_login_status, u);
}
static gboolean apply_user(gpointer data) {
    if (window) {
        gtk_label_set_text(GTK_LABEL(user_label), data);
        gtk_stack_set_visible_child_name(GTK_STACK(account_stack), "user");
        show_account = 1; apply_sections(NULL);
    }
    g_free(data);
    return G_SOURCE_REMOVE;
}
// Signed in: show the user page with this (already masked) account name.
void uu_ui_set_user(const char *name) {
    if (!ui_started || init_state != 1) return;
    g_idle_add(apply_user, g_strdup(name ? name : ""));
}
static gboolean apply_qr_refresh(gpointer data) {
    if (window && qr_refresh) { if (GPOINTER_TO_INT(data)) gtk_widget_show(qr_refresh); else gtk_widget_hide(qr_refresh); }
    return G_SOURCE_REMOVE;
}
void uu_login_ui_offer_refresh(int visible) {
    if (!ui_started || init_state != 1 || atomic_load(&closed)) return;
    atomic_store(&qr_refresh_requested, 0);
    g_idle_add(apply_qr_refresh, GINT_TO_POINTER(visible != 0));
}
int uu_login_ui_take_refresh(void) { return atomic_exchange(&qr_refresh_requested, 0); }

static gboolean apply_problem(gpointer data) {
    (void)data;
    if (window && problem_label) {
        pthread_mutex_lock(&problem_lock);
        gtk_label_set_text(GTK_LABEL(problem_label), problem_text ? problem_text : "");
        gtk_widget_set_visible(problem_label, problem_text != NULL);
        pthread_mutex_unlock(&problem_lock);
    }
    return G_SOURCE_REMOVE;
}
// Freedesktop notification through the session bus (no GTK main loop needed). Returns 0 when delivered.
static int notify_desktop(const char *summary, const char *body) {
    GDBusConnection *bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, NULL);
    if (!bus) return -1;
    GVariantBuilder actions, hints;
    g_variant_builder_init(&actions, G_VARIANT_TYPE("as"));
    g_variant_builder_init(&hints, G_VARIANT_TYPE("a{sv}"));
    g_variant_builder_add(&hints, "{sv}", "urgency", g_variant_new_byte(2)); // critical: stays until dismissed
    GVariant *reply = g_dbus_connection_call_sync(bus, "org.freedesktop.Notifications", "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications", "Notify",
        g_variant_new("(susssasa{sv}i)", L("UU 远程 Linux 主机", "UU Remote Linux host"), 0u, "uu-remote-linux", summary, body, &actions, &hints, -1),
        G_VARIANT_TYPE("(u)"), G_DBUS_CALL_FLAGS_NONE, 3000, NULL, NULL);
    g_object_unref(bus);
    if (!reply) return -1;
    g_variant_unref(reply);
    return 0;
}
// text NULL clears the window line. notify: also tell the person now, by a desktop notification or,
// without a notification daemon (e.g. bare Xfce), by opening the host window that shows the line.
// Returns 0 notified, 1 window opened, -1 neither (no notify requested, or no daemon and no window).
int uu_ui_set_problem(const char *summary, const char *text, int notify) {
    pthread_mutex_lock(&problem_lock);
    g_free(problem_text);
    problem_text = text ? g_strdup(text) : NULL;
    pthread_mutex_unlock(&problem_lock);
    int window_ready = ui_started && init_state == 1 && !atomic_load(&closed);
    if (window_ready) g_idle_add(apply_problem, NULL);
    if (!notify || !text) return -1;
    if (notify_desktop(summary ? summary : "", text) == 0) return 0;
    if (!window_ready) return -1;
    uu_assist_ui_present();
    return 1;
}
void uu_login_ui_set_done(const char *text) { uu_ui_set_user(text); }
int uu_login_ui_closed(void) { return atomic_load(&closed); }
void uu_login_ui_close(void) { uu_assist_ui_close(); }
