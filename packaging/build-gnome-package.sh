#!/usr/bin/env bash
# Build the architecture-independent package frogger-gnome (.deb and .rpm):
# the GNOME Shell extension for face login next to the password field.
#
#   packaging/build-gnome-package.sh [out-dir]
#
# Environment: VERSION (default: from Cargo.toml), MAINTAINER.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-dist}
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}
PKG_VERSION=${VERSION//-/\~}
MAINTAINER=${MAINTAINER:-"FROGGER maintainers <frogger@users.noreply.github.com>"}
UUID=frogger@carstenkiefer.github.io
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

stage() { # root
  install -Dm644 -t "$1/usr/share/gnome-shell/extensions/$UUID" \
    "$ROOT/gnome/$UUID/metadata.json" "$ROOT/gnome/$UUID/extension.js"
  install -Dm644 -t "$1/usr/share/doc/frogger-gnome" "$ROOT/docs/GNOME-SHELL.md" \
    "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE"
}
SUMMARY="FROGGER face login next to the password field (GNOME Shell extension)"
TEXT="GNOME Shell extension that runs FROGGER face authentication in parallel
to the password field on the GDM login screen and the lock screen, like a
fingerprint reader. Enable it with 'sudo frogger gnome enable'."

# ---- .deb ----
D=$WORK/deb
stage "$D"
install -d "$D/DEBIAN"
cat >"$D/DEBIAN/control" <<CTL
Package: frogger-gnome
Version: $PKG_VERSION
Architecture: all
Maintainer: $MAINTAINER
Installed-Size: $(du -sk "$D/usr" | cut -f1)
Depends: frogger (>= $PKG_VERSION), gnome-shell (>= 46)
Section: gnome
Priority: optional
Homepage: https://github.com/carstenkiefer/frogger
Description: $SUMMARY
$(sed 's/^/ /' <<<"$TEXT")
CTL
# Removing the extension must not leave the login screen without face
# login: move face authentication back to gdm-password.
cat >"$D/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = remove ] && command -v frogger >/dev/null 2>&1; then
  frogger gnome disable >/dev/null 2>&1 || true
fi
PRERM
chmod 755 "$D/DEBIAN/prerm"
dpkg-deb --root-owner-group --build "$D" "$OUT/frogger-gnome_${PKG_VERSION}_all.deb" >/dev/null

# ---- .rpm ----
TOP=$WORK/rpm
install -d "$TOP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
R=$WORK/rpmroot
stage "$R"
release=1
rpmver=${PKG_VERSION%%~*}
[[ $PKG_VERSION == *~* ]] && release="0.${PKG_VERSION#*~}"
cat >"$TOP/SPECS/frogger-gnome.spec" <<SPEC
Name: frogger-gnome
Version: $rpmver
Release: $release
Summary: $SUMMARY
License: MIT OR Apache-2.0
URL: https://github.com/carstenkiefer/frogger
BuildArch: noarch
Requires: frogger
Requires: gnome-shell >= 46

%description
$TEXT

%install
cp -a $R/. %{buildroot}/

%preun
if [ \$1 -eq 0 ] && command -v frogger >/dev/null 2>&1; then
  frogger gnome disable >/dev/null 2>&1 || :
fi

%files
/usr/share/gnome-shell/extensions/$UUID
%doc /usr/share/doc/frogger-gnome
SPEC
rpmbuild --quiet --define "_topdir $TOP" -bb "$TOP/SPECS/frogger-gnome.spec"
cp "$TOP"/RPMS/noarch/*.rpm "$OUT/"
echo "built frogger-gnome $PKG_VERSION"
