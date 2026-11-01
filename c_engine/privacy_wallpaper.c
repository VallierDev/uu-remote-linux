// Privacy screen, wallpaper mode: a fullscreen keep-above window on each physical monitor showing
// a picture and a notice, while the remote session works on a KWin virtual output. Runs in its own
// process (`uuremote-server privacy-wallpaper`), which the host kills on restore.
#include <gtk/gtk.h>
#include <stdio.h>
#include <stdlib.h>
#include "i18n.h"

typedef struct { int x, y, w, h; } Rect;
static GdkPixbuf *picture;

static gboolean host_closed(GIOChannel *channel, GIOCondition condition, gpointer data) {
    (void)channel; (void)condition; (void)data;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

static gboolean draw(GtkWidget *widget, cairo_t *cr, gpointer data) {
    (void)data;
    int w = gtk_widget_get_allocated_width(widget), h = gtk_widget_get_allocated_height(widget);
    cairo_set_source_rgb(cr, 0, 0, 0); cairo_paint(cr);
    if (picture) {
        // Cover: scale to fill, crop the overflow, keep the aspect ratio.
        double pw = gdk_pixbuf_get_width(picture), ph = gdk_pixbuf_get_height(picture);
        double s = w / pw > h / ph ? w / pw : h / ph;
        cairo_save(cr);
        cairo_translate(cr, (w - pw * s) / 2, (h - ph * s) / 2);
        cairo_scale(cr, s, s);
        gdk_cairo_set_source_pixbuf(cr, picture, 0, 0);
        cairo_paint(cr);
        cairo_restore(cr);
    }
    PangoLayout *layout = gtk_widget_create_pango_layout(widget, L("此设备正在被远程控制", "This device is being remotely controlled"));
    PangoFontDescription *font = pango_font_description_from_string("Sans 20");
    pango_layout_set_font_description(layout, font);
    int tw, th; pango_layout_get_pixel_size(layout, &tw, &th);
    cairo_set_source_rgba(cr, 0, 0, 0, 0.45);
    cairo_rectangle(cr, (w - tw) / 2.0 - 24, h - th - 96, tw + 48, th + 32); cairo_fill(cr);
    cairo_set_source_rgb(cr, 1, 1, 1);
    cairo_move_to(cr, (w - tw) / 2.0, h - th - 80);
    pango_cairo_show_layout(cr, layout);
    pango_font_description_free(font); g_object_unref(layout);
    return TRUE;
}

/* `rects` are the physical outputs in logical coordinates; monitors elsewhere (the virtual output)
 * stay uncovered. Returns when every window is closed or the process is killed. */
int uu_privacy_wallpaper_run(const char *image, const int *rects, int count) {
    if (!gtk_init_check(NULL, NULL)) return 1;
    if (image && *image) picture = gdk_pixbuf_new_from_file(image, NULL);
    GdkDisplay *display = gdk_display_get_default();
    int shown = 0;
    for (int i = 0; i < gdk_display_get_n_monitors(display); i++) {
        GdkMonitor *monitor = gdk_display_get_monitor(display, i);
        GdkRectangle g; gdk_monitor_get_geometry(monitor, &g);
        int physical = 0;
        for (int r = 0; r < count; r++) {
            const int *c = rects + r * 4;
            if (c[0] == g.x && c[1] == g.y && c[2] == g.width && c[3] == g.height) physical = 1;
        }
        if (!physical) continue;
        GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
        gtk_window_set_title(GTK_WINDOW(window), "uu-remote-linux privacy");
        gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
        gtk_window_set_keep_above(GTK_WINDOW(window), TRUE);
        gtk_window_set_skip_taskbar_hint(GTK_WINDOW(window), TRUE);
        gtk_window_set_skip_pager_hint(GTK_WINDOW(window), TRUE);
        gtk_window_set_default_size(GTK_WINDOW(window), g.width, g.height);
        g_signal_connect(window, "draw", G_CALLBACK(draw), NULL);
        g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
        gtk_window_fullscreen_on_monitor(GTK_WINDOW(window), gtk_widget_get_screen(window), i);
        gtk_widget_show_all(window);
        shown++;
    }
    if (!shown) return 2;
    /* EOF follows the host process, including crashes, rather than its spawning worker thread. */
    GIOChannel *host = g_io_channel_unix_new(0);
    guint watch = g_io_add_watch(host, G_IO_HUP | G_IO_ERR | G_IO_NVAL, host_closed, NULL);
    /* Readiness handshake: the host treats picture mode as on only after this line. */
    fputs("ready\n", stdout); fflush(stdout);
    gtk_main();
    if (g_main_context_find_source_by_id(NULL, watch)) g_source_remove(watch);
    g_io_channel_unref(host);
    return 0;
}
