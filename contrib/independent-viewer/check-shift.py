"""Reproduce missing Shift transitions in real SDL, without a real keyboard."""
import runpy
from pathlib import Path

base = runpy.run_path(str(Path(__file__).with_name('check-pc-shortcuts.py')))
key, exercise, focus_lost = (base[name] for name in ('key', 'exercise', 'focus_lost'))
sdl = base['sdl']
assert sdl.SDL_Init(0x20) == 0
checks = 0
try:
    for shift, mod in [(225, 1), (229, 2)]:
        # US layout: Shift+2 is @, Shift+A..Z is uppercase. The viewer sends
        # physical keys; these checks assert the modifier precedes each key.
        for scan in [31, *range(4, 30)]:
            for missing_press in [False, True]:
                sequence = [] if missing_press else [key(shift, 1, mod)]
                sequence += [key(scan, 1, mod), key(scan, 0, mod), key(shift, 0)]
                assert exercise(sequence) == [(scan, frozenset([scan, shift]))]
                checks += 1
        # A missing release must not make the next letter uppercase.
        assert exercise([key(shift, 1, mod), key(31, 1, mod), key(31, 0, mod),
                         key(4, 1), key(4, 0)])[-1] == (4, frozenset([4]))
        checks += 1
        # Shift was already held when the window gained focus.
        assert exercise([focus_lost(), key(31, 1, mod), key(31, 0, mod),
                         key(shift, 0)]) == [(31, frozenset([31, shift]))]
        checks += 1
    assert exercise([key(31, 1, 3), key(31, 0, 3), key(225, 0, 2),
                     key(229, 0)]) == [(31, frozenset([31, 225, 229]))]
    checks += 1
    print({'shift_scenarios_passed': checks, 'left_and_right_shift': True,
           'missing_press_and_release_recovered': True})
finally:
    sdl.SDL_Quit()
