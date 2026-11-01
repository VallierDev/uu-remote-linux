// One palette for all GTK windows. Named GTK colors also follow live system theme changes.
#include "ui_theme.h"
#include "i18n.h"
#include <glib/gstdio.h>
#include <string.h>

static int initialized, mode;
static const char *modes[] = {"system", "light", "dark"};
static const char *CSS =
    "* { font-family: 'Noto Sans CJK SC', 'Noto Sans SC', sans-serif; }\n"
    "@define-color uu_bg mix(@theme_bg_color, @theme_base_color, 0.5);\n"
    "@define-color uu_surface @theme_base_color;\n"
    "@define-color uu_fg @theme_fg_color;\n"
    "@define-color uu_muted mix(@theme_fg_color, @theme_bg_color, 0.25);\n"
    "@define-color uu_border mix(@theme_bg_color, @theme_fg_color, 0.18);\n"
    "@define-color uu_accent @theme_selected_bg_color;\n"
    "@define-color uu_accent_fg @theme_selected_fg_color;\n"
    "@define-color uu_tint mix(@theme_base_color, @theme_selected_bg_color, 0.12);\n"
    "@define-color uu_success mix(@theme_fg_color, #16a34a, 0.55);\n"
    "@define-color uu_error mix(@theme_fg_color, #dc2626, 0.65);\n"
    "window { background-color: @uu_bg; color: @uu_fg; }\n"
    "headerbar { background-image: none; background-color: @uu_bg; box-shadow: none; border-bottom: 1px solid @uu_border; }\n"
    "button { background-image: none; background-color: @uu_surface; color: @uu_fg; border: 1px solid @uu_border; border-radius: 8px; box-shadow: none; text-shadow: none; padding: 7px 12px; }\n"
    "button:hover { background-color: @uu_tint; border-color: @uu_accent; }\n"
    "button:active, button:checked { background-color: @uu_tint; border-color: @uu_accent; }\n"
    "button:disabled { color: mix(@uu_fg, @uu_surface, 0.55); background-color: @uu_bg; border-color: @uu_border; }\n"
    "button:focus, entry:focus { border-color: @uu_accent; outline-color: @uu_accent; outline-style: solid; outline-width: 2px; outline-offset: 2px; }\n"
    "entry { background-image: none; background-color: @uu_surface; color: @uu_fg; border: 1px solid @uu_border; border-radius: 8px; box-shadow: none; padding: 7px 10px; }\n"
    "entry:disabled { color: @uu_muted; background-color: @uu_bg; }\n"
    "separator { background-color: @uu_border; min-height: 1px; min-width: 1px; }\n"
    ".card, .cardbtn, .banner { background-color: @uu_surface; border: 1px solid @uu_border; border-radius: 12px; }\n"
    ".heading, .title, .h2, .user { font-size: 22px; font-weight: 700; color: @uu_fg; }\n"
    ".section, .group { font-size: 16px; font-weight: 600; color: @uu_fg; }\n"
    ".big { font-size: 32px; font-weight: 700; color: @uu_fg; }\n"
    ".key, .hint, .empty { font-size: 14px; color: @uu_muted; }\n"
    ".note, .footer { font-size: 13px; color: @uu_muted; }\n"
    ".status { font-size: 13px; color: @uu_muted; }\n"
    ".status.ok, .online, .dot { color: @uu_success; }\n"
    ".status.error { color: @uu_error; }\n"
    ".tag { background-color: @uu_tint; color: @uu_fg; border-radius: 6px; padding: 3px 8px; font-size: 12px; }\n"
    ".tag.remote { color: @uu_success; }\n"
    "button.primary, button.enter { background-color: @uu_accent; color: @uu_accent_fg; border-color: transparent; font-weight: 600; padding: 9px 24px; }\n"
    "button.primary:hover, button.enter:hover { background-color: shade(@uu_accent, 0.9); }\n"
    "button.primary:active, button.enter:active { background-color: shade(@uu_accent, 0.8); }\n"
    "button.primary:disabled, button.enter:disabled { background-color: @uu_bg; color: @uu_muted; border-color: @uu_border; }\n"
    "button.flat, button.hbtn, button.tool { background-color: transparent; border-color: transparent; }\n"
    "button.flat:hover, button.hbtn:hover, button.tool:hover, button.cardbtn:hover { background-color: @uu_tint; border-color: @uu_border; }\n"
    ".toast { background-color: @uu_fg; color: @uu_surface; border-radius: 10px; padding: 12px 18px; font-size: 14px; }\n"
    ".qrbox { background-color: white; border: 1px solid @uu_border; border-radius: 12px; padding: 14px; }\n";

