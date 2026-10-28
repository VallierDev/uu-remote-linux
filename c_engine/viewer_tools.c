// GTK3 tool windows for the native viewer: file transfer (`files-ui`) and port mapping (`portmap-ui`).
// One tool window per process; desktop files reuse the desktop peer. GTK runs on its own thread; Rust pushes state with g_idle_add and polls commands. Paths and
// names stay inside this window; nothing here logs.
#include <gtk/gtk.h>
#include <pthread.h>
#include <stdatomic.h>
#include <string.h>
#include <sys/stat.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include "i18n.h"

#define QSIZE 64
static char queue[QSIZE][4096];
static int q_head, q_len;
static pthread_mutex_t q_lock = PTHREAD_MUTEX_INITIALIZER;
static atomic_int closed;
static int init_state;
static pthread_mutex_t init_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t init_cond = PTHREAD_COND_INITIALIZER;
static GtkWidget *window, *toast;
static char remote_name[256];

static void push(const char *cmd) {
    pthread_mutex_lock(&q_lock);
    if (q_len < QSIZE) { snprintf(queue[(q_head + q_len) % QSIZE], sizeof queue[0], "%s", cmd); q_len++; }
    pthread_mutex_unlock(&q_lock);
}
int uu_tools_ui_take(char *out, size_t len) {
    pthread_mutex_lock(&q_lock);
    int got = q_len > 0;
    if (got) { snprintf(out, len, "%s", queue[q_head]); q_head = (q_head + 1) % QSIZE; q_len--; }
    pthread_mutex_unlock(&q_lock);
    return got;
}
int uu_tools_ui_closed(void) { return atomic_load(&closed); }
static void add_class(GtkWidget *w, const char *cls) { gtk_style_context_add_class(gtk_widget_get_style_context(w), cls); }
static GtkWidget *hbox(int s) { return gtk_box_new(GTK_ORIENTATION_HORIZONTAL, s); }
static GtkWidget *vbox(int s) { return gtk_box_new(GTK_ORIENTATION_VERTICAL, s); }
static GtkWidget *label(const char *text, const char *cls) { GtkWidget *l = gtk_label_new(text); if (cls) add_class(l, cls); gtk_label_set_xalign(GTK_LABEL(l), 0); return l; }
static GtkWidget *button(const char *text, const char *cls) { GtkWidget *b = gtk_button_new_with_label(text); if (cls) add_class(b, cls); return b; }
static gboolean hide_toast(gpointer d) { (void)d; gtk_widget_hide(toast); return G_SOURCE_REMOVE; }
static void show_toast(const char *text) { gtk_label_set_text(GTK_LABEL(toast), text); gtk_widget_show(toast); g_timeout_add(3500, hide_toast, NULL); }
static gboolean toast_idle(gpointer d) { show_toast(d); g_free(d); return G_SOURCE_REMOVE; }
void uu_tools_ui_toast(const char *text) { g_idle_add(toast_idle, g_strdup(text)); }
static gboolean on_delete(GtkWidget *w, GdkEvent *e, gpointer d) { (void)e; (void)d; gtk_widget_hide(w); atomic_store(&closed, 1); return TRUE; }

static const char *css =
    "window { background: #f5f6f8; }\n"
    ".title { font-size: 22px; font-weight: bold; color: #111827; }\n"
    ".tag { background: #e5e7eb; color: #4b5563; border-radius: 6px; padding: 2px 8px; }\n"
    ".tag.remote { background: #d1fae5; color: #047857; }\n"
    ".card { background: white; border: 1px solid #e5e7eb; border-radius: 10px; padding: 8px; }\n"
    ".section { font-size: 17px; font-weight: bold; }\n"
    ".toast { background: rgba(17,24,39,0.88); color: white; border-radius: 8px; padding: 10px 18px; }\n"
    ".primary { background-image: none; background-color: #10b981; color: white; border: none; }\n"
    ".empty { color: #6b7280; font-size: 15px; }\n"
    ".online { color: #10b981; }\n";

