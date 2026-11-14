#!/bin/sh
# FROGGER generic installer for distributions without a native package
# (Arch, Gentoo, Void-glibc, Slackware, ...). Run as root from the unpacked
# archive:   sudo ./install.sh
# Remove again with:   sudo /usr/share/frogger/uninstall.sh
set -eu
cd "$(dirname "$0")"
MANIFEST=/usr/share/frogger/install-manifest
die() { echo "frogger install: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "please run as root (sudo ./install.sh)"
ARCH=$(cat ARCH)
case "$(uname -m)" in
  x86_64) [ "$ARCH" = amd64 ] || die "this archive is for $ARCH, not x86_64" ;;
  aarch64|arm64) [ "$ARCH" = arm64 ] || die "this archive is for $ARCH, not aarch64" ;;
  *) die "unsupported CPU architecture $(uname -m)" ;;
esac

# glibc is required (musl systems such as Alpine are not supported).
GLIBC=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || true
[ -n "$GLIBC" ] || die "glibc not found (musl-based systems are not supported)"
NEED=$(cat GLIBC_MIN)
[ "$(printf '%s\n%s\n' "$NEED" "$GLIBC" | sort -V | head -1)" = "$NEED" ] \
  || die "glibc $GLIBC is too old, $NEED or newer is required"

# Where does this distribution keep PAM modules? Follow pam_unix.so.
PAMDIR=$(dirname "$(find /usr/lib64 /usr/lib /lib64 /lib -path '*/security/pam_unix.so' 2>/dev/null | head -1)" 2>/dev/null) || true
[ -n "$PAMDIR" ] && [ "$PAMDIR" != "." ] || die "PAM (pam_unix.so) not found"

# Account stack to reference from the kde-face PAM service.
if [ -f /etc/pam.d/common-account ]; then ACCOUNT=common-account; else ACCOUNT=system-auth; fi

inst() { # mode source destination
  install -D -m "$1" "$2" "$3"
  echo "$3" >> "$MANIFEST.new"
}
mkdir -p /usr/share/frogger
: > "$MANIFEST.new"
inst 755 bin/frogger /usr/bin/frogger
inst 755 libexec/froggerd /usr/libexec/froggerd
inst 755 libexec/frogger-askpass /usr/libexec/frogger-askpass
# Follow the distribution's convention (Debian 644, Arch/Fedora 755).
PAMMODE=$(stat -c %a "$PAMDIR/pam_unix.so" 2>/dev/null || echo 644)
inst "$PAMMODE" lib/pam_frogger.so "$PAMDIR/pam_frogger.so"
for f in share/models/*.onnx; do inst 644 "$f" "/usr/share/frogger/models/$(basename "$f")"; done
inst 644 share/config.toml.example /usr/share/frogger/config.toml.example
sed "s/@ACCOUNT@/$ACCOUNT/" share/pam.d/kde-face > /tmp/frogger-kde-face.$$
inst 644 /tmp/frogger-kde-face.$$ /usr/share/frogger/pam.d/kde-face
rm -f /tmp/frogger-kde-face.$$
inst 644 systemd/frogger.socket /usr/lib/systemd/system/frogger.socket
inst 644 systemd/frogger.service /usr/lib/systemd/system/frogger.service
inst 644 systemd/frogger.conf /usr/lib/sysusers.d/frogger.conf
inst 755 uninstall.sh /usr/share/frogger/uninstall.sh
for f in doc/*; do inst 644 "$f" "/usr/share/doc/frogger/$(basename "$f")"; done
mv "$MANIFEST.new" "$MANIFEST"

# System user with camera access.
if command -v systemd-sysusers >/dev/null 2>&1; then
  systemd-sysusers /usr/lib/sysusers.d/frogger.conf
elif ! id frogger >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/frogger --shell /usr/sbin/nologin --groups video frogger
fi

if [ -d /run/systemd/system ]; then
  systemctl daemon-reload
  systemctl enable --now frogger.socket
else
  echo "Note: systemd is not running. Start /usr/libexec/froggerd as user 'frogger' with your init system;"
  echo "      it creates /run/frogger/frogger.sock itself. Remote-session detection needs systemd-logind or elogind."
fi

echo
echo "FROGGER installed (PAM modules in $PAMDIR)."
echo "Next: sudo frogger setup"
