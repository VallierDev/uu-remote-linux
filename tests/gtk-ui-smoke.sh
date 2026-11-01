#!/bin/sh
# Run under xvfb-run -a -s "-screen 0 1600x1200x24". Optional first argument retains synthetic previews in that directory.
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