static GtkWidget *frame(const char *title, GtkWidget **content) {
    GtkCssProvider *p = gtk_css_provider_new(); gtk_css_provider_load_from_data(p, css, -1, NULL);
    gtk_style_context_add_provider_for_screen(gdk_screen_get_default(), GTK_STYLE_PROVIDER(p), GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), title);
    g_signal_connect(window, "delete-event", G_CALLBACK(on_delete), NULL);
    GtkWidget *overlay = gtk_overlay_new(); *content = vbox(14);
    gtk_container_set_border_width(GTK_CONTAINER(*content), 22);
    gtk_container_add(GTK_CONTAINER(overlay), *content);
    toast = label("", "toast"); gtk_widget_set_halign(toast, GTK_ALIGN_CENTER); gtk_widget_set_valign(toast, GTK_ALIGN_END);
    gtk_widget_set_margin_bottom(toast, 30); gtk_widget_set_no_show_all(toast, TRUE);
    gtk_overlay_add_overlay(GTK_OVERLAY(overlay), toast);
    gtk_container_add(GTK_CONTAINER(window), overlay);
    return overlay;
}
static GtkWidget *add_column(GtkWidget *view, const char *title, int col, int expand) {
    GtkCellRenderer *r = gtk_cell_renderer_text_new();
    if (expand) g_object_set(r, "ellipsize", PANGO_ELLIPSIZE_MIDDLE, NULL);
    GtkTreeViewColumn *c = gtk_tree_view_column_new_with_attributes(title, r, "text", col, NULL);
    gtk_tree_view_column_set_resizable(c, TRUE); gtk_tree_view_column_set_expand(c, expand);
    if (expand) gtk_tree_view_column_set_min_width(c, 160);
    gtk_tree_view_append_column(GTK_TREE_VIEW(view), c);
    return view;
}
static GtkWidget *scrolled(GtkWidget *child, int height) {
    GtkWidget *s = gtk_scrolled_window_new(NULL, NULL);
    gtk_scrolled_window_set_policy(GTK_SCROLLED_WINDOW(s), GTK_POLICY_AUTOMATIC, GTK_POLICY_AUTOMATIC);
    gtk_widget_set_size_request(s, -1, height); gtk_container_add(GTK_CONTAINER(s), child);
    return s;
}
static void format_size(guint64 size, int kind, char *out, size_t len) {
    if (kind != 4) { snprintf(out, len, "--"); return; }
    if (size < 1024) snprintf(out, len, "%llu B", (unsigned long long)size);
    else if (size < 1024 * 1024) snprintf(out, len, "%.2f KB", size / 1024.0);
    else if (size < 1024ull * 1024 * 1024) snprintf(out, len, "%.2f MB", size / 1048576.0);
    else snprintf(out, len, "%.2f GB", size / 1073741824.0);
}
static void format_time(guint64 secs, char *out, size_t len) {
    if (!secs) { snprintf(out, len, "--"); return; }
    time_t t = (time_t)secs; struct tm tm; localtime_r(&t, &tm);
    strftime(out, len, L("%Y年%m月%d日", "%Y-%m-%d"), &tm);
}
static const char *kind_name(int kind) { return kind == 4 ? L("文件", "File") : kind == 3 ? L("磁盘", "Disk") : L("文件夹", "Folder"); }

/* ---------------- file transfer ---------------- */
enum { C_NAME, C_TIME, C_TYPE, C_SIZE, C_PATH, C_KIND, C_N };
enum { T_ID, T_NAME, T_STATUS, T_SIZE, T_FROM, T_TO, T_DONE, T_N };
static GtkListStore *local_store, *remote_store, *task_store;
static GtkWidget *local_view, *remote_view, *local_path, *remote_path;
static char local_dir[2048] = "", remote_dir[2048] = ":/";

static void load_local(const char *path) {
    char *dir = g_strdup(path); /* path may alias local_dir */
    GDir *d = g_dir_open(dir, 0, NULL);
    if (!d) { show_toast(L("无法打开本机目录", "Cannot open local folder")); g_free(dir); return; }
    snprintf(local_dir, sizeof local_dir, "%s", dir); g_free(dir);
    gtk_entry_set_text(GTK_ENTRY(local_path), local_dir);
    gtk_list_store_clear(local_store);
    const char *name;
    GPtrArray *names = g_ptr_array_new_with_free_func(g_free);
    while ((name = g_dir_read_name(d))) if (name[0] != '.') g_ptr_array_add(names, g_strdup(name));
    g_dir_close(d);
    for (int pass = 0; pass < 2; pass++) for (guint i = 0; i < names->len; i++) {
        char *full = g_build_filename(local_dir, names->pdata[i], NULL); struct stat st;
        if (stat(full, &st) == 0 && (S_ISDIR(st.st_mode) ? pass == 0 : pass == 1)) {
            int kind = S_ISDIR(st.st_mode) ? 0 : 4; char size[32], when[64];
            format_size(st.st_size, kind, size, sizeof size); format_time(st.st_mtime, when, sizeof when);
            GtkTreeIter it; gtk_list_store_append(local_store, &it);
            gtk_list_store_set(local_store, &it, C_NAME, names->pdata[i], C_TIME, when, C_TYPE, kind_name(kind), C_SIZE, size, C_PATH, full, C_KIND, kind, -1);
        }
        g_free(full);
    }
    g_ptr_array_free(names, TRUE);
}
static void request_remote(const char *path) { char cmd[2100]; snprintf(cmd, sizeof cmd, "ls\t%s", path); push(cmd); }
static void on_local_up(GtkButton *b, gpointer d) { (void)b; (void)d; char *p = g_path_get_dirname(local_dir); load_local(p); g_free(p); }
static void on_local_refresh(GtkButton *b, gpointer d) { (void)b; (void)d; load_local(local_dir); }
static void on_local_path(GtkEntry *e, gpointer d) { (void)d; load_local(gtk_entry_get_text(e)); }
static void on_remote_up(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    if (!strcmp(remote_dir, ":/") || !strcmp(remote_dir, "/")) { request_remote(":/"); return; }
    char *p = g_path_get_dirname(remote_dir); request_remote(p); g_free(p);
}
static void on_remote_refresh(GtkButton *b, gpointer d) { (void)b; (void)d; request_remote(remote_dir); }
static void on_remote_path(GtkEntry *e, gpointer d) { (void)d; request_remote(gtk_entry_get_text(e)); }
static void on_row_activated(GtkTreeView *view, GtkTreePath *path, GtkTreeViewColumn *col, gpointer remote) {
    (void)col; GtkTreeModel *m = gtk_tree_view_get_model(view); GtkTreeIter it; char *full; int kind;
    if (!gtk_tree_model_get_iter(m, &it, path)) return;
    gtk_tree_model_get(m, &it, C_PATH, &full, C_KIND, &kind, -1);
    if (kind != 4) { if (remote) request_remote(full); else load_local(full); }
    g_free(full);
}
static char *ask_name(const char *title) {
    GtkWidget *dlg = gtk_dialog_new_with_buttons(title, GTK_WINDOW(window), GTK_DIALOG_MODAL, L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("确定", "OK"), GTK_RESPONSE_OK, NULL);
    GtkWidget *entry = gtk_entry_new(); gtk_entry_set_activates_default(GTK_ENTRY(entry), TRUE);
    gtk_dialog_set_default_response(GTK_DIALOG(dlg), GTK_RESPONSE_OK);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(dlg)); gtk_container_set_border_width(GTK_CONTAINER(area), 16);
    gtk_box_pack_start(GTK_BOX(area), entry, FALSE, FALSE, 0); gtk_widget_show_all(dlg);
    char *name = NULL;
    if (gtk_dialog_run(GTK_DIALOG(dlg)) == GTK_RESPONSE_OK) {
        const char *t = gtk_entry_get_text(GTK_ENTRY(entry));
        if (*t && !strchr(t, '/') && strcmp(t, ".") && strcmp(t, "..")) name = g_strdup(t); else if (*t) show_toast(L("名称无效", "Invalid name"));
    }
    gtk_widget_destroy(dlg);
    return name;
}
static void on_local_mkdir(GtkButton *b, gpointer d) {
    (void)b; (void)d; char *name = ask_name(L("新建文件夹", "New folder")); if (!name) return;
    char *full = g_build_filename(local_dir, name, NULL);
    if (mkdir(full, 0755) != 0) show_toast(L("无法创建文件夹", "Cannot create folder")); else load_local(local_dir);
    g_free(full); g_free(name);
}
static void on_remote_mkdir(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    if (!strcmp(remote_dir, ":/")) { show_toast(L("请先进入一个文件夹", "Open a folder first")); return; }
    char *name = ask_name(L("新建文件夹", "New folder")); if (!name) return;
    char *full = g_build_filename(remote_dir, name, NULL); char cmd[2200];
    snprintf(cmd, sizeof cmd, "mkdir\t%s", full); push(cmd); g_free(full); g_free(name);
}
/* Each selected row becomes one queued transfer. Remote folders download recursively; local folders
 * are not supported by the upload RPC sequence implemented here. */
