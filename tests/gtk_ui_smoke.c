// Offline fixtures only: no account, QR, cloud, or desktop capture is used.
#include <gtk/gtk.h>
#include <gdk/gdkx.h>
#include <cairo-xlib.h>
#include <string.h>
#include <stdlib.h>
int uu_i18n_english(void) { return g_strcmp0(g_getenv("UU_LANG"), "zh") != 0; }
int uu_i18n_save(int english) { (void)english; return 1; }
#if defined(UI_HOST)
#include "../c_engine/assist_window.c"
#elif defined(UI_VIEWER)
#include "../c_engine/viewer_window.c"
#else
#include <unistd.h>
static int fixture_hostname(char *name, size_t len) { g_strlcpy(name, "Local workstation", len); return 0; }
#define gethostname fixture_hostname
#include "../c_engine/viewer_tools.c"
#endif

static GtkWidget *appearance_combo(GtkWidget *w) {
    if (GTK_IS_COMBO_BOX(w) && !g_strcmp0(atk_object_get_name(gtk_widget_get_accessible(w)), L("外观", "Appearance"))) return w;
    if (!GTK_IS_CONTAINER(w)) return NULL;
    GList *children = gtk_container_get_children(GTK_CONTAINER(w));
    GtkWidget *found = NULL;
    for (GList *c = children; c && !found; c = c->next) found = appearance_combo(c->data);
    g_list_free(children);
    return found;
}

#if !defined(UI_VIEWER)
static GtkWidget *settings_button(GtkWidget *w) {
    if (GTK_IS_BUTTON(w) && !g_strcmp0(atk_object_get_name(gtk_widget_get_accessible(w)), L("设置", "Settings"))) return w;
    if (!GTK_IS_CONTAINER(w)) return NULL;
    GList *children = gtk_container_get_children(GTK_CONTAINER(w));
    GtkWidget *found = NULL;
    for (GList *c = children; c && !found; c = c->next) found = settings_button(c->data);
    g_list_free(children);
    return found;
}

#endif

static gboolean snapshot(GtkWidget *w, const char *path) {
    static int attempts;
    int width = gtk_widget_get_allocated_width(w), height = gtk_widget_get_allocated_height(w);
    GdkWindow *native = gtk_widget_get_window(w);
    int x, y;
    gdk_window_get_origin(native, &x, &y);
    g_assert_cmpint(x + width, <=, gdk_screen_width());
    g_assert_cmpint(y + height, <=, gdk_screen_height());
    gdk_window_process_updates(native, TRUE);
    gdk_display_sync(gtk_widget_get_display(w));
    // GTK 3.24.33's get_from_window marks its cached surface dirty without flushing;
    // Cairo 1.16 can abort on attached MIME data. Read through a fresh X11 surface.
    cairo_surface_t *surface = cairo_xlib_surface_create(GDK_WINDOW_XDISPLAY(native),
        GDK_WINDOW_XID(native), GDK_VISUAL_XVISUAL(gdk_window_get_visual(native)), width, height);
    g_assert_cmpint(cairo_surface_status(surface), ==, CAIRO_STATUS_SUCCESS);
    GdkPixbuf *shot = gdk_pixbuf_get_from_surface(surface, 0, 0, width, height);
    cairo_surface_destroy(surface);
    g_assert_nonnull(shot);
    const guchar *pixels = gdk_pixbuf_read_pixels(shot);
    int low = 255, high = 0;
    for (int row = 0; row < height; row++) {
        for (int col = 0; col < width; col++) {
            int v = pixels[row * gdk_pixbuf_get_rowstride(shot) + col * gdk_pixbuf_get_n_channels(shot)];
            low = MIN(low, v); high = MAX(high, v);
        }
    }
    if (high - low <= 80) {
        // A loaded CI runner may not have painted yet; retry through the main loop, with a bound.
        g_object_unref(shot);
        g_assert_cmpint(++attempts, <, 10);
        return FALSE;
    }
    attempts = 0;
    g_assert_true(gdk_pixbuf_save(shot, path, "png", NULL, NULL));
    g_object_unref(shot);
    return TRUE;
}