void uu_theme_load_css(const char *css) {
    GtkCssProvider *provider = gtk_css_provider_new();
    GError *error = NULL;
    if (gtk_css_provider_load_from_data(provider, css, -1, &error))
        gtk_style_context_add_provider_for_screen(gdk_screen_get_default(), GTK_STYLE_PROVIDER(provider), GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    else { g_warning("Could not load UI stylesheet"); g_clear_error(&error); }
    g_object_unref(provider);
}

static char *theme_path(void) {
    return g_build_filename(g_get_user_config_dir(), "uuremote", "theme", NULL);
}

static void apply_mode(void) {
    GtkSettings *settings = gtk_settings_get_default();
    if (!mode) {
        // Reset the application override, so subsequent desktop changes still propagate.
        gtk_settings_reset_property(settings, "gtk-theme-name");
        gtk_settings_reset_property(settings, "gtk-application-prefer-dark-theme");
    } else {
        g_object_set(settings, "gtk-theme-name", "Adwaita", "gtk-application-prefer-dark-theme", mode == 2, NULL);
    }
}

void uu_theme_init(void) {
    if (initialized) return;
    initialized = 1;
    char *path = theme_path(), *value = NULL;
    if (g_file_get_contents(path, &value, NULL, NULL)) {
        g_strstrip(value);
        for (int i = 0; i < 3; i++) if (!strcmp(value, modes[i])) mode = i;
    }
    g_free(value); g_free(path);
    apply_mode();
    uu_theme_load_css(CSS);
}

static void on_theme(GtkComboBox *combo, gpointer note) {
    int selected = gtk_combo_box_get_active(combo);
    if (selected < 0 || selected > 2) return;
    mode = selected;
    apply_mode();
    char *path = theme_path(), *dir = g_path_get_dirname(path);
    gboolean saved = g_mkdir_with_parents(dir, 0700) == 0 && g_file_set_contents(path, modes[mode], -1, NULL);
    gtk_label_set_text(GTK_LABEL(note), saved ? "" : L("无法保存外观设置", "Could not save appearance"));
    gtk_widget_set_visible(GTK_WIDGET(note), !saved);
    g_free(dir); g_free(path);
}

GtkWidget *uu_theme_selector(void) {
    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 6), *row = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 8);
    GtkWidget *label = gtk_label_new(L("外观", "Appearance")), *combo = gtk_combo_box_text_new(), *note = gtk_label_new("");
    gtk_style_context_add_class(gtk_widget_get_style_context(label), "key");
    gtk_style_context_add_class(gtk_widget_get_style_context(note), "status");
    gtk_style_context_add_class(gtk_widget_get_style_context(note), "error");
    gtk_label_set_line_wrap(GTK_LABEL(note), TRUE);
    gtk_widget_set_no_show_all(note, TRUE);
    gtk_widget_set_halign(note, GTK_ALIGN_START);
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(combo), L("跟随系统", "System"));
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(combo), L("浅色", "Light"));
    gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(combo), L("深色", "Dark"));
    gtk_combo_box_set_active(GTK_COMBO_BOX(combo), mode);
    atk_object_set_name(gtk_widget_get_accessible(combo), L("外观", "Appearance"));
    g_signal_connect(combo, "changed", G_CALLBACK(on_theme), note);
    gtk_box_pack_start(GTK_BOX(row), label, FALSE, FALSE, 0);
    gtk_box_pack_end(GTK_BOX(row), combo, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(box), row, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(box), note, FALSE, FALSE, 0);
    return box;
}

static void open_settings(GtkButton *button, gpointer parent) {
    (void)button;
    GtkWidget *dialog = gtk_dialog_new_with_buttons(L("设置", "Settings"), GTK_WINDOW(parent),
        GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT, L("关闭", "Close"), GTK_RESPONSE_CLOSE, NULL);
    GtkWidget *area = gtk_dialog_get_content_area(GTK_DIALOG(dialog));
    gtk_window_set_default_size(GTK_WINDOW(dialog), 360, -1);
    gtk_container_set_border_width(GTK_CONTAINER(area), 24);
    gtk_box_pack_start(GTK_BOX(area), uu_theme_selector(), FALSE, FALSE, 0);
    g_signal_connect_swapped(dialog, "response", G_CALLBACK(gtk_widget_destroy), dialog);
    gtk_widget_show_all(dialog);
}

GtkWidget *uu_theme_settings_button(GtkWindow *parent) {
    GtkWidget *button = gtk_button_new_from_icon_name("preferences-system-symbolic", GTK_ICON_SIZE_BUTTON);
    gtk_style_context_add_class(gtk_widget_get_style_context(button), "flat");
    gtk_widget_set_tooltip_text(button, L("设置", "Settings"));
    atk_object_set_name(gtk_widget_get_accessible(button), L("设置", "Settings"));
    g_signal_connect(button, "clicked", G_CALLBACK(open_settings), parent);
    return button;
}