static void on_send(GtkButton *b, gpointer upload) {
    (void)b; GtkTreeView *view = GTK_TREE_VIEW(upload ? local_view : remote_view);
    GtkTreeModel *m; GList *rows = gtk_tree_selection_get_selected_rows(gtk_tree_view_get_selection(view), &m);
    if (!rows) { show_toast(L("请先选择要发送的文件", "Select the files to send first")); return; }
    if (upload && !strcmp(remote_dir, ":/")) { show_toast(L("请先在远端进入目标文件夹", "Open the destination folder on the remote side first")); g_list_free_full(rows, (GDestroyNotify)gtk_tree_path_free); return; }
    int skipped = 0;
    for (GList *r = rows; r; r = r->next) {
        GtkTreeIter it; char *full, *name; int kind;
        if (!gtk_tree_model_get_iter(m, &it, r->data)) continue;
        gtk_tree_model_get(m, &it, C_PATH, &full, C_NAME, &name, C_KIND, &kind, -1);
        char cmd[4096];
        if (upload && kind != 4) skipped++;
        else if (!upload && kind == 3) skipped++;
        else {
            if (upload) snprintf(cmd, sizeof cmd, "put\t%s\t%s", full, remote_dir);
            else snprintf(cmd, sizeof cmd, "get\t%s\t%s", full, local_dir);
            push(cmd);
        }
        g_free(full); g_free(name);
    }
    g_list_free_full(rows, (GDestroyNotify)gtk_tree_path_free);
    if (skipped) show_toast(upload ? L("暂不支持上传文件夹，已跳过", "Uploading folders is not supported yet; skipped") : L("不能下载整个磁盘，已跳过", "Cannot download a whole disk; skipped"));
}
static void on_cancel_all(GtkButton *b, gpointer d) { (void)b; (void)d; push("cancel"); }
static void on_clear_done(GtkButton *b, gpointer d) {
    (void)b; (void)d; GtkTreeIter it; gboolean ok = gtk_tree_model_get_iter_first(GTK_TREE_MODEL(task_store), &it);
    while (ok) { int done; gtk_tree_model_get(GTK_TREE_MODEL(task_store), &it, T_DONE, &done, -1);
        ok = done ? gtk_list_store_remove(task_store, &it) : gtk_tree_model_iter_next(GTK_TREE_MODEL(task_store), &it); }
}
static GtkWidget *pane(int remote) {
    GtkWidget *card = vbox(6); add_class(card, "card");
    GtkWidget *bar = hbox(6), *up = button("↑", NULL), *refresh = button(L("刷新", "Refresh"), NULL), *mk = button(L("新建文件夹", "New folder"), NULL), *path = gtk_entry_new();
    gtk_widget_set_tooltip_text(up, L("上一级", "Up")); gtk_widget_set_tooltip_text(refresh, L("刷新", "Refresh"));
    gtk_box_pack_start(GTK_BOX(bar), up, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(bar), refresh, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(bar), mk, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(bar), path, TRUE, TRUE, 0);
    GtkListStore *store = gtk_list_store_new(C_N, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_INT);
    GtkWidget *view = gtk_tree_view_new_with_model(GTK_TREE_MODEL(store));
    add_column(view, L("名称", "Name"), C_NAME, 1); add_column(view, L("修改日期", "Date modified"), C_TIME, 0); add_column(view, L("类型", "Type"), C_TYPE, 0); add_column(view, L("大小", "Size"), C_SIZE, 0);
    gtk_tree_selection_set_mode(gtk_tree_view_get_selection(GTK_TREE_VIEW(view)), GTK_SELECTION_MULTIPLE);
    g_signal_connect(view, "row-activated", G_CALLBACK(on_row_activated), GINT_TO_POINTER(remote));
    g_signal_connect(up, "clicked", G_CALLBACK(remote ? on_remote_up : on_local_up), NULL);
    g_signal_connect(refresh, "clicked", G_CALLBACK(remote ? on_remote_refresh : on_local_refresh), NULL);
    g_signal_connect(mk, "clicked", G_CALLBACK(remote ? on_remote_mkdir : on_local_mkdir), NULL);
    g_signal_connect(path, "activate", G_CALLBACK(remote ? on_remote_path : on_local_path), NULL);
    gtk_box_pack_start(GTK_BOX(card), bar, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(card), scrolled(view, 300), TRUE, TRUE, 0);
    if (remote) { remote_store = store; remote_view = view; remote_path = path; }
    else { local_store = store; local_view = view; local_path = path; }
    return card;
}
static void build_files(void) {
    GtkWidget *content; frame(L("文件传输", "File transfer"), &content);
    gtk_window_set_default_size(GTK_WINDOW(window), 1100, 760);
    GtkWidget *head = gtk_grid_new(); gtk_grid_set_column_homogeneous(GTK_GRID(head), TRUE); gtk_grid_set_column_spacing(GTK_GRID(head), 24);
    GtkWidget *left = hbox(10), *right = hbox(10);
    char host[256]; if (gethostname(host, sizeof host) != 0) snprintf(host, sizeof host, L("本机", "This device")); host[sizeof host - 1] = 0;
    GtkWidget *tag_l = label(L("本机", "This device"), "tag"), *tag_r = label(L("远端", "Remote"), "tag"); add_class(tag_r, "remote");
    GtkWidget *send_r = button(L("发送 →", "Send →"), NULL), *send_l = button(L("← 发送", "← Send"), NULL);
    g_signal_connect(send_r, "clicked", G_CALLBACK(on_send), GINT_TO_POINTER(1));
    g_signal_connect(send_l, "clicked", G_CALLBACK(on_send), NULL);
    gtk_box_pack_start(GTK_BOX(left), label(host, "title"), FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(left), tag_l, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(left), send_r, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(right), send_l, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(right), label(remote_name, "title"), FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(right), tag_r, FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(head), left, 0, 0, 1, 1); gtk_grid_attach(GTK_GRID(head), right, 1, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(head), pane(0), 0, 1, 1, 1); gtk_grid_attach(GTK_GRID(head), pane(1), 1, 1, 1, 1);
    gtk_grid_set_row_spacing(GTK_GRID(head), 14);
    gtk_box_pack_start(GTK_BOX(content), head, TRUE, TRUE, 0);
    GtkWidget *tasks = vbox(8); add_class(tasks, "card");
    GtkWidget *tbar = hbox(8), *cancel = button(L("全部取消", "Cancel all"), NULL), *clear = button(L("清除完结任务", "Clear finished"), NULL);
    g_signal_connect(cancel, "clicked", G_CALLBACK(on_cancel_all), NULL); g_signal_connect(clear, "clicked", G_CALLBACK(on_clear_done), NULL);
    gtk_box_pack_start(GTK_BOX(tbar), label(L("传输列表", "Transfers"), "section"), FALSE, FALSE, 4);
    gtk_box_pack_end(GTK_BOX(tbar), clear, FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(tbar), cancel, FALSE, FALSE, 0);
    task_store = gtk_list_store_new(T_N, G_TYPE_INT, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_INT);
    GtkWidget *tview = gtk_tree_view_new_with_model(GTK_TREE_MODEL(task_store));
    add_column(tview, L("名称", "Name"), T_NAME, 1); add_column(tview, L("状态", "Status"), T_STATUS, 0); add_column(tview, L("大小", "Size"), T_SIZE, 0);
    add_column(tview, L("发送路径", "From"), T_FROM, 1); add_column(tview, L("接收路径", "To"), T_TO, 1);
    gtk_box_pack_start(GTK_BOX(tasks), tbar, FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(tasks), scrolled(tview, 180), TRUE, TRUE, 0);
    gtk_box_pack_start(GTK_BOX(content), tasks, TRUE, TRUE, 0);
    const char *home = g_get_home_dir(); load_local(home ? home : "/");
    gtk_entry_set_text(GTK_ENTRY(remote_path), L("此电脑", "This PC"));
}

