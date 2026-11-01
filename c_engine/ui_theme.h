#ifndef UU_UI_THEME_H
#define UU_UI_THEME_H
#include <gtk/gtk.h>

// Call on the GTK thread, after gtk_init_check. Shared by the host and viewer tools.
void uu_theme_init(void);
GtkWidget *uu_theme_selector(void);
GtkWidget *uu_theme_settings_button(GtkWindow *parent);
void uu_theme_load_css(const char *css);
#endif
