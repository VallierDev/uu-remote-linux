#!/bin/sh
set -eu
if [ "$(uname -s)" != Darwin ]; then
    echo 'Install this companion on the controlling Mac.' >&2
    exit 1
fi
command -v python3 >/dev/null
source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
data_dir="$HOME/.local/share/uuremote/macos-ssh"
bin_dir="$HOME/.local/bin"
mkdir -p "$data_dir" "$bin_dir"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
swiftc -O "$source_dir/clipboard-files.swift" -o "$scratch/clipboard-files"
"$scratch/clipboard-files" --self-test
install -m 755 "$scratch/clipboard-files" "$data_dir/clipboard-files"
install -m 755 "$source_dir/uu-ssh-paste" "$data_dir/uu-ssh-paste"
if [ -e "$bin_dir/uu-ssh-paste" ] && [ ! -L "$bin_dir/uu-ssh-paste" ]; then
    echo 'An existing uu-ssh-paste file was preserved; inspect it before replacing it.' >&2
    exit 1
fi
if [ -L "$bin_dir/uu-ssh-paste" ] && [ "$(readlink "$bin_dir/uu-ssh-paste")" != "$data_dir/uu-ssh-paste" ]; then
    echo 'An unrelated uu-ssh-paste symlink was preserved; inspect it before replacing it.' >&2
    exit 1
fi
ln -sfn "$data_dir/uu-ssh-paste" "$bin_dir/uu-ssh-paste"
echo 'Installed uu-ssh-paste. Connect using your existing UU SSH alias.'
