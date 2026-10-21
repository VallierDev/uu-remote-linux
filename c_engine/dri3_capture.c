#include "dri3_capture.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <X11/Xlib.h>
#include <X11/Xlib-xcb.h>
#include <xcb/xcb.h>
#include <xcb/dri3.h>
#include <xcb/damage.h>
#include <gbm.h>

struct Dri3Capture {
    Display *dpy;
    xcb_connection_t *conn;
    xcb_screen_t *screen;
    uint32_t width;
    uint32_t height;

    int drm_fd;
    struct gbm_device *gbm;
    struct gbm_bo *bo;
    int bo_fd;
    uint32_t stride;

    xcb_pixmap_t pix;
    xcb_gcontext_t gc;
    xcb_damage_damage_t damage;
    uint8_t damage_event_code;
};

Dri3Capture* dri3_capture_create(const char* display_name, uint32_t width, uint32_t height) {
    Display *dpy = XOpenDisplay(display_name);
    if (!dpy) {
        fprintf(stderr, "[dri3_capture] failed to open display %s\n", display_name ? display_name : "default");
        return NULL;
    }

    // Events are read with xcb_poll_for_event below, so XCB, not Xlib, must own the queue
    XSetEventQueueOwner(dpy, XCBOwnsEventQueue);
    xcb_connection_t *conn = XGetXCBConnection(dpy);
    const xcb_setup_t *setup = xcb_get_setup(conn);
    xcb_screen_t *screen = xcb_setup_roots_iterator(setup).data;
    if (!screen) {
        XCloseDisplay(dpy);
        return NULL;
    }

    // Check the extension first: a DRI3 request on a server without it breaks the connection,
    // and the Xlib cleanup below then exits the process.
    const xcb_query_extension_reply_t *dri3_ext = xcb_get_extension_data(conn, &xcb_dri3_id);
    if (!dri3_ext || !dri3_ext->present) {
        fprintf(stderr, "[dri3_capture] DRI3 extension not supported on display\n");
        XCloseDisplay(dpy);
        return NULL;
    }
    // Query DRI3
    xcb_dri3_query_version_cookie_t qvc = xcb_dri3_query_version(conn, 1, 2);
    xcb_dri3_query_version_reply_t *qvr = xcb_dri3_query_version_reply(conn, qvc, NULL);
    if (!qvr) {
        fprintf(stderr, "[dri3_capture] DRI3 extension not supported on display\n");
        XCloseDisplay(dpy);
        return NULL;
    }
    free(qvr);

    // Open DRM device via DRI3
    xcb_dri3_open_cookie_t oc = xcb_dri3_open(conn, screen->root, 0);
    xcb_dri3_open_reply_t *or = xcb_dri3_open_reply(conn, oc, NULL);
    if (!or) {
        fprintf(stderr, "[dri3_capture] xcb_dri3_open failed\n");
        XCloseDisplay(dpy);
        return NULL;
    }
    int drm_fd = xcb_dri3_open_reply_fds(conn, or)[0];
    free(or);

    uint32_t w = width > 0 ? width : screen->width_in_pixels;
    uint32_t h = height > 0 ? height : screen->height_in_pixels;

    struct gbm_device *gbm = gbm_create_device(drm_fd);
    if (!gbm) {
        fprintf(stderr, "[dri3_capture] gbm_create_device failed\n");
        close(drm_fd);
        XCloseDisplay(dpy);
        return NULL;
    }

    struct gbm_bo *bo = gbm_bo_create(gbm, w, h, GBM_FORMAT_XRGB8888, GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR);
    if (!bo) {
        fprintf(stderr, "[dri3_capture] gbm_bo_create failed\n");
        gbm_device_destroy(gbm);
        close(drm_fd);
        XCloseDisplay(dpy);
        return NULL;
    }

    int bo_fd = gbm_bo_get_fd(bo);
    uint32_t stride = gbm_bo_get_stride(bo);

    xcb_pixmap_t pix = xcb_generate_id(conn);
    xcb_generic_error_t *err = NULL;
    // XCB takes ownership of the fd it sends and closes it after transmission; only a
    // duplicate goes to X11, bo_fd stays ours for the VAAPI import and teardown.
    int send_fd = fcntl(bo_fd, F_DUPFD_CLOEXEC, 3);
    if (send_fd < 0) {
        close(bo_fd);
        gbm_bo_destroy(bo);
        gbm_device_destroy(gbm);
        close(drm_fd);
        XCloseDisplay(dpy);
        return NULL;
    }
    xcb_void_cookie_t pc = xcb_dri3_pixmap_from_buffer_checked(
        conn, pix, screen->root, stride * h, w, h, stride, screen->root_depth, 32, send_fd
    );
    err = xcb_request_check(conn, pc);
    if (err) {
        fprintf(stderr, "[dri3_capture] xcb_dri3_pixmap_from_buffer failed: %d\n", err->error_code);
        free(err);
        close(bo_fd);
        gbm_bo_destroy(bo);
        gbm_device_destroy(gbm);
        close(drm_fd);
        XCloseDisplay(dpy);
        return NULL;
    }

    xcb_gcontext_t gc = xcb_generate_id(conn);
    // The default ClipByChildren copies only the root background (solid black); XShmGetImage on the root
    // includes child windows, so the blit must too.
    uint32_t gc_mode = XCB_SUBWINDOW_MODE_INCLUDE_INFERIORS;
    xcb_create_gc(conn, gc, pix, XCB_GC_SUBWINDOW_MODE, &gc_mode);

    // Setup XDamage on root window
    const xcb_query_extension_reply_t *ext = xcb_get_extension_data(conn, &xcb_damage_id);
    uint8_t damage_event_code = 0;
    xcb_damage_damage_t damage = 0;
    if (ext && ext->present) {
        damage_event_code = ext->first_event;
        xcb_damage_query_version(conn, 1, 1);
        damage = xcb_generate_id(conn);
        xcb_damage_create(conn, damage, screen->root, XCB_DAMAGE_REPORT_LEVEL_NON_EMPTY);
    }

    Dri3Capture* cap = (Dri3Capture*)calloc(1, sizeof(Dri3Capture));
    cap->dpy = dpy;
    cap->conn = conn;
    cap->screen = screen;
    cap->width = w;
    cap->height = h;
    cap->drm_fd = drm_fd;
    cap->gbm = gbm;
    cap->bo = bo;
    cap->bo_fd = bo_fd;
    cap->stride = stride;
    cap->pix = pix;
    cap->gc = gc;
    cap->damage = damage;
    cap->damage_event_code = damage_event_code;

    fprintf(stderr, "[dri3_capture] Initialized: %ux%u (stride: %u, dma_fd: %d) Pure GPU Blit ready!\n",
            w, h, stride, bo_fd);
    return cap;
}

