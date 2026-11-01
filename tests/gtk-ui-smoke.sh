#!/bin/sh
# Requires a C compiler, GTK3/X11 development headers, Xvfb and xauth.
# Run: xvfb-run -a -s '-screen 0 1600x1200x24' sh tests/gtk-ui-smoke.sh /tmp/gtk-ui-previews
# Optional output directory retains synthetic main/settings screenshots; no account or QR is used.
# Checks 5 window types in Chinese/English with System/Light/Dark and settings persistence.
set -eu
cd "$(dirname "$0")/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
out=${1:-"$work/previews"}
mkdir -p "$out" "$work/config/gtk-3.0" "$work/config/uuremote" "$work/home/Documents"
export HOME="$work/home"
export XDG_CONFIG_HOME="$work/config"
export GSETTINGS_BACKEND=memory
export G_DEBUG=fatal-warnings
export GDK_BACKEND=x11
export GDK_SCALE=1
export GDK_DPI_SCALE=1
export NO_AT_BRIDGE=1
unset GTK_THEME
printf '[Settings]\ngtk-theme-name=Adwaita\ngtk-application-prefer-dark-theme=false\n' > "$XDG_CONFIG_HOME/gtk-3.0/settings.ini"
for ui in HOST VIEWER TOOLS; do
    # pkg-config output is intentionally split into compiler arguments.
    cc -std=gnu11 -Wall -Wextra -Werror -Wno-deprecated-declarations \
        -D"UI_$ui" tests/gtk_ui_smoke.c c_engine/ui_theme.c \
        $(pkg-config --cflags --libs gtk+-3.0) -ldl -lpthread -o "$work/$ui"
    for lang in en zh; do
        for theme in light dark system; do
            case $theme in light) mode=1;; dark) mode=2;; system) mode=0;; esac
            for kind in $(if [ "$ui" = TOOLS ]; then printf 'files portmap terminal'; else printf 'main'; fi); do
                printf '%s' "$theme" > "$XDG_CONFIG_HOME/uuremote/theme"
                UU_LANG=$lang "$work/$ui" "$out/$ui-$kind-$lang-$theme.png" "$mode" "$kind"
            done
        done
    done
done
printf 'GTK UI smoke checks passed (host, viewer, files, port mapping, terminal; English/Chinese; light/dark/system).\n'