static const char *output;
static int expected_mode, stage;
#if defined(UI_VIEWER)
static void verify_badge(void) {
    for (int i = 0; i < 2; i++) {
        show_device(i); // Fixture 0 is online, fixture 1 is offline.
        GtkStyleContext *ctx = gtk_widget_get_style_context(badge);
        GdkRGBA actual, expected;
        g_assert_cmpint(gtk_style_context_has_class(ctx, "off"), ==, i == 1);
        g_assert_true(gtk_style_context_lookup_color(ctx, i ? "uu_muted" : "uu_success", &expected));
        gtk_style_context_get_color(ctx, GTK_STATE_FLAG_NORMAL, &actual);
        g_assert_cmpfloat_with_epsilon(actual.red, expected.red, 0.005);
        g_assert_cmpfloat_with_epsilon(actual.green, expected.green, 0.005);
        g_assert_cmpfloat_with_epsilon(actual.blue, expected.blue, 0.005);
    }
}
#endif
static gboolean verify(gpointer unused) {
    (void)unused;
    if (!stage) {
        g_assert_null(appearance_combo(window)); // Appearance belongs in Settings.
        if (!snapshot(window, output)) return G_SOURCE_CONTINUE;
#if defined(UI_VIEWER)
        open_settings();
#else
        GtkWidget *button = settings_button(window);
        g_assert_nonnull(button);
        gtk_button_clicked(GTK_BUTTON(button));
#endif
        stage++;
        return G_SOURCE_CONTINUE;
    }
    GList *windows = gtk_window_list_toplevels();
    GtkWidget *combo = NULL, *settings = NULL;
    for (GList *w = windows; w && !combo; w = w->next) {
        combo = appearance_combo(w->data);
        if (combo) settings = w->data;
    }
    g_list_free(windows);
    g_assert_nonnull(combo);
    g_assert_cmpint(gtk_combo_box_get_active(GTK_COMBO_BOX(combo)), ==, expected_mode);
    if (stage == 1) {
        char *path = g_strconcat(output, ".settings.png", NULL);
        gboolean painted = snapshot(settings, path); g_free(path);
        if (!painted) return G_SOURCE_CONTINUE;
#if defined(UI_VIEWER)
        verify_badge();
#endif
        expected_mode = expected_mode == 2 ? 1 : 2;
        gtk_combo_box_set_active(GTK_COMBO_BOX(combo), expected_mode);
        stage++;
        return G_SOURCE_CONTINUE;
    }
    gboolean dark = FALSE;
    g_object_get(gtk_settings_get_default(), "gtk-application-prefer-dark-theme", &dark, NULL);
    g_assert_cmpint(dark, ==, expected_mode == 2);
#if defined(UI_VIEWER)
    verify_badge();
#endif
    char *path = g_build_filename(g_get_user_config_dir(), "uuremote", "theme", NULL), *saved = NULL;
    g_assert_true(g_file_get_contents(path, &saved, NULL, NULL));
    g_assert_cmpstr(saved, ==, expected_mode == 2 ? "dark" : "light");
    g_free(path); g_free(saved);
    // System must release the application override and restore the test desktop's light default.
    gtk_combo_box_set_active(GTK_COMBO_BOX(combo), 0);
    g_object_get(gtk_settings_get_default(), "gtk-application-prefer-dark-theme", &dark, NULL);
    g_assert_false(dark);
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

int main(int argc, char **argv) {
    g_assert_cmpint(argc, >=, 3);
    output = argv[1]; expected_mode = atoi(argv[2]);
    gtk_init(NULL, NULL);
    g_object_set(gtk_settings_get_default(), "gtk-enable-animations", FALSE, NULL);
    g_timeout_add(400, verify, NULL);
#if defined(UI_HOST)
    show_account = 1;
    build_window();
    gtk_main();
#elif defined(UI_VIEWER)
    uu_viewer_ui_set_devices("demo-workstation\tWorkstation\t1\t1\t0\ndemo-laptop\tLaptop\t4\t0\t0\n");
    logged_in = 1;
    g_idle_add(apply_devices, NULL);
    g_idle_add(apply_login, NULL);
    run(NULL);
#else
    g_strlcpy(remote_name, "Workstation", sizeof remote_name);
    if (argc > 3 && !strcmp(argv[3], "terminal")) build_terminal();
    else if (argc > 3 && !strcmp(argv[3], "portmap")) build_portmap();
    else build_files();
    gtk_widget_show_all(window);
    gtk_main();
#endif
    return 0;
}
