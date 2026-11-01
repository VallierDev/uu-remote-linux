/* Read-only live-keymap regression: cc tests/uinput_keymap.c -o /tmp/uu-keymap-test -lX11
 * Requires DISPLAY (X11 or XWayland). Never injects keys or changes the server keymap. */
#include <assert.h>
#include <stdio.h>
#include "../c_engine/uinput_keymap.c"

int main(void) {
    Display *display=XOpenDisplay(NULL);
    assert(display);
    XkbDescPtr map=XkbGetMap(display,XkbAllClientInfoMask,XkbUseCoreKbd);
    assert(map && XkbGetNames(display,XkbKeyNamesMask,map)==Success);
    XkbStateRec state={0};
    XkbStateRec actual;
    assert(XkbGetState(display,XkbUseCoreKbd,&actual)==Success);
    state.group=actual.group;
    uint8_t characters[97];
    for (unsigned i=0;i<95;i++) characters[i]=0x20+i;
    characters[95]='\t'; characters[96]='\n';
    UuTextKey plan[97];
    for (unsigned locks=0;locks<3;locks++) {
        state.locked_mods=locks==0 ? 0 : locks==1 ? LockMask : Mod2Mask;
        assert(text_plan(map,&state,characters,sizeof(characters),plan,97)==97);
        for (unsigned i=0;i<97;i++) {
            KeySym symbol=NoSymbol; unsigned consumed=0;
            unsigned modifiers=XkbBuildCoreState(state.locked_mods | (plan[i].shift ? ShiftMask : 0),state.group);
            assert(XkbTranslateKeyCode(map,plan[i].code+8,modifiers,&consumed,&symbol));
            assert(symbol==(characters[i]=='\n' ? XK_Return : characters[i]=='\t' ? XK_Tab : characters[i]));
            assert(plan[i].code>=1 && plan[i].code<=248 && plan[i].shift<=248);
        }
    }
    state.locked_mods=0;
    const uint8_t mixed[]={'a','b',0xc3,0xa9};
    assert(text_plan(map,&state,mixed,sizeof(mixed),plan,97)==2);
    assert(text_plan(map,&state,characters,sizeof(characters),plan,2)==2);
    state.base_mods=ControlMask;
    assert(text_plan(map,&state,characters,sizeof(characters),plan,97)==0);
    state.base_mods=0; state.latched_mods=ShiftMask;
    assert(text_plan(map,&state,characters,sizeof(characters),plan,97)==0);
    state.latched_mods=0;
    XkbKeyNameRec previous=map->names->keys[38];
    memcpy(map->names->keys[38].name,"NONE",4);
    assert(text_plan(map,&state,characters,sizeof(characters),plan,97)==0);
    map->names->keys[38]=previous;
    XkbFreeKeyboard(map,XkbAllComponentsMask,True);
    XCloseDisplay(display);
    puts("PASS: ASCII, shift/lock modifiers, partial prefixes and keycode validation");
    return 0;
}
