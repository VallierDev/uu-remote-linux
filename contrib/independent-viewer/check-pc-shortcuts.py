"""Real SDL queue checks for shortcut ordering, modifier release, and focus loss."""
import ctypes as C
import json
import runpy
from pathlib import Path

base = runpy.run_path(str(Path(__file__).with_name('check-backspace-repeat.py')))
Event, sdl, symbols = (base[name] for name in ('Event', 'sdl', 'symbols'))
assert sdl.SDL_Init(0x20) == 0


def key(scan, down, mod=0):
    event = Event()
    event.key.type = 0x300 if down else 0x301
    event.key.state = down
    event.key.keysym.scancode = scan
    event.key.keysym.mod = mod
    event.key.window_id = 123
    return event


def focus_lost():
    event = Event()
    event.type = 0x200
    event.padding[12] = 13
    return event


def exercise(inputs):
    initial = Event()
    while symbols.SDL_PollEvent(C.byref(initial)):
        pass
    assert sdl.SDL_PushEvent(C.byref(focus_lost())) == 1
    for event in inputs:
        assert sdl.SDL_PushEvent(C.byref(event)) == 1
    held, pressed = set(), []
    event = Event()
    while symbols.SDL_PollEvent(C.byref(event)):
        if event.type == 0x200 and event.padding[12] == 13:
            held.clear()
        elif event.type in (0x300, 0x301) and not event.key.repeat:
            scan = event.key.keysym.scancode
            if event.type == 0x300:
                if scan not in held:
                    held.add(scan)
                    if scan < 224:
                        pressed.append((scan, frozenset(held)))
            else:
                held.discard(scan)
    assert not held, 'A modifier or key remained pressed'
    return pressed


checks = 0
try:
    for control, mod in [(224, 0x40), (228, 0x80)]:
        for scan in [4, 6, 25, 27, 29, 9, 22, 18, 17, 19, 26, 15, 23, 21]:
            for early_release in [False, True]:
                sequence = [key(control, 1, mod), key(scan, 1, mod)]
                sequence += ([key(control, 0), key(scan, 0)] if early_release else
                             [key(scan, 0, mod), key(control, 0)])
                presses = exercise(sequence)
                assert presses == [(scan, frozenset([scan, 227]))]
                checks += 1
    # Shift stays held for Command+Shift+Z.
    presses = exercise([key(224, 1, 0x40), key(225, 1, 0x41), key(29, 1, 0x41),
                        key(29, 0, 0x41), key(225, 0, 0x40), key(224, 0)])
    assert presses[-1] == (29, frozenset([225, 227, 29])); checks += 1
    # Ctrl+Alt+F stays available for the viewer's file controls.
    presses = exercise([key(224, 1, 0x40), key(226, 1, 0x140), key(9, 1, 0x140),
                        key(9, 0, 0x140), key(226, 0, 0x40), key(224, 0)])
    assert presses == [(9, frozenset([224, 226, 9]))]; checks += 1
    # Control remains Control for an unmapped terminal/special chord.
    presses = exercise([key(224, 1, 0x40), key(7, 1, 0x40), key(7, 0, 0x40), key(224, 0)])
    assert presses == [(7, frozenset([224, 7]))]; checks += 1
    # Two simultaneous Control keys, successive edits, and an interrupted chord.
    presses = exercise([key(224, 1, 0x40), key(228, 1, 0xc0), key(6, 1, 0xc0),
                        key(6, 0, 0xc0), key(25, 1, 0xc0), key(25, 0, 0xc0),
                        key(224, 0, 0x80), key(228, 0)])
    assert all(227 in held and 224 not in held and 228 not in held for _, held in presses)
    checks += 1
    presses = exercise([key(224, 1, 0x40), key(6, 1, 0x40), focus_lost(), key(4, 1), key(4, 0)])
    assert presses[-1] == (4, frozenset([4])); checks += 1
    # Physical Win pressed during a mapped chord must remain held until its own release.
    presses = exercise([key(224, 1, 0x40), key(6, 1, 0x40), key(227, 1, 0x440),
                        key(6, 0, 0x440), key(224, 0, 0x400), key(4, 1, 0x400),
                        key(4, 0, 0x400), key(227, 0)])
    assert presses[-1] == (4, frozenset([4, 227])); checks += 1
    print(json.dumps({'pc_shortcut_scenarios_passed': checks,
                      'ctrl_alt_preserved': True, 'focus_loss_releases_modifiers': True}))
finally:
    sdl.SDL_Quit()
