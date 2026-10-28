// Chinese / English UI text. The language is decided once on the Rust side (src/i18n.rs: UU_LANG, the
// choice saved in settings, then the locale) so the GTK windows and Rust messages always agree.
#ifndef UU_I18N_H
#define UU_I18N_H
int uu_i18n_english(void);
int uu_i18n_save(int english);
#define L(zh, en) (uu_i18n_english() ? (en) : (zh))
#endif
