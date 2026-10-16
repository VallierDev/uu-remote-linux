#ifndef UU_PORTAL_ENGINE_H
#define UU_PORTAL_ENGINE_H
#include <stddef.h>
#include <stdint.h>
struct uu_portal;
/* One object owns one consent/session and one PipeWire stream. */
struct uu_portal *uu_portal_open(int *error, uint32_t *width, uint32_t *height);
/* Saves the rotated token immediately after Start, before video initialization. */
struct uu_portal *uu_portal_open_persist(const char *restore, void (*save_token)(const char *, void *), void *data, int *error, uint32_t *width, uint32_t *height);
struct uu_portal *uu_portal_open_restore(const char *restore, int *error, uint32_t *width, uint32_t *height);
const char *uu_portal_restore_token(struct uu_portal *portal);
void uu_portal_close(struct uu_portal *portal);
int uu_portal_clipboard_ready(struct uu_portal *portal);
int uu_portal_clipboard_set(struct uu_portal *portal, const uint8_t *text, size_t length);
int uu_portal_clipboard_poll(struct uu_portal *portal, uint8_t *text, size_t capacity);
int uu_portal_alive(struct uu_portal *portal);
int uu_portal_capture_alive(struct uu_portal *portal);
void uu_portal_input_dispatch(struct uu_portal *portal);
void uu_portal_logical_size(struct uu_portal *portal, uint32_t *width, uint32_t *height);
void uu_portal_reset_frame(struct uu_portal *portal);
int uu_portal_capture(struct uu_portal *portal, uint8_t *bgra, size_t length);
int uu_portal_capture_since(struct uu_portal *portal, uint8_t *bgra, size_t length, uint64_t *revision);
int uu_portal_move(struct uu_portal *portal, double x, double y, int relative);
int uu_portal_button(struct uu_portal *portal, int button, int pressed);
int uu_portal_key(struct uu_portal *portal, uint32_t keysym, int keycode, int pressed);
#endif