struct listing { char *path; char *rows; };
static gboolean remote_idle(gpointer d) {
    struct listing *l = d;
    snprintf(remote_dir, sizeof remote_dir, "%s", l->path);
    gtk_entry_set_text(GTK_ENTRY(remote_path), strcmp(remote_dir, ":/") ? remote_dir : L("此电脑", "This PC"));
    gtk_list_store_clear(remote_store);
    char **lines = g_strsplit(l->rows, "\n", -1);
    for (char **line = lines; *line; line++) {
        char **f = g_strsplit(*line, "\t", 5);
        if (g_strv_length(f) == 5) {
            int kind = atoi(f[0]); char size[32], when[64];
            format_size(g_ascii_strtoull(f[2], NULL, 10), kind, size, sizeof size); format_time(g_ascii_strtoull(f[3], NULL, 10), when, sizeof when);
            GtkTreeIter it; gtk_list_store_append(remote_store, &it);
            gtk_list_store_set(remote_store, &it, C_NAME, f[1], C_TIME, when, C_TYPE, kind_name(kind), C_SIZE, size, C_PATH, f[4], C_KIND, kind, -1);
        }
        g_strfreev(f);
    }
    g_strfreev(lines); g_free(l->path); g_free(l->rows); g_free(l);
    return G_SOURCE_REMOVE;
}
/* rows: "kind\tname\tsize\tmodified\tpath\n" */
void uu_files_ui_set_remote(const char *path, const char *rows) {
    struct listing *l = g_new0(struct listing, 1); l->path = g_strdup(path); l->rows = g_strdup(rows); g_idle_add(remote_idle, l);
}
struct task { int id, done; char *name, *status, *size, *from, *to; };
static gboolean task_idle(gpointer d) {
    struct task *t = d; GtkTreeIter it; gboolean ok = gtk_tree_model_get_iter_first(GTK_TREE_MODEL(task_store), &it), found = FALSE;
    while (ok) { int id; gtk_tree_model_get(GTK_TREE_MODEL(task_store), &it, T_ID, &id, -1); if (id == t->id) { found = TRUE; break; } ok = gtk_tree_model_iter_next(GTK_TREE_MODEL(task_store), &it); }
    if (!found) { gtk_list_store_append(task_store, &it); gtk_list_store_set(task_store, &it, T_ID, t->id, T_NAME, t->name, -1); }
    gtk_list_store_set(task_store, &it, T_STATUS, t->status, T_SIZE, t->size, T_FROM, t->from, T_TO, t->to, T_DONE, t->done, -1);
    g_free(t->name); g_free(t->status); g_free(t->size); g_free(t->from); g_free(t->to); g_free(t);
    return G_SOURCE_REMOVE;
}
/* total == 0 means unknown size; done: 0 running/queued, 1 finished (success), 2 finished (failed/canceled). */
void uu_files_ui_task(int id, const char *name, const char *status, unsigned long long transferred, unsigned long long total, const char *from, const char *to, int done) {
    struct task *t = g_new0(struct task, 1); char a[32], b[32];
    t->id = id; t->done = done;
    format_size(transferred, 4, a, sizeof a); format_size(total, 4, b, sizeof b);
    t->name = g_strdup(name); t->status = g_strdup(status); t->from = g_strdup(from); t->to = g_strdup(to);
    t->size = total ? g_strdup_printf("%s / %s", a, b) : g_strdup(transferred ? a : "--");
    g_idle_add(task_idle, t);
}
static gboolean refresh_local_idle(gpointer d) { (void)d; load_local(local_dir); return G_SOURCE_REMOVE; }
void uu_files_ui_refresh_local(void) { g_idle_add(refresh_local_idle, NULL); }

