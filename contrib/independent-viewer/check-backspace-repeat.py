"""Exercise the adapter against real SDL's event queue, using a dummy window."""
import ctypes as C
import ctypes.util
import json

class Keysym(C.Structure):
    _fields_ = [('scancode', C.c_int), ('sym', C.c_int),
                ('mod', C.c_ushort), ('unused', C.c_uint)]

class Keyboard(C.Structure):
    _fields_ = [('type', C.c_uint), ('timestamp', C.c_uint),
                ('window_id', C.c_uint), ('state', C.c_ubyte),
                ('repeat', C.c_ubyte), ('padding', C.c_ubyte * 2),
                ('keysym', Keysym)]

class Event(C.Union):
    _fields_ = [('type', C.c_uint), ('key', Keyboard),
                ('padding', C.c_ubyte * 56), ('alignment', C.c_void_p)]

sdl = C.CDLL(ctypes.util.find_library('SDL2-2.0') or ctypes.util.find_library('SDL2'), mode=C.RTLD_GLOBAL)
symbols = C.CDLL(None)
sdl.SDL_Init.argtypes = [C.c_uint]
sdl.SDL_Init.restype = C.c_int
sdl.SDL_PushEvent.argtypes = [C.POINTER(Event)]
symbols.SDL_PollEvent.argtypes = [C.POINTER(Event)]
symbols.SDL_PollEvent.restype = C.c_int
assert C.sizeof(Event) == 56
assert sdl.SDL_Init(0x20) == 0
try:
    event = Event()
    while symbols.SDL_PollEvent(C.byref(event)):
        pass
    inputs = [(225, 1, 0), (42, 1, 0), (42, 1, 1), (42, 1, 1),
              (42, 0, 0), (225, 0, 0), (4, 1, 0), (4, 1, 1), (4, 0, 0)]
    for scan, down, repeat in inputs:
        event = Event()
        event.key.type = 0x300 if down else 0x301
        event.key.window_id = 123
        event.key.state = down
        event.key.repeat = repeat
        event.key.keysym.scancode = scan
        event.key.keysym.mod = 1 if scan == 225 and down or scan == 42 else 0
        assert sdl.SDL_PushEvent(C.byref(event)) == 1
    outputs = []
    while symbols.SDL_PollEvent(C.byref(event)):
        outputs.append((event.key.keysym.scancode, event.key.state, event.key.repeat))
        assert event.key.window_id == 123
    expected = [(225, 1, 0), (42, 1, 0), (42, 0, 0), (42, 1, 0),
                (42, 0, 0), (42, 1, 0), (42, 0, 0), (225, 0, 0),
                (4, 1, 0), (4, 0, 0), (4, 1, 0), (4, 0, 0)]
    assert outputs == expected, 'Unexpected SDL queue transformation'
    held = set()
    presses = 0
    for scan, down, repeat in outputs:
        if repeat:
            continue  # Matches the stock viewer's filter.
        if down and scan not in held:
            held.add(scan)
            presses += scan == 42
        elif not down:
            held.discard(scan)
    assert presses == 3 and not held
    assert symbols.SDL_PollEvent(C.byref(event)) == 0
    print(json.dumps({'backspace_presses': presses, 'all_keys_released': not held,
                      'typing_repeat_supported': True, 'no_queued_repeat_tail': True}))
finally:
    sdl.SDL_Quit()
