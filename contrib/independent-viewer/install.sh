#!/bin/sh
set -eu
if [ "$(uname -s)" != Linux ]; then
    echo '此辅助工具用于 Linux 控制端 / This companion runs on the Linux viewer.' >&2
    exit 1
fi
command -v python3 >/dev/null
command -v systemd-run >/dev/null
source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
viewer=''
if [ "$#" -ne 0 ]; then
    if [ "$#" -ne 2 ] || [ "$1" != --viewer ] || [ ! -x "$2" ]; then
        echo 'Usage: install.sh [--viewer PATH_TO_BUILT_VIEWER]' >&2
        exit 1
    fi
    viewer=$(readlink -f -- "$2")
fi
bin_dir="$HOME/.local/bin"
lib_dir="$HOME/.local/lib/uuremote-independent"
mkdir -p "$bin_dir" "$lib_dir"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
compiler=${CC:-cc}
if command -v "$compiler" >/dev/null; then
    "$compiler" -shared -fPIC -o "$scratch/input.so" "$source_dir/sdl-input-compat.c" -ldl
    install -m 755 "$scratch/input.so" "$lib_dir/.input-new.so"
    mv -f "$lib_dir/.input-new.so" "$lib_dir/libuuremote-backspace-repeat.so"
else
    echo '未找到 C 编译器；独立会话可用，暂不启用按键适配 / No C compiler; independent sessions work without the input adapter.' >&2
fi
for helper in uuremote-independent uuremote-image-clipboard; do
    install -m 755 "$source_dir/$helper" "$bin_dir/.$helper-new"
    mv -f "$bin_dir/.$helper-new" "$bin_dir/$helper"
done
if [ -n "$viewer" ]; then
    viewer_dir="$HOME/.local/lib/uuremote-independent"
    mkdir -p "$viewer_dir"
    install -m 755 "$viewer" "$viewer_dir/.viewer-new"
    mv -f "$viewer_dir/.viewer-new" "$viewer_dir/uuremote-viewer"
    ln -sfn "$viewer_dir/uuremote-viewer" "$bin_dir/uuremote-viewer"
    server=$(command -v uuremote-server || true)
    if [ -x /usr/bin/uuremote-server ]; then server=/usr/bin/uuremote-server; fi
    if [ -n "$server" ]; then ln -sfn "$server" "$viewer_dir/uuremote-server"; fi
    python3 - "$viewer_dir/uuremote-viewer" <<'PY'
import os
import shlex
import sys
from pathlib import Path

directory = Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'applications'
directory.mkdir(parents=True, exist_ok=True)
legacy = directory / 'uu-remote-linux-viewer.desktop'
target = directory / 'uu-remote-linux-independent-viewer.desktop'
variables = []
previous_entry = target if target.is_file() else legacy
if previous_entry.is_file():
    previous = previous_entry.read_text()
    for line in previous.splitlines():
        if line.startswith('Exec='):
            for argument in shlex.split(line[5:]):
                if argument.startswith(('UU_VIEWER_FONT=', 'UU_VIEWER_SIZE=')):
                    variables.append(argument)
    previous_entry.with_suffix('.desktop.before-independent').write_text(previous)

def quoted(value):
    return '"' + value.replace('\\', '\\\\\\\\').replace('"', '\\"').replace('`', '\\`').replace('$', '\\$').replace('%', '%%') + '"'

arguments = ['/usr/bin/env', *variables, sys.argv[1], 'ui']
target.write_text('[Desktop Entry]\nType=Application\nName=UU Remote Linux Viewer\n'
                  'Name[zh_CN]=UU 远程 Linux 控制端（非官方）\nExec=' + ' '.join(map(quoted, arguments)) +
                  '\nIcon=uu-remote-linux\nTerminal=false\nCategories=Network;RemoteAccess;\n'
                  'StartupNotify=false\nStartupWMClass=uuremote-viewer\n')
# A distinct desktop ID prevents the locally built viewer sharing the package's
# cached launcher. Hide only this user's old menu item; retain the stock binary.
legacy.write_text('[Desktop Entry]\nType=Application\nName=UU Remote Linux Viewer\nHidden=true\nNoDisplay=true\n')
PY
    if command -v update-desktop-database >/dev/null; then
        update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications"
    fi
fi
echo '已安装。主窗口可使用“独立远控”和“会话设置”。 / Installed. Use Independent remote and Session settings in the main window.'