/* ---------------- port mapping ---------------- */
enum { P_ID, P_NAME, P_LOCAL, P_TARGET, P_STATUS, P_N };
static GtkListStore *rule_store;
static GtkWidget *rule_stack;
static void on_add_rule(GtkButton *b, gpointer d) {
    (void)b; (void)d;
    GtkWidget *dlg = gtk_dialog_new_with_buttons(L("添加映射规则", "Add mapping rule"), GTK_WINDOW(window), GTK_DIALOG_MODAL, L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("保存后立即启用", "Save and enable"), GTK_RESPONSE_OK, NULL);
    GtkWidget *grid = gtk_grid_new(); gtk_grid_set_row_spacing(GTK_GRID(grid), 12); gtk_grid_set_column_spacing(GTK_GRID(grid), 24);
    gtk_container_set_border_width(GTK_CONTAINER(grid), 20);
    const char *labels[] = {L("名称", "Name"), L("本地端口", "Local port"), L("目标地址", "Target address"), L("目标端口", "Target port")}, *hints[] = {L("名称", "Name"), L("请输入端口", "Enter port"), "", L("请输入端口", "Enter port")};
    GtkWidget *entries[4];
    for (int i = 0; i < 4; i++) {
        entries[i] = gtk_entry_new(); gtk_entry_set_placeholder_text(GTK_ENTRY(entries[i]), hints[i]); gtk_widget_set_hexpand(entries[i], TRUE);
        gtk_grid_attach(GTK_GRID(grid), label(labels[i], NULL), 0, i, 1, 1); gtk_grid_attach(GTK_GRID(grid), entries[i], 1, i, 1, 1);
    }
    gtk_entry_set_text(GTK_ENTRY(entries[2]), "127.0.0.1");
    gtk_box_pack_start(GTK_BOX(gtk_dialog_get_content_area(GTK_DIALOG(dlg))), grid, TRUE, TRUE, 0);
    gtk_window_set_default_size(GTK_WINDOW(dlg), 480, -1); gtk_widget_show_all(dlg);
    while (gtk_dialog_run(GTK_DIALOG(dlg)) == GTK_RESPONSE_OK) {
        const char *name = gtk_entry_get_text(GTK_ENTRY(entries[0])), *host = gtk_entry_get_text(GTK_ENTRY(entries[2]));
        char *end; long local = strtol(gtk_entry_get_text(GTK_ENTRY(entries[1])), &end, 10); int bad_local = *end || local < 0 || local > 65535;
        long port = strtol(gtk_entry_get_text(GTK_ENTRY(entries[3])), &end, 10); int bad_port = *end || port < 1 || port > 65535;
        if (!*name || strchr(name, '\t') || !*host || strchr(host, '\t') || bad_local || bad_port) { show_toast(L("请填写名称、有效端口和目标地址", "Enter a name, valid ports and a target address")); continue; }
        char cmd[1024]; snprintf(cmd, sizeof cmd, "add\t%s\t%ld\t%s\t%ld", name, local, host, port); push(cmd);
        break;
    }
    gtk_widget_destroy(dlg);
}
static void on_remove_rule(GtkButton *b, gpointer view) {
    (void)b; GtkTreeModel *m; GtkTreeIter it;
    if (!gtk_tree_selection_get_selected(gtk_tree_view_get_selection(view), &m, &it)) { show_toast(L("请先选择一条规则", "Select a rule first")); return; }
    int id; gtk_tree_model_get(m, &it, P_ID, &id, -1); char cmd[64]; snprintf(cmd, sizeof cmd, "remove\t%d", id); push(cmd);
}
static void build_portmap(void) {
    GtkWidget *content; frame(L("端口映射", "Port mapping"), &content);
    gtk_window_set_default_size(GTK_WINDOW(window), 820, 560);
    GtkWidget *head = hbox(12), *add = button(L("添加规则", "Add rule"), "primary"), *del = button(L("删除", "Remove"), NULL);
    GtkWidget *dot = label(L("● 已连接", "● Connected"), "online");
    gtk_box_pack_start(GTK_BOX(head), label(remote_name, "title"), FALSE, FALSE, 0); gtk_box_pack_start(GTK_BOX(head), dot, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(head), add, FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(head), del, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(content), head, FALSE, FALSE, 0);
    rule_store = gtk_list_store_new(P_N, G_TYPE_INT, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING);
    GtkWidget *view = gtk_tree_view_new_with_model(GTK_TREE_MODEL(rule_store));
    add_column(view, L("名称", "Name"), P_NAME, 1); add_column(view, L("本地地址", "Local address"), P_LOCAL, 0); add_column(view, L("目标地址", "Target address"), P_TARGET, 1); add_column(view, L("状态", "Status"), P_STATUS, 0);
    rule_stack = gtk_stack_new();
    GtkWidget *empty = label(L("当前没有端口映射规则，点击「添加规则」把远端端口映射到本机", "No port mapping rules. Click \"Add rule\" to map a remote port to this computer"), "empty"); gtk_widget_set_halign(empty, GTK_ALIGN_CENTER);
    gtk_stack_add_named(GTK_STACK(rule_stack), empty, "empty");
    gtk_stack_add_named(GTK_STACK(rule_stack), scrolled(view, 360), "list");
    GtkWidget *card = vbox(0); add_class(card, "card"); gtk_box_pack_start(GTK_BOX(card), rule_stack, TRUE, TRUE, 0);
    gtk_box_pack_start(GTK_BOX(content), card, TRUE, TRUE, 0);
    gtk_box_pack_start(GTK_BOX(content), label(L("本地端口只监听 127.0.0.1；本地端口填 0 时自动分配。", "Local ports listen on 127.0.0.1 only; enter 0 to pick a free port."), "empty"), FALSE, FALSE, 0);
    g_signal_connect(add, "clicked", G_CALLBACK(on_add_rule), NULL);
    g_signal_connect(del, "clicked", G_CALLBACK(on_remove_rule), view);
}
static gboolean rules_idle(gpointer d) {
    char *rows = d; gtk_list_store_clear(rule_store); int n = 0;
    char **lines = g_strsplit(rows, "\n", -1);
    for (char **line = lines; *line; line++) {
        char **f = g_strsplit(*line, "\t", 5);
        if (g_strv_length(f) == 5) { GtkTreeIter it; gtk_list_store_append(rule_store, &it);
            gtk_list_store_set(rule_store, &it, P_ID, atoi(f[0]), P_NAME, f[1], P_LOCAL, f[2], P_TARGET, f[3], P_STATUS, f[4], -1); n++; }
        g_strfreev(f);
    }
    g_strfreev(lines); g_free(rows);
    gtk_stack_set_visible_child_name(GTK_STACK(rule_stack), n ? "list" : "empty");
    return G_SOURCE_REMOVE;
}
/* rows: "id\tname\tlocal\ttarget\tstatus\n" */
void uu_portmap_ui_set_rules(const char *rows) { g_idle_add(rules_idle, g_strdup(rows)); }

