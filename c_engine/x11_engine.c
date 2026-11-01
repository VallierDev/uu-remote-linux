/**
 * Native Linux X11 High-Performance Capture & Input Injection Engine
 * Zero-Wine dependency, zero-overhead, thread-safe.
 */
#include <stdio.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <sys/shm.h>
#include <sys/ipc.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/Xatom.h>
#include <strings.h>
#include <X11/extensions/XShm.h>
#include <X11/extensions/XTest.h>
#include <X11/extensions/Xfixes.h>
#include <stdint.h>
#include <poll.h>
#include <pthread.h>

static Display *g_display = NULL;
static Window g_root = 0;
static int g_screen = 0;
static int g_width = 0;
static int g_height = 0;
static int g_depth = 0;

static XShmSegmentInfo g_shminfo;
static XImage *g_shm_image = NULL;
static int g_use_shm = 0;
/* Guards the capture geometry and SHM image against a resize between connections. */
static pthread_mutex_t g_capture_lock = PTHREAD_MUTEX_INITIALIZER;
static unsigned char g_injected_down[32]; // keycodes currently held by the remote viewer
static int g_injected_count = 0;
static int g_restore_repeat = 0;

// Injected keys must not auto-repeat on the host: the viewer sends its own repeats, and with
// network delay the key-up can arrive after the 500 ms X repeat delay ("wunnnnnnami").
// XTEST events repeat through the master keyboard, so global auto-repeat is switched off only
// while a remote key is held and restored when the last one is released.
static void track_injected_key(int keycode, int is_press) {
    if (keycode < 0 || keycode > 255) return;
    unsigned char bit = 1u << (keycode & 7), *slot = &g_injected_down[keycode >> 3];
    if (is_press && !(*slot & bit)) {
        if (g_injected_count++ == 0) {
            XKeyboardState state;
            XGetKeyboardControl(g_display, &state);
            g_restore_repeat = state.global_auto_repeat == AutoRepeatModeOn;
            if (g_restore_repeat) XAutoRepeatOff(g_display);
        }
        *slot |= bit;
    } else if (!is_press && (*slot & bit)) {
        *slot &= ~bit;
        if (--g_injected_count == 0 && g_restore_repeat) { XAutoRepeatOn(g_display); g_restore_repeat = 0; }
    }
}

/* XShmAttach fails asynchronously (BadAccess when the X server cannot reach our segment, e.g. a
 * server in another container); Xlib's default handler would exit the process before the XGetImage fallback. */
static volatile int g_shm_attach_failed = 0;
static int shm_attach_error(Display *display, XErrorEvent *event) {
    (void)display; (void)event;
    g_shm_attach_failed = 1;
    return 0;
}

static void shm_open_image(void) {
    g_use_shm = 0;
    if (!XShmQueryExtension(g_display)) return;
    g_shm_image = XShmCreateImage(g_display, DefaultVisual(g_display, g_screen),
                                  g_depth, ZPixmap, NULL, &g_shminfo, g_width, g_height);
    if (!g_shm_image) return;
    g_shminfo.shmid = shmget(IPC_PRIVATE, g_shm_image->bytes_per_line * g_shm_image->height, IPC_CREAT | 0777);
    if (g_shminfo.shmid == -1) return;
    g_shminfo.shmaddr = g_shm_image->data = (char *)shmat(g_shminfo.shmid, 0, 0);
    g_shminfo.readOnly = False;
    if (g_shminfo.shmaddr != (char *)-1) {
        XSync(g_display, False);
        g_shm_attach_failed = 0;
        int (*previous)(Display *, XErrorEvent *) = XSetErrorHandler(shm_attach_error);
        Status attached = XShmAttach(g_display, &g_shminfo);
        XSync(g_display, False);
        XSetErrorHandler(previous);
        if (attached && !g_shm_attach_failed) g_use_shm = 1;
        else fprintf(stderr, "X11 shared-memory capture unavailable; using XGetImage\n");
    }
    shmctl(g_shminfo.shmid, IPC_RMID, 0); // 标记删除，进程退出时自动回收；失败时也不遗留段
}
static void shm_close_image(void) {
    if (g_use_shm) XShmDetach(g_display, &g_shminfo);
    if (g_shm_image) {
        g_shm_image->data = NULL; /* shm memory is released by shmdt, not XDestroyImage */
        XDestroyImage(g_shm_image);
        g_shm_image = NULL;
    }
    if (g_shminfo.shmaddr && g_shminfo.shmaddr != (char *)-1) shmdt(g_shminfo.shmaddr);
    memset(&g_shminfo, 0, sizeof(g_shminfo));
    g_use_shm = 0;
}

