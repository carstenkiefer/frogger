#!/bin/sh
# Install the package inside a distro container and check that everything
# links and runs. No camera or systemd is available here.
set -eu
if [ "${1:-}" = "--installed" ]; then
  # Already installed by the caller (PKGBUILD, generic archive).
  PAMDIR=$(dirname "$(find /usr/lib64 /usr/lib /lib64 /lib -name pam_frogger.so 2>/dev/null | head -1)")
elif command -v apt-get >/dev/null; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq ./dist/frogger_*_"$(dpkg --print-architecture)".deb
  PAMDIR=/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || gcc -print-multiarch 2>/dev/null || echo "$(uname -m)-linux-gnu")/security
  # Exercise the pam-auth-update profile.
  pam-auth-update --package --enable frogger
  grep -q pam_frogger.so /etc/pam.d/common-auth
  pam-auth-update --package --remove frogger
  ! grep -q pam_frogger.so /etc/pam.d/common-auth
else
  dnf install -y -q ./dist/frogger-*."$(uname -m)".rpm
  PAMDIR=/usr/lib64/security
fi
test -f "$PAMDIR/pam_frogger.so"
# The PAM module must resolve all symbols against the system libpam.
if ldd "$PAMDIR/pam_frogger.so" | grep -q "not found"; then
  ldd "$PAMDIR/pam_frogger.so"; exit 1
fi
frogger --version
frogger --help >/dev/null
LANG=de_DE.UTF-8 frogger status 2>&1 | grep -q "froggerd" # daemon not running → localized error
# Load models once to prove the inference engine works on this platform.
rm -f /tmp/frogger.sock
DAEMON=$(command -v /usr/libexec/froggerd /usr/lib/frogger/froggerd | head -1)
FROGGER_SOCKET=/tmp/frogger.sock timeout 20 "$DAEMON" &
PID=$!
for _ in $(seq 1 40); do [ -S /tmp/frogger.sock ] && break; sleep 0.25; done
frogger --socket /tmp/frogger.sock status | grep -q '"version"'
kill "$PID"
echo "smoke test OK"