/* ---------------- terminal sessions ---------------- */
enum { S_ID, S_SHELL, S_TIME, S_STATE, S_N };
static GtkListStore *session_store;
static GtkWidget *session_view, *session_stack, *session_dot, *session_create, *session_kill;
static guint selected_session(void) {
    GtkTreeModel *m; GtkTreeIter it; guint id = 0;
    if (gtk_tree_selection_get_selected(gtk_tree_view_get_selection(GTK_TREE_VIEW(session_view)), &m, &it)) gtk_tree_model_get(m, &it, S_ID, &id, -1);
    return id;
}
static void on_create_session(GtkButton *b, gpointer d) { (void)b; (void)d; push("create"); }
static void on_open_session(GtkTreeView *v, GtkTreePath *path, GtkTreeViewColumn *col, gpointer d) {
    (void)col; (void)d; GtkTreeIter it; guint id;
    if (!gtk_widget_get_sensitive(GTK_WIDGET(v)) || !gtk_tree_model_get_iter(GTK_TREE_MODEL(session_store), &it, path)) return;
    gtk_tree_model_get(GTK_TREE_MODEL(session_store), &it, S_ID, &id, -1);
    char cmd[64]; snprintf(cmd, sizeof cmd, "open\t%u", id); push(cmd);
}
static void on_kill_session(GtkButton *b, gpointer d) {
    (void)b; (void)d; guint id = selected_session();
    if (!id) { show_toast(L("请先选择一个会话", "Select a session first")); return; }
    GtkWidget *dlg = gtk_message_dialog_new(GTK_WINDOW(window), GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT,
        GTK_MESSAGE_WARNING, GTK_BUTTONS_NONE, L("确定结束会话 %u？", "End session %u?"), id);
    gtk_message_dialog_format_secondary_text(GTK_MESSAGE_DIALOG(dlg), L("会话内正在运行的程序将被终止。", "Programs running in the session will be terminated."));
    gtk_dialog_add_buttons(GTK_DIALOG(dlg), L("取消", "Cancel"), GTK_RESPONSE_CANCEL, L("结束会话", "End session"), GTK_RESPONSE_OK, NULL);
    gtk_dialog_set_default_response(GTK_DIALOG(dlg), GTK_RESPONSE_CANCEL);
    if (gtk_dialog_run(GTK_DIALOG(dlg)) == GTK_RESPONSE_OK) { char cmd[64]; snprintf(cmd, sizeof cmd, "kill\t%u", id); push(cmd); }
    gtk_widget_destroy(dlg);
}
static void build_terminal(void) {
    GtkWidget *content; frame(L("终端远控", "Remote terminal"), &content); gtk_window_set_default_size(GTK_WINDOW(window), 820, 560);
    GtkWidget *head = hbox(12); session_dot = label(L("● 正在读取会话", "● Loading sessions"), "online");
    session_create = button(L("创建会话", "New session"), "primary"); session_kill = button(L("结束会话", "End session"), NULL);
    gtk_box_pack_start(GTK_BOX(head), label(remote_name, "title"), FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(head), session_dot, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(head), session_create, FALSE, FALSE, 0); gtk_box_pack_end(GTK_BOX(head), session_kill, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(content), head, FALSE, FALSE, 0);
    session_store = gtk_list_store_new(S_N, G_TYPE_UINT, G_TYPE_STRING, G_TYPE_STRING, G_TYPE_STRING);
    session_view = gtk_tree_view_new_with_model(GTK_TREE_MODEL(session_store));
    add_column(session_view, L("会话 ID", "Session ID"), S_ID, 0); add_column(session_view, "Shell", S_SHELL, 1);
    add_column(session_view, L("创建时间", "Created"), S_TIME, 0); add_column(session_view, L("状态", "Status"), S_STATE, 0);
    session_stack = gtk_stack_new();
    GtkWidget *empty = label(L("当前无正在运行的终端会话", "No running terminal sessions"), "empty"); gtk_widget_set_halign(empty, GTK_ALIGN_CENTER);
    gtk_stack_add_named(GTK_STACK(session_stack), empty, "empty");
    gtk_stack_add_named(GTK_STACK(session_stack), label(L("正在读取会话…", "Loading sessions…"), "empty"), "loading");
    gtk_stack_add_named(GTK_STACK(session_stack), label(L("连接已断开，请关闭窗口后重试", "Disconnected. Close the window and try again"), "empty"), "offline");
    gtk_stack_add_named(GTK_STACK(session_stack), scrolled(session_view, 360), "list");
    gtk_stack_set_visible_child_name(GTK_STACK(session_stack), "loading");
    gtk_box_pack_start(GTK_BOX(content), session_stack, TRUE, TRUE, 0);
    gtk_box_pack_start(GTK_BOX(content), label(L("双击接回会话；创建会话将在本机终端程序中打开。", "Double-click to reattach; new sessions open in a local terminal."), "empty"), FALSE, FALSE, 0);
    gtk_widget_set_sensitive(session_create, FALSE); gtk_widget_set_sensitive(session_kill, FALSE);
    g_signal_connect(session_create, "clicked", G_CALLBACK(on_create_session), NULL);
    g_signal_connect(session_kill, "clicked", G_CALLBACK(on_kill_session), NULL);
    g_signal_connect(session_view, "row-activated", G_CALLBACK(on_open_session), NULL);
}
struct sessions { char *rows; int connected; };
static gboolean sessions_idle(gpointer d) {
    struct sessions *s = d;
    if (atomic_load(&closed)) { g_free(s->rows); g_free(s); return G_SOURCE_REMOVE; }
    guint selected = selected_session(); gtk_list_store_clear(session_store); int n = 0;
    char **lines = g_strsplit(s->rows, "\n", -1);
    for (char **line = lines; *line; line++) {
        char **f = g_strsplit(*line, "\t", 4);
        if (g_strv_length(f) == 4) {
            guint id = (guint)g_ascii_strtoull(f[0], NULL, 10); GtkTreeIter it; char date[64] = "--";
            gint64 ms = g_ascii_strtoll(f[2], NULL, 10);
            GDateTime *time = ms > 0 ? g_date_time_new_from_unix_local(ms / 1000) : NULL;
            if (time) { char *text = g_date_time_format(time, "%Y-%m-%d %H:%M:%S"); g_strlcpy(date, text, sizeof date); g_free(text); g_date_time_unref(time); }
            gtk_list_store_append(session_store, &it);
            gtk_list_store_set(session_store, &it, S_ID, id, S_SHELL, f[1], S_TIME, date, S_STATE, f[3], -1); n++;
            if (id == selected) gtk_tree_selection_select_iter(gtk_tree_view_get_selection(GTK_TREE_VIEW(session_view)), &it);
        }
        g_strfreev(f);
    }
    g_strfreev(lines);
    gtk_label_set_text(GTK_LABEL(session_dot), s->connected ? L("● 已连接", "● Connected") : L("● 已断开", "● Disconnected"));
    gtk_widget_set_sensitive(session_create, s->connected); gtk_widget_set_sensitive(session_kill, s->connected);
    gtk_widget_set_sensitive(session_view, s->connected);
    gtk_stack_set_visible_child_name(GTK_STACK(session_stack), s->connected ? (n ? "list" : "empty") : "offline");
    g_free(s->rows); g_free(s); return G_SOURCE_REMOVE;
}
/* rows: "id\tshell\tcreated_at_ms\tstate\n"; copied before returning to Rust. */
void uu_terminal_ui_set(const char *rows, int connected) {
    struct sessions *s = g_new0(struct sessions, 1); s->rows = g_strdup(rows); s->connected = connected; g_idle_add(sessions_idle, s);
}

