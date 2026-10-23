#!/bin/sh
# Install uu-remote-linux from the release tarball for the current desktop user, and install FFmpeg
# (needed for desktop video) through the system package manager when it is missing.
# usage: ./install.sh [--yes] [--no-ffmpeg]
#   --yes        install FFmpeg without asking
#   --no-ffmpeg  only check FFmpeg, never install it
set -eu

assume_yes=0 ffmpeg_install=1
for arg in "$@"; do
  case $arg in
    -y|--yes) assume_yes=1 ;;
    --no-ffmpeg) ffmpeg_install=0 ;;
    -h|--help) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
  esac
done

if [ "$(id -u)" = 0 ]; then
  echo "Run this as your desktop user, not root: the host is a per-user service. It asks for sudo only to install FFmpeg." >&2
  exit 1
fi

src=$(cd "$(dirname "$0")" && pwd)
bin=$HOME/.local/bin lib=$HOME/.local/lib/uu-remote-linux share=$HOME/.local/share
unit_dir=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user

# A running binary cannot be overwritten in place ("text file busy"): copy, then rename.
put() { install -Dm"$1" "$2" "$3.new" && mv -f "$3.new" "$3"; }

[ -f "$src/uuremote-server" ] || { echo "uuremote-server not found next to install.sh" >&2; exit 1; }
put 755 "$src/uuremote-server" "$bin/uuremote-server"
[ -f "$src/uuremote-viewer" ] && put 755 "$src/uuremote-viewer" "$bin/uuremote-viewer"
for so in libuu-wayland.so libuu-portal.so; do
  [ -f "$src/$so" ] && put 644 "$src/$so" "$lib/$so"
done
if [ -d "$src/contrib/desktop" ]; then
  put 644 "$src/contrib/desktop/uu-remote-linux.svg" "$share/icons/hicolor/scalable/apps/uu-remote-linux.svg"
  put 644 "$src/contrib/desktop/uu-remote-linux.desktop" "$share/applications/uu-remote-linux.desktop"
  [ -f "$bin/uuremote-viewer" ] && put 644 "$src/contrib/desktop/uu-remote-linux-viewer.desktop" "$share/applications/uu-remote-linux-viewer.desktop"
fi
echo "Installed uuremote-server to $bin"

# Same unit as the .deb, pointing at ~/.local/bin. An existing unit is the user's: leave it alone.
if [ -f "$unit_dir/uuremote-native.service" ]; then
  echo "Kept existing $unit_dir/uuremote-native.service"
else
  mkdir -p "$unit_dir"
  cat > "$unit_dir/uuremote-native.service" <<'UNIT'
[Unit]
Description=UU Remote Linux host (unofficial)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Environment=RUST_LOG=info
ExecStart=%h/.local/bin/uuremote-server
Restart=always
RestartSec=5

[Install]
WantedBy=graphical-session.target
UNIT
  echo "Created $unit_dir/uuremote-native.service"
fi
if command -v systemctl >/dev/null 2>&1; then systemctl --user daemon-reload 2>/dev/null || true; fi

# FFmpeg with libx264 is required: without it the viewer waits on 正在传输画面 forever.
ffmpeg_ok() { command -v ffmpeg >/dev/null 2>&1 && ffmpeg -hide_banner -encoders 2>/dev/null | grep -qw libx264; }

ffmpeg_cmd=""
ffmpeg_note=""
if command -v apt-get >/dev/null 2>&1; then
  ffmpeg_cmd="apt-get update && apt-get install -y ffmpeg"
elif command -v pacman >/dev/null 2>&1; then
  ffmpeg_cmd="pacman -S --needed --noconfirm ffmpeg"
elif command -v dnf >/dev/null 2>&1; then
  # Fedora's own ffmpeg-free has no libx264; the full build is in RPM Fusion, which we do not enable for you.
  if dnf repolist --enabled 2>/dev/null | grep -qi rpmfusion-free; then
    ffmpeg_cmd="dnf install -y --allowerasing ffmpeg"
  else
    ffmpeg_note="Enable RPM Fusion (https://rpmfusion.org/Configuration), then: sudo dnf install --allowerasing ffmpeg"
  fi
elif command -v zypper >/dev/null 2>&1; then
  ffmpeg_note="openSUSE's ffmpeg has no libx264; add the Packman repository, then: sudo zypper install ffmpeg"
else
  ffmpeg_note="Install FFmpeg with libx264 from your distribution, or set \"ffmpeg_root\" in ~/.config/uuremote/config.json"
fi

ffmpeg_ready=1
if ffmpeg_ok; then
  echo "FFmpeg with libx264: found"
else
  if command -v ffmpeg >/dev/null 2>&1; then echo "FFmpeg is installed but has no libx264."; else echo "FFmpeg is not installed."; fi
  if [ "$ffmpeg_install" = 1 ] && [ -n "$ffmpeg_cmd" ]; then
    answer=n
    if [ "$assume_yes" = 1 ]; then
      answer=y
    elif [ -t 0 ]; then
      printf 'Install it now with: sudo sh -c "%s"? [Y/n] ' "$ffmpeg_cmd"
      read -r answer || answer=n
      case $answer in ""|y|Y|yes|YES) answer=y ;; esac
    fi
    if [ "$answer" = y ]; then
      if [ "$(id -u)" != 0 ] && ! command -v sudo >/dev/null 2>&1; then
        echo "sudo is not available; run as root: $ffmpeg_cmd" >&2
      else
        sudo sh -c "$ffmpeg_cmd" || echo "FFmpeg installation failed." >&2
      fi
    fi
  fi
  if ffmpeg_ok; then
    echo "FFmpeg with libx264: installed"
  else
    ffmpeg_ready=0
    if [ -n "$ffmpeg_cmd" ]; then echo "Install it with: sudo sh -c \"$ffmpeg_cmd\""; else echo "$ffmpeg_note"; fi
  fi
fi

case ":$PATH:" in *":$bin:"*) ;; *) echo "Note: $bin is not on PATH; add it or call $bin/uuremote-server directly." ;; esac
cat <<NEXT

Next steps (as this user, inside the graphical session):
  uuremote-server login                                  # scan the QR code with the UU Remote app
  systemctl --user enable --now uuremote-native.service  # start with the graphical session
Desktops that do not export DISPLAY to systemd (e.g. Xfce) need a drop-in; see README step 4.
NEXT
if [ "$ffmpeg_ready" = 0 ]; then
  echo "WARNING: desktop video will not start until FFmpeg with libx264 is installed." >&2
  exit 1
fi
