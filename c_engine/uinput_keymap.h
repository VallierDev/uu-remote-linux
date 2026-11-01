#ifndef UU_UINPUT_KEYMAP_H
#define UU_UINPUT_KEYMAP_H
#include <stddef.h>
#include <stdint.h>
typedef struct { uint16_t code, shift; } UuTextKey;
/* Read-only XKB lookup; emits no input and never changes the keymap or clipboard.
 * Return the number of initial ASCII characters representable by physical keys. */
size_t uu_uinput_text_plan(const uint8_t *text, size_t length, UuTextKey *plan, size_t capacity);
#endif