int x11_init(const char *display_name) {
    if (g_display) return 0;
    
    XInitThreads();
    if (display_name && *display_name) {
        g_display = XOpenDisplay(display_name);
    } else {
        const char *configured = getenv("DISPLAY");
        /* An explicit DISPLAY that fails must not silently fall back to another (physical) screen. */
        if (configured && *configured) g_display = XOpenDisplay(configured);
        else {
            g_display = XOpenDisplay(":0");
            if (!g_display) g_display = XOpenDisplay(":1");
        }
    }
    if (!g_display) {
        return -1;
    }
    
    g_screen = DefaultScreen(g_display);
    g_root = RootWindow(g_display, g_screen);
    g_width = DisplayWidth(g_display, g_screen);
    g_height = DisplayHeight(g_display, g_screen);
    g_depth = DefaultDepth(g_display, g_screen);
    
    shm_open_image();
    return 0;
}

int x11_get_dimensions(int *out_w, int *out_h) {
    if (!g_display) return -1;
    pthread_mutex_lock(&g_capture_lock);
    *out_w = g_width;
    *out_h = g_height;
    pthread_mutex_unlock(&g_capture_lock);
    return 0;
}

/* The startup size goes stale when RandR/xrdp resizes the root window. Query the
 * server and rebuild the SHM image on change. Returns 1 if changed, 0 if not, <0 on error. */
int x11_refresh_geometry(void) {
    if (!g_display) return -1;
    Window root; int x, y; unsigned int w, h, border, depth;
    if (!XGetGeometry(g_display, g_root, &root, &x, &y, &w, &h, &border, &depth) || !w || !h) return -2;
    pthread_mutex_lock(&g_capture_lock);
    int changed = (int)w != g_width || (int)h != g_height;
    if (changed) {
        shm_close_image();
        g_width = (int)w; g_height = (int)h;
        shm_open_image();
    }
    pthread_mutex_unlock(&g_capture_lock);
    return changed;
}

/**
 * 截取一帧画面（RGBA/BGRA 格式）
 * @param out_buffer 输出缓冲区，大小必须至少为 width * height * 4
 */
/* *out_w/*out_h carry the caller's buffer geometry in; a mismatch (desktop resized
 * since that buffer was sized) returns -4 instead of overflowing it. */
int x11_capture_frame(unsigned char *out_buffer, int *out_w, int *out_h) {
    if (!g_display) return -1;
    pthread_mutex_lock(&g_capture_lock);
    int result = 0;
    if (*out_w != g_width || *out_h != g_height) result = -4;
    else if (g_use_shm && g_shm_image) {
        if (!XShmGetImage(g_display, g_root, g_shm_image, 0, 0, AllPlanes)) result = -2;
        else memcpy(out_buffer, g_shm_image->data, (size_t)g_width * g_height * 4);
    } else {
        // Fallback: XGetImage
        XImage *img = XGetImage(g_display, g_root, 0, 0, g_width, g_height, AllPlanes, ZPixmap);
        if (!img) result = -3;
        else { memcpy(out_buffer, img->data, (size_t)g_width * g_height * 4); XDestroyImage(img); }
    }
    *out_w = g_width;
    *out_h = g_height;
    pthread_mutex_unlock(&g_capture_lock);
    return result;
}

