#!/usr/bin/env bash
# Build .deb and .rpm packages from release binaries – only dpkg-deb and
# rpmbuild are required, no third-party packaging tools.
#
#   packaging/build-packages.sh <amd64|arm64> <dir with froggerd, frogger, libpam_frogger.so> [out-dir]
#
# Environment: VERSION (default: from Cargo.toml), MODEL_DIR (default:
# target/models, filled by packaging/fetch-models.sh), MAINTAINER.
set -euo pipefail

ARCH=${1:?arch (amd64|arm64)}
BIN=${2:?binary directory}
OUT=${3:-dist}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODEL_DIR=${MODEL_DIR:-$ROOT/target/models}
VERSION=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}
# Pre-releases (1.2.3-alpha) must sort before the final release: dpkg/rpm use "~".
PKG_VERSION=${VERSION//-/\~}
MAINTAINER=${MAINTAINER:-"FROGGER maintainers <frogger@users.noreply.github.com>"}

case "$ARCH" in
  amd64) MULTIARCH=x86_64-linux-gnu; RPM_ARCH=x86_64 ;;
  arm64) MULTIARCH=aarch64-linux-gnu; RPM_ARCH=aarch64 ;;
  *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
esac

for f in froggerd frogger frogger-askpass libpam_frogger.so; do
  [ -f "$BIN/$f" ] || { echo "missing $BIN/$f" >&2; exit 1; }
done
for m in face_detection_yunet_2023mar.onnx face_recognition_sface_2021dec.onnx open_closed_eye.onnx; do
  [ -f "$MODEL_DIR/$m" ] || { echo "missing model $MODEL_DIR/$m (run packaging/fetch-models.sh)" >&2; exit 1; }
done

# Minimum glibc actually required by the binaries (highest GLIBC_x symbol
# version they reference) – never hand-maintained.
GLIBC_MIN=$(readelf -V "$BIN/froggerd" "$BIN/frogger" "$BIN/frogger-askpass" "$BIN/libpam_frogger.so" \
  | sed -n 's/.*Name: GLIBC_\([0-9.]*\).*/\1/p' | sort -V | tail -1)
echo "binaries require glibc >= $GLIBC_MIN"

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# stage <root> <pam module dir> <account include> <with pam-auth-update profile>
stage() {
  local r=$1 pamdir=$2 account=$3 pamcfg=$4
  install -Dm755 "$BIN/froggerd" "$r/usr/libexec/froggerd"
  install -Dm755 "$BIN/frogger-askpass" "$r/usr/libexec/frogger-askpass"
  install -Dm755 "$BIN/frogger" "$r/usr/bin/frogger"
  install -Dm644 "$BIN/libpam_frogger.so" "$r$pamdir/pam_frogger.so"
  install -Dm644 -t "$r/usr/share/frogger/models" \
    "$MODEL_DIR/face_detection_yunet_2023mar.onnx" "$MODEL_DIR/face_recognition_sface_2021dec.onnx" \
    "$MODEL_DIR/open_closed_eye.onnx"
  install -Dm644 "$ROOT/packaging/config.toml" "$r/usr/share/frogger/config.toml.example"
  install -d "$r/usr/share/frogger/pam.d"
  sed "s/@ACCOUNT@/$account/" "$ROOT/packaging/pam.d/kde-face" > "$r/usr/share/frogger/pam.d/kde-face"
  chmod 644 "$r/usr/share/frogger/pam.d/kde-face"
  install -Dm644 -t "$r/usr/lib/systemd/system" "$ROOT/packaging/systemd/frogger.socket" "$ROOT/packaging/systemd/frogger.service"
  install -Dm644 "$ROOT/packaging/sysusers.d/frogger.conf" "$r/usr/lib/sysusers.d/frogger.conf"
  if [ "$pamcfg" = yes ]; then
    install -Dm644 "$ROOT/packaging/pam-configs/frogger" "$r/usr/share/pam-configs/frogger"
  fi
  install -Dm644 -t "$r/usr/share/doc/frogger" "$ROOT/README.md" "$ROOT/SECURITY.md" \
    "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$ROOT/THIRD_PARTY.md"
}

