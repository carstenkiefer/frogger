# GNOME Shell integration (planned for 0.3)

Status: **design notes, not implemented.** Today FROGGER runs inside the
`gdm-password` PAM service (see the README). This document describes the
optional GNOME Shell extension planned for 0.3 and the reasons behind it.

## Why an extension

GDM and the GNOME lock screen only run their built-in authentication services
(`gdm-password`, `gdm-fingerprint`, `gdm-smartcard`). Only fingerprint and
smartcard run *in parallel* to the password field. Inside `gdm-password`,
face recognition runs *before* the password prompt, so the password field
appears only after recognition gave up (at most 4 s, 2 s if nobody looks at
the camera – the improvements made in 0.2.x).

An extension can start an additional PAM service next to `gdm-password`, the
way GNOME starts `gdm-fingerprint`: the password field is usable at all
times, and recognition runs in the background with proper visual feedback.

## Plan

* **Separate, optional package** `frogger-gnome` (architecture independent).
  Without it everything keeps working via `gdm-password`; when a GNOME
  release breaks the extension, users fall back to that path automatically.
* **PAM service `/etc/pam.d/gdm-frogger`** containing only `pam_frogger.so`
  (`auth requisite`, no password modules). `frogger pam enable` removes
  FROGGER from `gdm-password` while the extension is active, so recognition
  does not run twice.
* **Extension** (session modes `unlock-dialog` and `gdm`) hooks into the
  shell's user verifier and starts `gdm-frogger` together with
  `gdm-password`. It shows a camera status (searching / blink / recognised /
  failed) in the auth prompt and stops the service as soon as the password
  is submitted.
* **First login after boot** still requires the password (keyring); the
  extension does not start the face service in that case. FROGGER will not
  store the user's password to unlock the keyring – face login must never
  reveal the password.
* **Optional confirmation** (like Windows Hello's "automatically dismiss the
  lock screen" switch): after recognition, unlock only after a key press,
  so the screen does not unlock just because someone looks at it.
* **Polkit dialogs** (GNOME's own polkit agent) are out of scope for the
  first version; they keep using the sequential path.

## Daemon / PAM work needed first

* Stop the camera immediately when the PAM client disconnects (the shell
  cancels the face service when the password is submitted); today an
  attempt runs until its time budget ends.
* A retry policy for the parallel service: start a new attempt when a face
  appears again, up to `auth.max_failures`.
* Add `gdm-frogger` to the default `auth.services` and to
  `auth.login_services`.

## Risks

* GNOME has no stable extension API. The auth prompt internals changed
  between GNOME 48 (Debian 13), 50 (Ubuntu 26.04) and 51 (refactored auth
  services for the new switchable authentication in GDM), so the extension
  needs one code path per major version and testing for every GNOME release.
* The extension runs inside the lock screen: errors there can block
  unlocking. It must catch every exception and fall back to the password.
* The extension must be enabled for the GDM greeter user via a dconf
  profile, which differs between distributions (`gdm` vs `Debian-gdm`).
* SELinux (Fedora): the GDM session worker must be allowed to connect to
  `/run/frogger/frogger.sock`.

## Long term

GDM is getting switchable authentication methods. A built-in "face" slot in
GDM, like the one KDE Plasma 6.8 provides (`kde-face`), would make the
extension unnecessary; FROGGER would then only ship a PAM file, as it does
for KDE. Raising this with the GDM developers is part of the 0.3 work.
