#!/bin/sh
set -eu
if [ "$(uname -s)" != Linux ] || [ "$#" -ne 0 ]; then
    echo '用法：在 Linux 图形用户会话运行 install.sh / Usage: run install.sh in a Linux desktop session.' >&2
    exit 1
fi
command -v python3 >/dev/null
command -v systemd-run >/dev/null
source_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
bin_dir="$HOME/.local/bin"
mkdir -p "$bin_dir"
temporary=$(mktemp "$bin_dir/.uuremote-independent-XXXXXX")
trap 'rm -f "$temporary"' EXIT HUP INT TERM
install -m 755 "$source_dir/uuremote-independent" "$temporary"
mv -f "$temporary" "$bin_dir/uuremote-independent"
echo '已安装。使用 UU_VIEWER_INDEPENDENT=1 uuremote-viewer ui 启用入口。 / Installed. Opt in with UU_VIEWER_INDEPENDENT=1 uuremote-viewer ui.'
