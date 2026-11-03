/* Run only under Xvfb: cc viewer_clipboard{,_test}.c -lX11; xvfb-run ./a.out */
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <assert.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <time.h>
#include <stdio.h>
int uu_viewer_clipboard_png(unsigned char **out, size_t *length, size_t limit);
static unsigned char pattern(size_t i) { return (unsigned char)(i * 17 + i / 251); }
static void check(int mode) {
    Display *d = XOpenDisplay(NULL); assert(d);
    Window w = XCreateSimpleWindow(d, DefaultRootWindow(d), 0, 0, 1, 1, 0, 0, 0);
    Atom clip = XInternAtom(d, "CLIPBOARD", False), png = XInternAtom(d, "image/png", False);
    Atom incr = XInternAtom(d, "INCR", False);
    size_t total = mode == 1 ? 700000 : 68, limit = 1000000;
    unsigned char *fixture = malloc(total); assert(fixture);
    for (size_t i = 0; i < total; ++i) fixture[i] = pattern(i);
    XSetSelectionOwner(d, clip, w, CurrentTime); XSync(d, False);
    pid_t child = fork(); assert(child >= 0);
    if (!child) {
        unsigned char *data = NULL; size_t size = 0;
        int status = uu_viewer_clipboard_png(&data, &size, limit);
        assert(status == (mode == 3 ? 0 : mode == 2 ? -1 : 1));
        if (status == 1) {
            assert(size == total);
            for (size_t i = 0; i < size; ++i) assert(data[i] == pattern(i));
        }
        free(data); _exit(0);
    }
    Window requestor = None; Atom property = None; size_t offset = 0; int ended = 0, status;
    time_t deadline = time(NULL) + 8;
    while (!waitpid(child, &status, WNOHANG)) {
        assert(time(NULL) < deadline);
        while (XPending(d)) {
            XEvent e; XNextEvent(d, &e);
            if (e.type == SelectionRequest) {
                XSelectionRequestEvent *r = &e.xselectionrequest;
                XEvent reply = {0}; reply.xselection.type = SelectionNotify;
                reply.xselection.display = d; reply.xselection.requestor = r->requestor;
                reply.xselection.selection = r->selection; reply.xselection.target = r->target;
                reply.xselection.time = r->time; reply.xselection.property = mode == 3 ? None : r->property;
                if (mode == 0) XChangeProperty(d, r->requestor, r->property, png, 8, PropModeReplace, fixture, total);
                if (mode == 1 || mode == 2) {
                    requestor = r->requestor; property = r->property;
                    unsigned long length = mode == 2 ? limit + 1 : total;
                    XSelectInput(d, requestor, PropertyChangeMask);
                    XChangeProperty(d, requestor, property, incr, 32, PropModeReplace, (unsigned char *)&length, 1);
                }
                XSendEvent(d, r->requestor, False, 0, &reply); XFlush(d);
            } else if (mode == 1 && !ended && e.type == PropertyNotify && e.xproperty.window == requestor &&
                       e.xproperty.atom == property && e.xproperty.state == PropertyDelete) {
                size_t n = total - offset; if (n > 32768) n = 32768;
                XChangeProperty(d, requestor, property, png, 8, PropModeReplace, fixture + offset, n); XFlush(d);
                offset += n; if (!n) ended = 1;
            }
        }
        usleep(1000);
    }
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    free(fixture); XDestroyWindow(d, w); XCloseDisplay(d);
}
int main(void) {
    for (int mode = 0; mode < 4; ++mode) check(mode);
    puts("X11 PNG: normal, INCR, oversized and absent selection passed"); return 0;
}