// ============ 原生输入注入 ============

const char *x11_display_name(void) {
    return g_display ? DisplayString(g_display) : NULL;
}

/* Reset display-idle activity during an active remote session without moving
 * the pointer or changing the screen-lock policy. */
int x11_wake_display(void) {
    if (!g_display) return -1;
    XTestFakeRelativeMotionEvent(g_display, 0, 0, CurrentTime);
    XFlush(g_display);
    return 0;
}

int x11_mouse_move(int x, int y) {
    if (!g_display) return -1;
    XTestFakeMotionEvent(g_display, g_screen, x, y, CurrentTime);
    XFlush(g_display);
    return 0;
}

int x11_mouse_relative(int dx, int dy) {
    if (!g_display) return -1;
    XTestFakeRelativeMotionEvent(g_display, dx, dy, CurrentTime);
    XFlush(g_display);
    return 0;
}

int x11_mouse_button(int button, int is_press) {
    if (!g_display) return -1;
    // button: 1=Left, 2=Middle, 3=Right, 4=ScrollUp, 5=ScrollDown
    int ok = XTestFakeButtonEvent(g_display, button, is_press ? True : False, CurrentTime);
    XFlush(g_display);
    return ok ? 0 : -2;
}

int x11_pointer_state(int *x, int *y, unsigned int *mask) {
    if (!g_display) return -1;
    Window root, child; int wx, wy;
    return XQueryPointer(g_display,g_root,&root,&child,x,y,&wx,&wy,mask) ? 0 : -2;
}

int x11_key_symbol(unsigned long symbol, int is_press) {
    if (!g_display) return -1;
    KeyCode code = XKeysymToKeycode(g_display, (KeySym)symbol);
    if (!code) return -2;
    if (is_press) track_injected_key(code, 1);
    XTestFakeKeyEvent(g_display, code, is_press ? True : False, CurrentTime);
    if (!is_press) track_injected_key(code, 0);
    XFlush(g_display);
    return 0;
}

/* Type UTF-8 text the way xdotool does: bind each character's keysym to a spare (unmapped) keycode, press it,
 * then restore the keycode. Works for CJK text from mobile input methods. Never logs the text. */
int x11_type_utf8(const char *text) {
    if (!g_display || !text) return -1;
    int min_kc = 0, max_kc = 0, per = 0;
    XDisplayKeycodes(g_display, &min_kc, &max_kc);
    KeySym *map = XGetKeyboardMapping(g_display, (KeyCode)min_kc, max_kc - min_kc + 1, &per);
    if (!map) return -2;
    int spare = 0;
    for (int kc = max_kc; kc >= min_kc && !spare; kc--) {
        int empty = 1;
        for (int i = 0; i < per; i++) if (map[(kc - min_kc) * per + i] != NoSymbol) { empty = 0; break; }
        if (empty) spare = kc;
    }
    XFree(map);
    if (!spare) return -3;
    int typed = 0;
    const unsigned char *p = (const unsigned char *)text;
    while (*p) {
        unsigned cp; int n;
        if (*p < 0x80) { cp = *p; n = 1; }
        else if ((*p & 0xE0) == 0xC0 && (p[1] & 0xC0) == 0x80) { cp = ((p[0] & 0x1F) << 6) | (p[1] & 0x3F); n = 2; }
        else if ((*p & 0xF0) == 0xE0 && (p[1] & 0xC0) == 0x80 && (p[2] & 0xC0) == 0x80) { cp = ((p[0] & 0x0F) << 12) | ((p[1] & 0x3F) << 6) | (p[2] & 0x3F); n = 3; }
        else if ((*p & 0xF8) == 0xF0 && (p[1] & 0xC0) == 0x80 && (p[2] & 0xC0) == 0x80 && (p[3] & 0xC0) == 0x80) { cp = ((p[0] & 0x07) << 18) | ((p[1] & 0x3F) << 12) | ((p[2] & 0x3F) << 6) | (p[3] & 0x3F); n = 4; }
        else { p++; continue; }
        p += n;
        KeySym sym;
        if (cp == '\n') sym = 0xff0d;          /* Return */
        else if (cp == '\t') sym = 0xff09;     /* Tab */
        else if (cp < 0x20 || cp == 0x7f || (cp >= 0x80 && cp < 0xa0)) continue;
        else if (cp < 0x100) sym = cp;         /* Latin-1 keysyms equal the code point */
        else sym = 0x1000000 | cp;             /* Unicode keysym */
        KeySym syms[2] = { sym, sym };
        XChangeKeyboardMapping(g_display, spare, 2, syms, 1);
        XSync(g_display, False);
        usleep(8000);                          /* let clients see the MappingNotify before the key */
        XTestFakeKeyEvent(g_display, spare, True, CurrentTime);
        XTestFakeKeyEvent(g_display, spare, False, CurrentTime);
        XSync(g_display, False);
        usleep(8000);
        typed++;
    }
    KeySym none[2] = { NoSymbol, NoSymbol };
    XChangeKeyboardMapping(g_display, spare, 2, none, 1);
    XSync(g_display, False);
    return typed;
}

