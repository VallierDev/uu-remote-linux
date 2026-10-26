#!/bin/sh
# Build uu-remote-linux_<version>_<arch>.deb from release binaries.
# usage: packaging/deb/build-deb.sh VERSION BIN_DIR OUT_DIR   (VERSION without the leading v)
set -eu
version=$1 bin=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd) root=$(cd "$here/../.." && pwd)
arch=$(dpkg --print-architecture 2>/dev/null || true)
if [ -z "$arch" ]; then
  case $(uname -m) in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; loongarch64|loong64) arch=loong64 ;; *) echo "unsupported arch" >&2; exit 1 ;; esac
fi
case $arch in amd64|arm64|loong64|loongarch64) ;; *) echo "unsupported dpkg architecture: $arch" >&2; exit 1 ;; esac
pkg=$(mktemp -d)
trap 'rm -rf "$pkg"' EXIT
install -Dm755 "$bin/uuremote-server" "$pkg/usr/bin/uuremote-server"
[ -f "$bin/uuremote-viewer" ] && install -Dm755 "$bin/uuremote-viewer" "$pkg/usr/bin/uuremote-viewer"
for lib in libuu-wayland.so libuu-portal.so; do
  [ -f "$bin/$lib" ] && install -Dm644 "$bin/$lib" "$pkg/usr/lib/uu-remote-linux/$lib"
done
install -Dm644 "$here/uuremote-native.service" "$pkg/usr/lib/systemd/user/uuremote-native.service"
install -Dm644 "$root/contrib/desktop/uu-remote-linux.desktop" "$pkg/usr/share/applications/uu-remote-linux.desktop"
[ -f "$pkg/usr/bin/uuremote-viewer" ] && install -Dm644 "$root/contrib/desktop/uu-remote-linux-viewer.desktop" "$pkg/usr/share/applications/uu-remote-linux-viewer.desktop"
install -Dm644 "$root/contrib/desktop/uu-remote-linux.svg" "$pkg/usr/share/icons/hicolor/scalable/apps/uu-remote-linux.svg"
install -Dm644 "$root/LICENSE" "$pkg/usr/share/doc/uu-remote-linux/copyright"
# uinput access is opt-in: it lets the service inject input into the whole seat.
printf '%s\n' 'KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"' \
  > "$pkg/usr/share/doc/uu-remote-linux/60-uu-remote-linux-uinput.rules"
size=$(du -sk "$pkg/usr" | cut -f1)
# Library dependencies from the binaries themselves (the server hard, the viewer recommended).
shlibdeps() {
  work=$(mktemp -d); mkdir "$work/debian"
  printf 'Source: uu-remote-linux\n\nPackage: uu-remote-linux\nArchitecture: any\n' > "$work/debian/control"
  (cd "$work" && dpkg-shlibdeps -O --ignore-missing-info "$1" 2>/dev/null) | sed -n 's/^shlibs:Depends=//p'
  rm -rf "$work"
}
depends=$(shlibdeps "$pkg/usr/bin/uuremote-server")
recommends=""
[ -f "$pkg/usr/bin/uuremote-viewer" ] && recommends=$(shlibdeps "$pkg/usr/bin/uuremote-viewer")
[ -n "$depends" ] || { echo "dpkg-shlibdeps produced no dependencies" >&2; exit 1; }
mkdir -p "$pkg/DEBIAN"
cat > "$pkg/DEBIAN/control" <<CONTROL
Package: uu-remote-linux
Version: $version
Architecture: $arch
Maintainer: VallierDev <24893073+VallierDev@users.noreply.github.com>
Installed-Size: $size
Depends: $depends, ffmpeg
Recommends: ${recommends:+$recommends, }libwayland-client0, libxkbcommon0
Suggests: libpipewire-0.3-0, libei1, wl-clipboard
Section: net
Priority: optional
Homepage: https://github.com/VallierDev/uu-remote-linux
Description: Unofficial native Linux host for NetEase UU Remote
 Lets the official NetEase UU Remote clients control this Linux desktop
 (X11, wlroots Wayland and GNOME/KDE Wayland through runtime plugins).
 Includes the experimental uuremote-viewer. Not affiliated with NetEase.
CONTROL
cat > "$pkg/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  cat <<'MSG'
uu-remote-linux installed. As your desktop user (not root):
  uuremote-server login                                  # scan the QR code with the UU Remote app
  systemctl --user enable --now uuremote-native.service  # start with the graphical session
Optional uinput keyboard/mouse (Wayland): see /usr/share/doc/uu-remote-linux/60-uu-remote-linux-uinput.rules
MSG
fi
POSTINST
chmod 755 "$pkg/DEBIAN/postinst"
mkdir -p "$out"
dpkg-deb --build --root-owner-group "$pkg" "$out/uu-remote-linux_${version}_${arch}.deb"
