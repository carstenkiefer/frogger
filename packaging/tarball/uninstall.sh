#!/bin/sh
# Remove a FROGGER installation made with install.sh.
# Face templates in /var/lib/frogger are deleted as well (biometric data).
set -eu
MANIFEST=/usr/share/frogger/install-manifest
[ "$(id -u)" -eq 0 ] || { echo "please run as root" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "no install manifest found" >&2; exit 1; }

if [ -d /run/systemd/system ]; then
  systemctl disable --now frogger.service frogger.socket 2>/dev/null || true
fi
# Take FROGGER out of the PAM stacks before the module disappears.
for f in /etc/pam.d/*; do
  if grep -qs "pam_frogger.so" "$f"; then
    if grep -qs "FROGGER" "$f" && [ "$(basename "$f")" = kde-face ]; then
      rm -f "$f"
    else
      sed -i.frogger-bak '/pam_frogger\.so/d' "$f"
      echo "removed pam_frogger.so from $f (backup: $f.frogger-bak)"
    fi
  fi
done
while read -r f; do rm -f "$f"; done < "$MANIFEST"
rm -f "$MANIFEST"
rmdir /usr/share/frogger/models /usr/share/frogger/pam.d /usr/share/frogger /usr/share/doc/frogger 2>/dev/null || true
rm -rf /var/lib/frogger
[ -d /run/systemd/system ] && systemctl daemon-reload
echo "FROGGER removed."
