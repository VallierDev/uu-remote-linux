/* On-demand binary selection read. The unmapped requestor never takes focus or
 * creates a taskbar entry. This connection belongs exclusively to the worker. */
#include <X11/Xlib.h>
#include <stdlib.h>
#include <string.h>
#include <poll.h>
#include <time.h>
#include <errno.h>

static long long milliseconds(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (long long)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static int next_event(Display *d, XEvent *e, long long deadline) {
    while (!XPending(d)) {
        long long remaining = deadline - milliseconds();
        if (remaining <= 0) return 0;
        struct pollfd fd = { ConnectionNumber(d), POLLIN, 0 };
        int result = poll(&fd, 1, (int)remaining);
        if (result < 0 && errno == EINTR) continue;
        if (result <= 0 || (fd.revents & (POLLERR | POLLHUP | POLLNVAL))) return 0;
    }
    XNextEvent(d, e); return 1;
}
/* 1: PNG bytes (caller frees with free), 0: no PNG selection, -1: failure.
 * Both normal and ICCCM INCR transfers obey the same total byte/time budget. */
int uu_viewer_clipboard_png(unsigned char **out, size_t *length, size_t limit) {
    *out = NULL; *length = 0;
    Display *d = XOpenDisplay(NULL);
    if (!d) return -1;
    Window w = XCreateSimpleWindow(d, DefaultRootWindow(d), 0, 0, 1, 1, 0, 0, 0);
    Atom selection = XInternAtom(d, "CLIPBOARD", False);
    Atom target = XInternAtom(d, "image/png", False);
    Atom property = XInternAtom(d, "UU_VIEWER_PNG", False);
    Atom incr = XInternAtom(d, "INCR", False);
    int result = -1, incremental = 0, started = 0;
    size_t used = 0; unsigned char *buffer = NULL;
    long long deadline = milliseconds() + 5000;
    if (XGetSelectionOwner(d, selection) == None) { result = 0; goto done; }
    XSelectInput(d, w, PropertyChangeMask);
    XConvertSelection(d, selection, target, property, w, CurrentTime); XFlush(d);
    XEvent event;
    while (next_event(d, &event, deadline)) {
        if (!started) {
            if (event.type != SelectionNotify || event.xselection.requestor != w ||
                event.xselection.selection != selection || event.xselection.target != target) continue;
            if (event.xselection.property == None) { result = 0; break; }
            if (event.xselection.property != property) break;
            started = 1;
        } else if (!incremental || event.type != PropertyNotify || event.xproperty.window != w ||
                   event.xproperty.atom != property || event.xproperty.state != PropertyNewValue) continue;
        Atom type; int format; unsigned long size, remaining; unsigned char *data = NULL;
        int status = XGetWindowProperty(d, w, property, 0, (long)(limit / 4 + 1), True,
                                        AnyPropertyType, &type, &format, &size, &remaining, &data);
        if (status != Success || remaining) { if (data) XFree(data); break; }
        if (!incremental && type == incr) {
            int valid = format == 32 && size == 1 && ((unsigned long *)data)[0] <= limit;
            if (data) XFree(data);
            if (!valid) break;
            incremental = 1; XFlush(d); continue;
        }
        if (type != target || format != 8 || size > limit - used) { if (data) XFree(data); break; }
        if (size) {
            unsigned char *grown = realloc(buffer, used + size);
            if (!grown) { XFree(data); break; }
            buffer = grown; memcpy(buffer + used, data, size); used += size;
        }
        if (data) XFree(data);
        XFlush(d);
        if (!incremental || size == 0) { *out = buffer; *length = used; buffer = NULL; result = 1; break; }
    }
 done:
    free(buffer); XDestroyWindow(d, w); XCloseDisplay(d); return result;
}
