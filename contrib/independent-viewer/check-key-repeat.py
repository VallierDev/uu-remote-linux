"""Typing repeats, shortcut safety and release/focus boundaries in real SDL."""
import runpy
from pathlib import Path

base = runpy.run_path(str(Path(__file__).with_name('check-shift.py')))
key, exercise, focus_lost, sdl = (base[name] for name in ('key', 'exercise', 'focus_lost', 'sdl'))


def repeated(scan, mod=0):
    event = key(scan, 1, mod)
    event.key.repeat = 1
    return event


assert sdl.SDL_Init(0x20) == 0
checks = 0
try:
    scans = [*range(4, 40), *range(44, 57), *range(84, 88), *range(89, 101), 103]
    for scan in scans:
        for shift, mod in [(None, 0), (225, 1), (229, 2)]:
            seq = [] if shift is None else [key(shift, 1, mod)]
            seq += [key(scan, 1, mod), repeated(scan, mod), repeated(scan, mod), key(scan, 0, mod)]
            if shift is not None:
                seq += [key(shift, 0)]
            held = frozenset([scan] if shift is None else [scan, shift])
            assert exercise(seq) == [(scan, held)] * 3
            checks += 1
    # Ctrl+V/C/Z/W, Alt and Command chords must only fire once, even when
    # the modifier is released before the letter while OS repeats continue.
    for modifier, mod in [(224, 0x40), (228, 0x80), (226, 0x100), (230, 0x200),
                          (227, 0x400), (231, 0x800)]:
        for scan in [25, 6, 29, 26]:
            seq = [key(modifier, 1, mod), key(scan, 1, mod), repeated(scan, mod)]
            assert len(exercise(seq + [key(scan, 0, mod), key(modifier, 0)])) == 1
            checks += 1
            assert len(exercise(seq + [key(modifier, 0), repeated(scan), key(scan, 0)])) == 1
            checks += 1
    for scan in [40, 43, 41, 58]:  # Return, Tab, Escape, F1
        assert len(exercise([key(scan, 1), repeated(scan), key(scan, 0)])) == 1
        checks += 1
    for scan in [4, 31, 42]:
        assert len(exercise([key(scan, 1), key(scan, 0), repeated(scan)])) == 1
        assert len(exercise([key(scan, 1), focus_lost(), repeated(scan), key(scan, 0)])) == 1
        checks += 2
    assert len(exercise([key(4, 1), repeated(4), key(4, 0), key(5, 1), repeated(5), key(5, 0)])) == 4
    checks += 1
    print({'held_key_scenarios_passed': checks, 'release_and_focus_stop_repeat': True,
           'shortcut_repeat_suppressed': True})
finally:
    sdl.SDL_Quit()
