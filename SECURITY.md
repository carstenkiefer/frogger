# Security model

FROGGER is an authentication component. This document describes what it
protects against, how, and – just as important – what it does **not** protect
against.

## Goals

1. **A photo alone must never log anyone in** – neither printed nor shown on a
   phone, tablet or monitor.
2. Face authentication must only be usable by someone **physically at the
   machine**; it must never authorise a remote (SSH) session.
3. Failure of any component must fall back to the password, never to "allow".
4. Biometric data stays on the device and is accessible to the daemon only.

## Layers

| Attack | Countermeasure |
|---|---|
| Photo/video on a phone, tablet or monitor | IR camera required. LCD/OLED screens emit no near-infrared light, so the face appears black under the IR emitter (`min_ir_face_brightness`). Colour frames on an IR camera are rejected (`NotInfrared`). |
| Printed photo (also printed from an IR image) | **Blink challenge**: within one continuous face track the eyes must go open → closed → open, measured against the user's own eye calibration recorded at enrollment. A static print cannot blink. |
| Swapping two prints (eyes open / eyes closed) | The challenge is bound to a continuous track: a jump of the face box (IoU < `min_track_iou`), a lost face or a different identity resets all evidence. The identity must also match before *and* after the blink. |
| Second person / photo next to the user | Frames with more than one face are discarded (`reject_multiple_faces`). |
| Brute force / lookalikes | Strict similarity threshold, several matching frames required, lockout after `max_failures` (falls back to password). |
| Camera replaced by another device | Camera pinning by USB vendor:product (`expected_usb_id`, written by `frogger setup`); templates remember the camera type. |
| Remote attacker runs `sudo`/`su`/`pkexec` while the user sits at the desk | The daemon derives locality from systemd-logind for the **requesting process** (via `SO_PEERCRED`, not from client claims): remote sessions are refused; on Linux ≥ 6.5 the requesting process is pinned with a pidfd (`SO_PEERPIDFD`), so a PID reused during the check cannot be mistaken for the requester; processes outside a session need a local seat session of the *real* uid and no remote session of that user (closes the `systemd-run --user` escape); the polkit helper service is only served while no remote session exists on the machine. `PAM_RHOST`/SSH variables are checked as an additional pre-filter. |
| Local user probing other users' faces | Unprivileged callers may only authenticate/test/remove **themselves**; enrollment requires root (i.e. a password via sudo). |
| Malformed requests | Line-limited JSON protocol, validated user names, PAM module catches panics and never returns success on error. |
| Daemon compromise | Runs as unprivileged `frogger` user with an extensive systemd sandbox (no network, only video devices, read-only system, no capabilities, seccomp filter). |

## Residual risks (out of scope / not solved)

* **3D masks and sophisticated prosthetics.** No consumer IR camera without a
  depth sensor can reliably reject a well-made 3D mask; neither can Windows
  Hello on most devices.
* **Printed photo with cut-out eyes worn by a person** (real eyes blink behind
  the print). Mitigated somewhat by IR reflectance of paper vs. skin, but not
  reliably detected.
* **Malicious USB camera that injects frames** (cf. CVE-2021-34466 against
  Windows Hello). Camera pinning raises the bar, but USB IDs can be cloned.
  Physical access to USB ports is required.
* **Passive authentication while the user is present.** Malware already
  running as the user can trigger `sudo` while the user looks at the screen
  (and blinks naturally). This is inherent to passive biometrics; on devices
  with sensitive data consider removing `sudo`/`polkit-1` from
  `auth.services`.
* **Twins / close relatives** may exceed the similarity threshold.
* **The blink detector is a heuristic** calibrated per user; extreme glasses
  reflections can make enrollment fail (the user is told so) – it then falls
  back to the password.
* The recognition model was trained on colour images; on IR frames accuracy is
  lower than on the published RGB benchmarks. Thresholds must be validated on
  the target hardware (`frogger test` prints scores).

## Devices without an IR camera

RGB-only face login **can be defeated with a photo or video**. FROGGER refuses
to set it up unless the administrator sets `policy.allow_insecure_rgb = true`,
and then only after an explicit acknowledgement. Every RGB login shows a
warning. `policy.sensitive_device = true` forbids RGB entirely and forces the
blink challenge – set it on every device that holds sensitive data.

## Privacy (GDPR Art. 9)

Face templates are biometric data. FROGGER stores only 128-dimensional
embeddings and small normalised eye patches – no camera images – in
`/var/lib/frogger/users/` (directory 0700, files 0600, owned by the daemon
user). Nothing leaves the device. Purging the package deletes all templates.

On devices with a TPM 2.0 chip and systemd ≥ 254, `frogger setup` seals a
random 256-bit key to the TPM (`systemd-creds`, stored in
`/etc/credstore.encrypted/frogger.key`). systemd decrypts it only for the
daemon, which encrypts every template with ChaCha20-Poly1305 bound to the
user name. A disk removed from the device – or a backup – does not reveal the
templates. The key is bound to the TPM itself, not to PCR measurements, so
firmware and Secure Boot updates do not break face login; clearing the TPM
does (users then re-enroll, the password keeps working). `frogger doctor`
shows whether encryption is active. Full-disk encryption is still
recommended.

## Reporting vulnerabilities

Please report vulnerabilities privately via GitHub Security Advisories of this
repository, not in public issues.