int dri3_capture_blit(Dri3Capture* cap) {
    if (!cap) return -1;
    xcb_copy_area(cap->conn, cap->screen->root, cap->pix, cap->gc, 0, 0, 0, 0, cap->width, cap->height);
    // Round trip: the server has executed (and flushed to the GPU) the copy before VAAPI reads the BO;
    // xcb_flush alone only sends the request.
    xcb_get_input_focus_reply_t *r = xcb_get_input_focus_reply(cap->conn, xcb_get_input_focus(cap->conn), NULL);
    if (!r) return -1;
    free(r);
    return 0;
}

int dri3_capture_wait_damage(Dri3Capture* cap, int timeout_ms) {
    if (!cap) return -1;

    // Check queued events
    xcb_generic_event_t *ev = NULL;
    int damaged = 0;
    while ((ev = xcb_poll_for_event(cap->conn)) != NULL) {
        if ((ev->response_type & 0x7f) == (cap->damage_event_code + XCB_DAMAGE_NOTIFY)) {
            damaged = 1;
        }
        free(ev);
    }
    if (damaged) {
        xcb_damage_subtract(cap->conn, cap->damage, XCB_NONE, XCB_NONE);
        xcb_flush(cap->conn);
        return 1;
    }

    int fd = xcb_get_file_descriptor(cap->conn);
    struct pollfd pfd = { .fd = fd, .events = POLLIN, .revents = 0 };
    int ret = poll(&pfd, 1, timeout_ms);
    if (ret > 0 && (pfd.revents & POLLIN)) {
        // Readable but not yet a whole event: polling again would return at once and spin a core
        ev = xcb_poll_for_event(cap->conn);
        if (!ev) ev = xcb_wait_for_event(cap->conn);
        if (!ev) return -1;
        do {
            if ((ev->response_type & 0x7f) == (cap->damage_event_code + XCB_DAMAGE_NOTIFY)) {
                damaged = 1;
            }
            free(ev);
        } while ((ev = xcb_poll_for_event(cap->conn)) != NULL);
        if (damaged) {
            xcb_damage_subtract(cap->conn, cap->damage, XCB_NONE, XCB_NONE);
            xcb_flush(cap->conn);
            return 1;
        }
    }
    return ret < 0 ? -1 : 0;
}

int dri3_capture_get_dma_fd(Dri3Capture* cap) {
    return cap ? cap->bo_fd : -1;
}

uint32_t dri3_capture_get_stride(Dri3Capture* cap) {
    return cap ? cap->stride : 0;
}

void dri3_capture_destroy(Dri3Capture* cap) {
    if (!cap) return;

    if (cap->damage) xcb_damage_destroy(cap->conn, cap->damage);
    if (cap->gc) xcb_free_gc(cap->conn, cap->gc);
    if (cap->pix) xcb_free_pixmap(cap->conn, cap->pix);
    if (cap->bo_fd >= 0) close(cap->bo_fd);
    if (cap->bo) gbm_bo_destroy(cap->bo);
    if (cap->gbm) gbm_device_destroy(cap->gbm);
    if (cap->drm_fd >= 0) close(cap->drm_fd);
    if (cap->dpy) XCloseDisplay(cap->dpy);

    free(cap);
}