int x11_key_event(int keycode, int is_press) {
    if (!g_display) return -1;
    if (is_press) track_injected_key(keycode, 1);
    XTestFakeKeyEvent(g_display, keycode, is_press ? True : False, CurrentTime);
    if (!is_press) track_injected_key(keycode, 0);
    XFlush(g_display);
    return 0;
}

/* Session boundaries must also recover XTEST state left by a previous process.
 * Only query/release on this authenticated display; never log key state. */
int x11_release_all(void) {
    if (!g_display) return -1;
    XSync(g_display, False);
    char keys[32];
    XQueryKeymap(g_display, keys);
    for (int code = 8; code < 256; code++) {
        if (((unsigned char)keys[code >> 3] | g_injected_down[code >> 3]) & (1u << (code & 7)))
            XTestFakeKeyEvent(g_display, code, False, CurrentTime);
    }
    /* Includes side buttons; wheel buttons are stateless and need no release. */
    unsigned char mapping[256];
    int buttons = XGetPointerMapping(g_display, mapping, sizeof(mapping));
    for (int button = 1; button <= buttons && button <= 9; button++) {
        if (button <= 3 || button >= 8) XTestFakeButtonEvent(g_display, button, False, CurrentTime);
    }
    if (g_restore_repeat) XAutoRepeatOn(g_display);
    memset(g_injected_down, 0, sizeof(g_injected_down));
    g_injected_count = g_restore_repeat = 0;
    XSync(g_display, False);
    return 0;
}

/* ---- Terminal-aware paste (Mac viewer Cmd+V arrives as Ctrl+V) ------------------------------
 * Linux terminals paste with Ctrl+Shift+V; Ctrl+V quotes the next key (a paste then shows as
 * "^[[200~..."). Reports whether the focused window is a terminal emulator. Uses its own
 * connection so it never races the capture/input connection. */
static Display *g_probe = NULL;

static int class_is_terminal(const char *value) {
    static const char *names[] = { "gnome-terminal", "gnome-terminal-server", "org.gnome.Console", "kgx", "konsole",
        "xfce4-terminal", "mate-terminal", "lxterminal", "qterminal", "terminator", "tilix", "xterm", "uxterm",
        "urxvt", "rxvt", "alacritty", "kitty", "wezterm", "wezterm-gui", "foot", "st-256color", "terminology",
        "guake", "tilda", "deepin-terminal", "ptyxis", "io.elementary.terminal", "cool-retro-term", NULL };
    if (!value) return 0;
    for (int i = 0; names[i]; i++) if (strcasecmp(value, names[i]) == 0) return 1;
    return 0;
}

