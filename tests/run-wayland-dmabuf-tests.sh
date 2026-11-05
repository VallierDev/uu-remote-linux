#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT HUP INT TERM
for protocol in wlr-screencopy-unstable-v1 wlr-virtual-pointer-unstable-v1 virtual-keyboard-unstable-v1 wlr-output-power-management-unstable-v1 linux-dmabuf-unstable-v1; do
    wayland-scanner client-header "$root/c_engine/wayland-protocols/$protocol.xml" "$build/$protocol-client-protocol.h"
    wayland-scanner private-code "$root/c_engine/wayland-protocols/$protocol.xml" "$build/$protocol-protocol.c"
done
# Sanitizers are on by default, as for the other native engine regressions.
${CC:-cc} -O1 -g ${CFLAGS:--fsanitize=address,undefined -fno-omit-frame-pointer} \
    -Wall -Wextra -Wno-unused-parameter -I"$build" "$root/tests/wayland_dmabuf.c" \
    "$build/"*-protocol.c $(pkg-config --cflags --libs wayland-client xkbcommon) \
    -lpthread -ldl -o "$build/wayland-dmabuf"
"$build/wayland-dmabuf"
