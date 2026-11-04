// Viewer annotation (观看模式「批注」) overlay for X11 desktops.
//
// A full-screen, override-redirect ARGB window with an empty input shape: strokes from the
// viewer are drawn on it, the compositor blends it over the desktop (so the capture shows the
// strokes), and every pointer event passes through to the windows below. The window only
// exists while the viewer keeps annotation enabled. One thread owns each handle (Xlib is not
// initialized for threads here).
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <X11/Xutil.h>
#include <X11/extensions/shape.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    Display *dpy;
    Window win, root;
    Colormap cmap;
    GC gc;
    int x, y, width, height;
    int root_width, root_height;
} uu_annotation;

static int has_compositor(Display *dpy, int screen) {
    char name[32];
    snprintf(name, sizeof name, "_NET_WM_CM_S%d", screen);
    Atom selection = XInternAtom(dpy, name, False);
    return XGetSelectionOwner(dpy, selection) != None;
}

// Opens the overlay over the root region (x, y, width, height); width/height <= 0 = whole root.
// error: 1 display, 2 no compositing manager (an ARGB window would be opaque), 3 no 32-bit visual.
void *uu_annotation_open(const char *display, int x, int y, int width, int height, int *error) {
    *error = 0;
    Display *dpy = XOpenDisplay(display && *display ? display : NULL);
    if (!dpy) { *error = 1; return NULL; }
    int screen = DefaultScreen(dpy);
    Window root = RootWindow(dpy, screen);
    if (!has_compositor(dpy, screen)) { XCloseDisplay(dpy); *error = 2; return NULL; }
    XVisualInfo info;
    if (!XMatchVisualInfo(dpy, screen, 32, TrueColor, &info)) { XCloseDisplay(dpy); *error = 3; return NULL; }
    if (width <= 0 || height <= 0) {
        x = 0; y = 0;
        width = DisplayWidth(dpy, screen);
        height = DisplayHeight(dpy, screen);
    }
    Colormap cmap = XCreateColormap(dpy, root, info.visual, AllocNone);
    XSetWindowAttributes attrs;
    memset(&attrs, 0, sizeof attrs);
    attrs.override_redirect = True;
    attrs.background_pixel = 0;
    attrs.border_pixel = 0;
    attrs.colormap = cmap;
    attrs.event_mask = ExposureMask | StructureNotifyMask;
    Window win = XCreateWindow(dpy, root, x, y, (unsigned)width, (unsigned)height, 0, 32, InputOutput, info.visual,
        CWOverrideRedirect | CWBackPixel | CWBorderPixel | CWColormap | CWEventMask, &attrs);
    // Empty input region: clicks, scrolls and hovers reach the windows below.
    XShapeCombineRectangles(dpy, win, ShapeInput, 0, 0, NULL, 0, ShapeSet, Unsorted);
    XStoreName(dpy, win, "uu-remote-linux annotation");
    XClassHint hint = { "uu-remote-annotation", "uu-remote-linux" };
    XSetClassHint(dpy, win, &hint);
    // Keep the overlay composited (2 = never unredirect).
    long bypass = 2;
    XChangeProperty(dpy, win, XInternAtom(dpy, "_NET_WM_BYPASS_COMPOSITOR", False), XA_CARDINAL, 32,
        PropModeReplace, (unsigned char *)&bypass, 1);
    XMapRaised(dpy, win);
    // Root ConfigureNotify reports RandR resizes, so the caller can rebuild at the new geometry.
    XSelectInput(dpy, root, StructureNotifyMask);
    GC gc = XCreateGC(dpy, win, 0, NULL);
    XFlush(dpy);
    uu_annotation *a = calloc(1, sizeof *a);
    if (!a) { XFreeGC(dpy, gc); XDestroyWindow(dpy, win); XFreeColormap(dpy, cmap); XCloseDisplay(dpy); *error = 1; return NULL; }
    a->dpy = dpy; a->win = win; a->root = root; a->cmap = cmap; a->gc = gc;
    a->x = x; a->y = y; a->width = width; a->height = height;
    a->root_width = DisplayWidth(dpy, screen); a->root_height = DisplayHeight(dpy, screen);
    return a;
}

void uu_annotation_size(void *handle, int *width, int *height) {
    uu_annotation *a = handle;
    *width = a->width; *height = a->height;
}

// argb: premultiplied 0xAARRGGBB for the 32-bit visual. points: x0,y0,x1,y1,... in window pixels.
void uu_annotation_polyline(void *handle, const short *points, int count, unsigned int argb, int line_width) {
    uu_annotation *a = handle;
    if (count <= 0) return;
    XSetForeground(a->dpy, a->gc, argb);
    XSetLineAttributes(a->dpy, a->gc, line_width < 1 ? 1 : (unsigned)line_width, LineSolid, CapRound, JoinRound);
    if (count == 1) {
        int r = line_width / 2;
        XFillArc(a->dpy, a->win, a->gc, points[0] - r, points[1] - r, (unsigned)line_width, (unsigned)line_width, 0, 360 * 64);
    } else {
        XDrawLines(a->dpy, a->win, a->gc, (XPoint *)points, count, CoordModeOrigin);
    }
}

void uu_annotation_clear(void *handle) {
    uu_annotation *a = handle;
    XClearWindow(a->dpy, a->win);
}

void uu_annotation_flush(void *handle) {
    uu_annotation *a = handle;
    XRaiseWindow(a->dpy, a->win);
    XFlush(a->dpy);
}

// Drains pending events: 2 when the root was resized (reopen the overlay), 1 when the window needs
// a full repaint (exposed or remapped), else 0.
int uu_annotation_poll(void *handle) {
    uu_annotation *a = handle;
    int result = 0;
    while (XPending(a->dpy)) {
        XEvent event;
        XNextEvent(a->dpy, &event);
        if (event.type == ConfigureNotify && event.xconfigure.window == a->root
            && (event.xconfigure.width != a->root_width || event.xconfigure.height != a->root_height)) {
            a->root_width = event.xconfigure.width; a->root_height = event.xconfigure.height;
            result = 2;
        } else if (result == 0 && ((event.type == Expose && event.xexpose.count == 0) || event.type == MapNotify)) {
            result = 1;
        }
    }
    return result;
}

void uu_annotation_close(void *handle) {
    uu_annotation *a = handle;
    if (!a) return;
    XFreeGC(a->dpy, a->gc);
    XDestroyWindow(a->dpy, a->win);
    XFreeColormap(a->dpy, a->cmap);
    XCloseDisplay(a->dpy);
    free(a);
}