/* Returns 1 when the focused window belongs to a terminal emulator, else 0. */
int x11_terminal_shortcut_state(void) {
    if (!g_probe && !(g_probe = XOpenDisplay(NULL))) return 0;
    Window focus = 0; int revert = 0;
    XGetInputFocus(g_probe, &focus, &revert);
    if (focus == None || focus == PointerRoot) return 0;
    Window cur = focus;
    for (int depth = 0; depth < 12 && cur; depth++) {
        XClassHint hint = {0};
        if (XGetClassHint(g_probe, cur, &hint)) {
            int terminal = class_is_terminal(hint.res_name) || class_is_terminal(hint.res_class);
            if (hint.res_name) XFree(hint.res_name);
            if (hint.res_class) XFree(hint.res_class);
            return terminal;
        }
        Window root, parent, *children = NULL; unsigned int n = 0;
        if (!XQueryTree(g_probe, cur, &root, &parent, &children, &n)) return 0;
        if (children) XFree(children);
        if (parent == root) return 0;
        cur = parent;
    }
    return 0;
}

void x11_close() {
    if (!g_display) return;
    pthread_mutex_lock(&g_capture_lock);
    shm_close_image();
    pthread_mutex_unlock(&g_capture_lock);
    XCloseDisplay(g_display);
    g_display = NULL;
}

// ============ Pointer shape monitor ============
// A dedicated connection: input injection shares g_display across threads,
// while this one is owned by the single monitor thread.
static Display *g_cursor_display = NULL;
static int g_cursor_event_base = 0;
static unsigned long g_cursor_serial = 0;

int x11_cursor_open(const char *display_name) {
    if (g_cursor_display) return 0;
    g_cursor_display = XOpenDisplay(display_name);
    if (!g_cursor_display) return -1;
    int error_base;
    if (!XFixesQueryExtension(g_cursor_display, &g_cursor_event_base, &error_base)) {
        XCloseDisplay(g_cursor_display);
        g_cursor_display = NULL;
        return -2;
    }
    XFixesSelectCursorInput(g_cursor_display, DefaultRootWindow(g_cursor_display),
                            XFixesDisplayCursorNotifyMask);
    XFlush(g_cursor_display);
    return 0;
}

/* Blocks until the X server has an event (e.g. a cursor change) or timeout_ms passes,
 * so a shape change is forwarded immediately instead of on the next polling tick. */
int x11_cursor_wait(int timeout_ms) {
    if (!g_cursor_display) return -1;
    if (XPending(g_cursor_display)) return 1;
    struct pollfd fd = { ConnectionNumber(g_cursor_display), POLLIN, 0 };
    return poll(&fd, 1, timeout_ms);
}

/* Copies the current cursor when it changed (or when force is set).
 * pixels receive premultiplied ARGB32. Returns 1 copied, 0 unchanged, <0 error. */
int x11_cursor_poll(uint32_t *pixels, int max_pixels, int *width, int *height,
                    int *hot_x, int *hot_y, int force) {
    if (!g_cursor_display) return -1;
    int changed = force;
    while (XPending(g_cursor_display)) {
        XEvent event;
        XNextEvent(g_cursor_display, &event);
        if (event.type == g_cursor_event_base + XFixesCursorNotify) changed = 1;
    }
    if (!changed) return 0;
    XFixesCursorImage *image = XFixesGetCursorImage(g_cursor_display);
    if (!image) return -2;
    int result = 0;
    if (force || image->cursor_serial != g_cursor_serial) {
        int count = image->width * image->height;
        if (image->width == 0 || image->height == 0 || count > max_pixels) {
            result = -3;
        } else {
            for (int i = 0; i < count; i++) pixels[i] = (uint32_t)image->pixels[i];
            *width = image->width;
            *height = image->height;
            *hot_x = image->xhot;
            *hot_y = image->yhot;
            g_cursor_serial = image->cursor_serial;
            result = 1;
        }
    }
    XFree(image);
    return result;
}
