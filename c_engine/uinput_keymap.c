#include "uinput_keymap.h"
#include <X11/Xlib.h>
#include <X11/XKBlib.h>
#include <X11/keysym.h>
#include <string.h>

static size_t text_plan(XkbDescPtr map, const XkbStateRec *state,
    const uint8_t *text, size_t length, UuTextKey *plan, size_t capacity) {
    if (!map || !map->map || !map->map->modmap || !state || state->base_mods || state->latched_mods) return 0;
    /* Prove the XKB -> evdev offset from physical key names rather than assuming a US layout. */
    if (!map->names || !map->names->keys || map->min_key_code > 10 || map->max_key_code < 50 ||
        memcmp(map->names->keys[10].name,"AE01",4) ||
        memcmp(map->names->keys[38].name,"AC01",4) ||
        memcmp(map->names->keys[50].name,"LFSH",4)) return 0;
    uint16_t shift_code=0;
    for (unsigned code=9; code<=map->max_key_code; code++) {
        if (code>=map->min_key_code && code-8<=248 && (map->map->modmap[code] & ShiftMask)) {
            shift_code=code-8; break;
        }
    }
    UuTextKey lookup[128]={0};
    for (unsigned shift=0; shift<2; shift++) {
        if (shift && !shift_code) continue;
        for (unsigned code=9; code<=map->max_key_code; code++) {
            if (code<map->min_key_code || code-8>248) continue;
            KeySym symbol=NoSymbol;
            unsigned consumed=0;
            unsigned modifiers=XkbBuildCoreState(state->locked_mods | (shift ? ShiftMask : 0),state->group);
            if (!XkbTranslateKeyCode(map,code,modifiers,&consumed,&symbol)) continue;
            UuTextKey key={.code=code-8,.shift=shift ? shift_code : 0};
            if (symbol>=0x20 && symbol<=0x7e && !lookup[symbol].code) lookup[symbol]=key;
            else if (symbol==XK_Return && !lookup['\n'].code) lookup['\n']=lookup['\r']=key;
            else if (symbol==XK_Tab && !lookup['\t'].code) lookup['\t']=key;
        }
    }
    size_t count=0;
    while (count<length && count<capacity && text[count]<128 && lookup[text[count]].code) {
        plan[count]=lookup[text[count]];
        count++;
    }
    return count;
}

size_t uu_uinput_text_plan(const uint8_t *text, size_t length, UuTextKey *plan, size_t capacity) {
    if (!text || !length || !plan || !capacity || text[0]>=128) return 0;
    XInitThreads();
    Display *display=XOpenDisplay(NULL);
    if (!display) return 0;
    XkbDescPtr map=XkbGetMap(display,XkbAllClientInfoMask,XkbUseCoreKbd);
    XkbStateRec state;
    size_t count=0;
    if (map && XkbGetState(display,XkbUseCoreKbd,&state)==Success &&
        XkbGetNames(display,XkbKeyNamesMask,map)==Success)
        count=text_plan(map,&state,text,length,plan,capacity);
    if (map) XkbFreeKeyboard(map,XkbAllComponentsMask,True);
    XCloseDisplay(display);
    return count;
}
