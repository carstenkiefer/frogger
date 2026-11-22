# GNOME: face login next to the password field

Status: **experimental (0.3).** Without the extension FROGGER runs inside the
`gdm-password` PAM service (see the README): recognition runs *before* the
password prompt, which appears after at most 4 s (2 s if nobody looks at the
camera). The optional extension runs face recognition *in parallel* to the
password field, the way GNOME runs a fingerprint reader.

## Setup

```bash
sudo apt install frogger-gnome        # Fedora: sudo dnf install frogger-gnome
sudo frogger gnome enable
```

Then log out and in once (GNOME Shell only discovers new extensions at
login); the login screen uses the extension after the next reboot.
`frogger gnome status` shows the state, `sudo frogger gnome disable` (or
removing the package) returns to the `gdm-password` path.

## How it works

* **PAM service `/etc/pam.d/gdm-frogger`**, written by `frogger gnome enable`:
  only `pam_frogger.so` in the auth stack; the account, session and password
  stacks are copied from the system's `gdm-password`, so it matches every
  distribution (Debian `common-*`, Fedora `password-auth`). GDM accepts
  client-started services whose name begins with `gdm-`.
* **Extension** `frogger@carstenkiefer.github.io` (session modes `user`,
  `unlock-dialog`, `gdm`; GNOME 46–50). It hooks into the shell's user
  verifier (`js/gdm/util.js`), which serves both the login and the lock
  screen:
  * starts `gdm-frogger` together with the password (and fingerprint)
    service once the user is known;
  * shows FROGGER's messages ("look at the camera", "blink") as hints below
    the password field, like fingerprint hints;
  * a failed face attempt never resets the prompt or counts as a failed
    password; it is retried twice per prompt (1.5 s apart), then only the
    password remains until the prompt resets;
  * typing the password ends the face service; the daemon notices the closed
    connection and stops the camera at once (0.3).
* **Configuration:** `frogger gnome enable` replaces `gdm-password` by
  `gdm-frogger` in `auth.services`, so recognition never runs twice.
  `gdm-frogger` is a login service: the first login after boot still needs
  the password (keyring).
* **Login screen:** the extension is enabled for the GDM user through the
  dconf database `gdm` (`/etc/dconf/db/gdm.d/90-frogger`). Distributions
  without such a profile get a message explaining the manual step.
* Everything security-relevant stays in PAM and the daemon. If the extension
  fails (e.g. after a GNOME update), the password keeps working;
  `sudo frogger gnome disable` restores the sequential path.

## Testing on a new GNOME version

1. Lock the screen, wake it, look at the camera and blink: unlocks without
   touching the keyboard; the hint below the password field shows FROGGER's
   prompt.
2. Wake it, look away, type the password: unlocks; `journalctl -u frogger`
   shows `cancelled … the client went away`.
3. Wake it with nobody in front of the camera: after three short attempts
   (about 10 s) the camera turns off; the password still works.
4. Type a wrong password once: the prompt shakes as usual, face recognition
   starts again.
5. Reboot: the first login asks for the password; after locking, the face
   works.
6. `sudo frogger gnome disable`, log out and in: the sequential path
   (`gdm-password`) works again.

## Known limits

* GNOME has no stable extension API. The verifier code the extension relies
  on is identical in GNOME 46 and 50; each new major version must be checked
  (`shell-version` in `metadata.json`).
* GNOME's own polkit dialogs (Settings, Software) do not use the shell's
  user verifier in the same way and keep using `polkit-1` sequentially.
* Fedora/SELinux: the GDM session worker must be allowed to connect to
  `/run/frogger/frogger.sock` – the same requirement as for `gdm-password`.

## Long term

GDM is getting switchable authentication methods. A built-in "face" slot in
GDM, like the one KDE Plasma 6.8 provides (`kde-face`), would make the
extension unnecessary; FROGGER would then only ship a PAM file, as it does
for KDE.
