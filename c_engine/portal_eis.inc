/* All access is serialized by PortalSession's handle mutex, including close.
 * ConnectToEIS disables every legacy Notify* input method on this session. */
enum { PORTAL_EIS_UNSUPPORTED = 13 };
static void portal_eis_log(struct ei *ei, enum ei_log_priority priority,
    const char *message, struct ei_log_context *context) {
    /* Library error text can include input values. Keep diagnostics numeric. */
    static atomic_uint count;
    if (atomic_fetch_add(&count, 1) < 12)
        fprintf(stderr, "Portal EIS library diagnostic: priority=%d\n", priority);
}
static int portal_eis_dispatch(struct uu_portal *p) {
    ei_dispatch(p->ei);
    struct ei_event *event;
    while ((event = ei_get_event(p->ei))) {
        struct ei_device *device = ei_event_get_device(event);
        switch (ei_event_get_type(event)) {
        case EI_EVENT_SEAT_ADDED:
            ei_seat_bind_capabilities(ei_event_get_seat(event),
                EI_DEVICE_CAP_POINTER, EI_DEVICE_CAP_POINTER_ABSOLUTE,
                EI_DEVICE_CAP_BUTTON, EI_DEVICE_CAP_SCROLL, EI_DEVICE_CAP_KEYBOARD, EI_DEVICE_CAP_TOUCH, NULL);
            break;
        case EI_EVENT_DEVICE_RESUMED:
            if (!g_list_find(p->ei_devices, device)) {
                p->ei_devices = g_list_append(p->ei_devices, ei_device_ref(device));
                ei_device_start_emulating(device, ++p->ei_sequence);
            }
            break;
        case EI_EVENT_DEVICE_PAUSED:
        case EI_EVENT_DEVICE_REMOVED:
            if (g_list_find(p->ei_devices, device)) {
                p->ei_devices = g_list_remove(p->ei_devices, device);
                ei_device_unref(device);
            }
            break;
        case EI_EVENT_DISCONNECT:
            p->ei_failed = 1;
            break;
        default: break;
        }
        ei_event_unref(event);
    }
    return p->ei_failed ? 9 : 0;
}
static struct ei_device *portal_eis_device(struct uu_portal *p, enum ei_device_capability capability) {
    for (GList *item = p->ei_devices; item; item = item->next)
        if (ei_device_has_capability(item->data, capability)) return item->data;
    return NULL;
}
static struct ei_region *portal_eis_region(struct uu_portal *p, struct ei_device *device) {
    for (size_t i = 0; ; i++) {
        struct ei_region *region = ei_device_get_region(device, i);
        if (!region) return NULL;
        const char *id = ei_region_get_mapping_id(region);
        if (id && p->mapping_id && !strcmp(id, p->mapping_id) &&
            ei_region_get_width(region) && ei_region_get_height(region)) return region;
    }
}
static struct ei_device *portal_eis_mapped(struct uu_portal *p, enum ei_device_capability capability, struct ei_region **region) {
    for (GList *item = p->ei_devices; item; item = item->next) {
        struct ei_device *device = item->data;
        if (ei_device_has_capability(device, capability) &&
            (*region = portal_eis_region(p, device))) return device;
    }
    return NULL;
}
static struct ei_device *portal_eis_absolute(struct uu_portal *p, struct ei_region **region) {
    return portal_eis_mapped(p, EI_DEVICE_CAP_POINTER_ABSOLUTE, region);
}
static void portal_eis_close(struct uu_portal *p) {
    if (p->ei_touch) p->ei_touch = ei_touch_unref(p->ei_touch);
    for (GList *item = p->ei_devices; item; item = item->next) ei_device_unref(item->data);
    g_list_free(p->ei_devices); p->ei_devices = NULL;
    if (p->ei) p->ei = ei_unref(p->ei);
}
static int portal_eis_open(struct uu_portal *p) {
    if (!p->mapping_id) return PORTAL_EIS_UNSUPPORTED;
    GUnixFDList *fds = NULL;
    GError *error = NULL;
    GVariant *reply = g_dbus_connection_call_with_unix_fd_list_sync(p->bus, DEST, PATH, REMOTE,
        "ConnectToEIS", g_variant_new("(o@a{sv})", p->session, empty_options()),
        G_VARIANT_TYPE("(h)"), G_DBUS_CALL_FLAGS_NONE, 3000, NULL, &fds, NULL, &error);
    if (!reply) {
        int unsupported = g_error_matches(error, G_DBUS_ERROR, G_DBUS_ERROR_UNKNOWN_METHOD) ||
            g_error_matches(error, G_DBUS_ERROR, G_DBUS_ERROR_NOT_SUPPORTED);
        g_clear_error(&error); g_clear_object(&fds);
        return unsupported ? PORTAL_EIS_UNSUPPORTED : 9;
    }
    gint32 index; g_variant_get(reply, "(h)", &index);
    int fd = fds && index >= 0 && index < g_unix_fd_list_get_length(fds) ?
        g_unix_fd_list_get(fds, index, NULL) : -1;
    g_variant_unref(reply); g_clear_object(&fds);
    if (fd < 0) return 9;
    p->ei = ei_new_sender(NULL);
    if (!p->ei) { close(fd); return 8; }
    ei_log_set_handler(p->ei, portal_eis_log);
    ei_configure_name(p->ei, "uu-remote-linux");
    if (ei_setup_backend_fd(p->ei, fd)) return 9;
    gint64 deadline = g_get_monotonic_time() + 3000000;
    while (!portal_eis_dispatch(p)) {
        struct ei_region *region = NULL;
        if (portal_eis_absolute(p, &region) &&
            portal_eis_device(p, EI_DEVICE_CAP_POINTER) &&
            portal_eis_device(p, EI_DEVICE_CAP_BUTTON) &&
            portal_eis_device(p, EI_DEVICE_CAP_SCROLL) &&
            portal_eis_device(p, EI_DEVICE_CAP_KEYBOARD)) {
            fprintf(stderr, "Portal EIS input ready: region=%ux%u\n",
                ei_region_get_width(region), ei_region_get_height(region));
            return 0;
        }
        gint64 remaining = deadline - g_get_monotonic_time();
        if (remaining <= 0) break;
        struct pollfd pending = {.fd = ei_get_fd(p->ei), .events = POLLIN};
        int result = poll(&pending, 1, (int)((remaining + 999) / 1000));
        if (result < 0 && errno != EINTR) break;
    }
    fprintf(stderr, "Portal EIS input initialization failed\n");
    return 9;
}
static int portal_eis_available(struct uu_portal *p) {
    if (!uu_portal_alive(p)) return 9;
    return portal_eis_dispatch(p);
}
static int portal_eis_frame(struct uu_portal *p, struct ei_device *device) {
    ei_device_frame(device, ei_now(p->ei));
    return portal_eis_dispatch(p);
}
static int portal_eis_move(struct uu_portal *p, double x, double y, int relative) {
    if (portal_eis_available(p)) return 9;
    struct ei_device *device;
    if (relative) {
        device = portal_eis_device(p, EI_DEVICE_CAP_POINTER);
        if (!device) return 9;
        ei_device_pointer_motion(device, x * p->logical_width / p->width,
            y * p->logical_height / p->height);
    } else {
        if (x < 0 || y < 0 || x >= p->width || y >= p->height) return 11;
        struct ei_region *region = NULL;
        device = portal_eis_absolute(p, &region);
        if (!device) return 9;
        double target_x = ei_region_get_x(region) + x * ei_region_get_width(region) / p->width;
        double target_y = ei_region_get_y(region) + y * ei_region_get_height(region) / p->height;
        ei_device_pointer_motion_absolute(device, target_x, target_y);
    }
    return portal_eis_frame(p, device);
}
static int portal_eis_button(struct uu_portal *p, int button, int pressed) {
    if (portal_eis_available(p)) return 9;
    if (button >= 4 && button <= 7) {
        if (!pressed) return 0;
        struct ei_device *device = portal_eis_device(p, EI_DEVICE_CAP_SCROLL);
        if (!device) return 9;
        int step = button == 4 || button == 6 ? -120 : 120;
        ei_device_scroll_discrete(device, button <= 5 ? 0 : step, button <= 5 ? step : 0);
        return portal_eis_frame(p, device);
    }
    int code = button == 1 ? 0x110 : button == 2 ? 0x112 : button == 3 ? 0x111 :
        button == 8 ? 0x113 : button == 9 ? 0x114 : 0;
    if (!code) return 11;
    struct ei_device *device = portal_eis_device(p, EI_DEVICE_CAP_BUTTON);
    if (!device) return 9;
    ei_device_button_button(device, code, !!pressed);
    return portal_eis_frame(p, device);
}
static int portal_eis_scroll(struct uu_portal *p, double dx, double dy, int finish) {
    if (portal_eis_available(p)) return 9;
    struct ei_device *device = portal_eis_device(p, EI_DEVICE_CAP_SCROLL);
    if (!device) return 9;
    if (dx != 0 || dy != 0) ei_device_scroll_delta(device, dx, dy);
    if (finish) ei_device_scroll_stop(device, true, true);
    return portal_eis_frame(p, device);
}
static int portal_eis_touch(struct uu_portal *p, int phase, double x, double y) {
    if (portal_eis_available(p)) return 9;
    if (phase == 2) {
        if (!p->ei_touch) return 0;
        struct ei_device *device = ei_touch_get_device(p->ei_touch);
        ei_touch_up(p->ei_touch);
        p->ei_touch = ei_touch_unref(p->ei_touch);
        return portal_eis_frame(p, device);
    }
    struct ei_region *region = NULL;
    struct ei_device *device = portal_eis_mapped(p, EI_DEVICE_CAP_TOUCH, &region);
    if (!device) return 9;
    double target_x = ei_region_get_x(region) + x * ei_region_get_width(region) / p->width;
    double target_y = ei_region_get_y(region) + y * ei_region_get_height(region) / p->height;
    if (phase == 0) {
        if (p->ei_touch) { ei_touch_up(p->ei_touch); p->ei_touch = ei_touch_unref(p->ei_touch); }
        if (!(p->ei_touch = ei_device_touch_new(device))) return 9;
        ei_touch_down(p->ei_touch, target_x, target_y);
    } else {
        if (!p->ei_touch) return 11;
        ei_touch_motion(p->ei_touch, target_x, target_y);
    }
    return portal_eis_frame(p, device);
}
static int portal_eis_key(struct uu_portal *p, int code, int pressed) {
    if (code < 0 || code > 0x2ff) return 11;
    if (portal_eis_available(p)) return 9;
    struct ei_device *device = portal_eis_device(p, EI_DEVICE_CAP_KEYBOARD);
    if (!device) return 9;
    ei_device_keyboard_key(device, (uint32_t)code, !!pressed);
    return portal_eis_frame(p, device);
}