/* ---------------- startup ---------------- */
static char tool_kind[16];
static gboolean show_window(gpointer d) {
    (void)d;
    if (!window) {
        if (!strcmp(tool_kind, "files")) build_files(); else if (!strcmp(tool_kind, "terminal")) build_terminal(); else build_portmap();
    }
    atomic_store(&closed, 0); gtk_widget_show_all(window); gtk_window_present(GTK_WINDOW(window));
    pthread_mutex_lock(&init_lock); init_state = 1; pthread_cond_signal(&init_cond); pthread_mutex_unlock(&init_lock);
    return G_SOURCE_REMOVE;
}
static void *ui_thread(void *d) {
    (void)d;
    if (!gtk_init_check(NULL, NULL)) { pthread_mutex_lock(&init_lock); init_state = -1; pthread_cond_signal(&init_cond); pthread_mutex_unlock(&init_lock); return NULL; }
    gtk_window_set_default_icon_name("uu-remote-linux"); // same taskbar icon as the host (#13)
    // Light palette in CSS; keep a dark system theme from making unstyled text white on white (#43).
    g_object_set(gtk_settings_get_default(), "gtk-theme-name", "Adwaita", "gtk-application-prefer-dark-theme", FALSE, NULL);
    show_window(NULL); gtk_main(); return NULL;
}
/* Hidden windows retain their widgets so queued idle callbacks stay valid. */
int uu_tools_ui_start(const char *kind, const char *name) {
    static int started;
    pthread_mutex_lock(&init_lock); if (init_state < 0) { pthread_mutex_unlock(&init_lock); return -1; } init_state = 0;
    snprintf(tool_kind, sizeof tool_kind, "%s", kind); snprintf(remote_name, sizeof remote_name, "%s", name);
    pthread_mutex_lock(&q_lock); q_head = q_len = 0; pthread_mutex_unlock(&q_lock);
    if (!started) {
        pthread_t t; if (pthread_create(&t, NULL, ui_thread, NULL) != 0) { pthread_mutex_unlock(&init_lock); return -1; }
        pthread_detach(t); started = 1;
    } else g_idle_add(show_window, NULL);
    while (!init_state) pthread_cond_wait(&init_cond, &init_lock);
    int s = init_state; pthread_mutex_unlock(&init_lock); return s == 1 ? 0 : -1;
}
static gboolean hide_window(gpointer d) { (void)d; if (window) gtk_widget_hide(window); atomic_store(&closed, 1); return G_SOURCE_REMOVE; }
void uu_tools_ui_hide(void) { g_idle_add(hide_window, NULL); }
void uu_tools_ui_present(void) { pthread_mutex_lock(&init_lock); if (init_state == 1) g_idle_add(show_window, NULL); pthread_mutex_unlock(&init_lock); }
