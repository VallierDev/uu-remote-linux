/* Repaints a small window every ~16 ms so XDamage reports changes (benchmark desktop load). */
#include <X11/Xlib.h>
#include <unistd.h>
int main(void) {
    Display *d = XOpenDisplay(NULL);
    if (!d) return 1;
    int s = DefaultScreen(d);
    Window w = XCreateSimpleWindow(d, RootWindow(d, s), 0, 0, 200, 50, 0, 0, 0);
    XMapWindow(d, w);
    XSync(d, False);
    GC gc = XCreateGC(d, w, 0, NULL);
    unsigned long c = 0;
    for (;;) {
        XSetForeground(d, gc, c++ & 0xffffff);
        XFillRectangle(d, w, gc, 0, 0, 200, 50);
        XFlush(d);
        usleep(16000);
    }
}