# ---- Debian / Ubuntu -------------------------------------------------------
DEB=$WORK/deb
stage "$DEB" "/usr/lib/$MULTIARCH/security" common-account yes
install -d "$DEB/DEBIAN"
install -m755 "$ROOT/packaging/debian/postinst" "$ROOT/packaging/debian/prerm" "$ROOT/packaging/debian/postrm" "$DEB/DEBIAN/"
SIZE=$(du -sk --exclude=DEBIAN "$DEB" | cut -f1)
cat > "$DEB/DEBIAN/control" <<CONTROL
Package: frogger
Version: $PKG_VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Installed-Size: $SIZE
Depends: libc6 (>= $GLIBC_MIN), libgcc-s1, libpam0g, libpam-runtime, systemd
Suggests: linux-enable-ir-emitter
Section: admin
Priority: optional
Homepage: https://github.com/carstenkiefer/frogger
Description: Face authentication with IR cameras (Windows-Hello-like)
 FROGGER lets users log in, unlock the screen and authorise sudo/polkit
 with their face. It requires an infrared camera and a blink-based liveness
 check so that photos and screens cannot be used to log in. Devices without
 an IR camera are refused by default policy.
CONTROL
dpkg-deb --root-owner-group -Zxz --build "$DEB" "$OUT/frogger_${PKG_VERSION}_${ARCH}.deb"

# ---- Fedora / RHEL / openSUSE ---------------------------------------------
RPMSTAGE=$WORK/rpmroot
stage "$RPMSTAGE" "/usr/lib64/security" system-auth no
RPMTOP=$WORK/rpmbuild
mkdir -p "$RPMTOP"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
sed -e "s/@VERSION@/$PKG_VERSION/" -e "s/@GLIBC_MIN@/$GLIBC_MIN/" "$ROOT/packaging/rpm/frogger.spec.in" > "$RPMTOP/SPECS/frogger.spec"
rpmbuild -bb --target "$RPM_ARCH" \
  --define "_topdir $RPMTOP" \
  --define "stage $RPMSTAGE" \
  "$RPMTOP/SPECS/frogger.spec" >/dev/null
find "$RPMTOP/RPMS" -name '*.rpm' -exec cp {} "$OUT/" \;

# ---- Generic archive (Arch, Gentoo, Void-glibc, ...) ----------------------
TARNAME=frogger-${VERSION}-${ARCH}-linux
T=$WORK/tar/$TARNAME
install -Dm755 "$BIN/frogger" "$T/bin/frogger"
install -Dm755 "$BIN/froggerd" "$T/libexec/froggerd"
install -Dm755 "$BIN/frogger-askpass" "$T/libexec/frogger-askpass"
install -Dm644 "$BIN/libpam_frogger.so" "$T/lib/pam_frogger.so"
install -Dm644 -t "$T/share/models" \
  "$MODEL_DIR/face_detection_yunet_2023mar.onnx" "$MODEL_DIR/face_recognition_sface_2021dec.onnx" \
    "$MODEL_DIR/open_closed_eye.onnx"
install -Dm644 "$ROOT/packaging/config.toml" "$T/share/config.toml.example"
install -Dm644 "$ROOT/packaging/pam.d/kde-face" "$T/share/pam.d/kde-face"
install -Dm644 -t "$T/systemd" "$ROOT/packaging/systemd/frogger.socket" "$ROOT/packaging/systemd/frogger.service" \
  "$ROOT/packaging/sysusers.d/frogger.conf"
install -Dm644 -t "$T/doc" "$ROOT/README.md" "$ROOT/SECURITY.md" "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" \
  "$ROOT/THIRD_PARTY.md"
install -m755 "$ROOT/packaging/tarball/install.sh" "$ROOT/packaging/tarball/uninstall.sh" "$T/"
echo "$ARCH" > "$T/ARCH"
echo "$GLIBC_MIN" > "$T/GLIBC_MIN"
tar -C "$WORK/tar" --owner=0 --group=0 -czf "$OUT/$TARNAME.tar.gz" "$TARNAME"

# ---- GNOME Shell extension (architecture independent, built once) ---------
if [ "$ARCH" = amd64 ]; then
  VERSION=$VERSION MAINTAINER=$MAINTAINER "$ROOT/packaging/build-gnome-package.sh" "$OUT"
fi
ls -l "$OUT"
