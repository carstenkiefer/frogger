# Roadmap

Plans, not promises – priorities change with field-test results. Security
fixes always come first.

## 0.3

**Spoken prompts** (done, in the next release)
* The CLI plays the enrollment prompts from optional voice packages
  (`frogger-voice-<lang>`, built in the separate
  [frogger-voices](https://github.com/carstenkiefer/frogger-voices)
  repository) and falls back to system sounds without them. Pre-recorded
  clips only, no speech engine at runtime; one file per message key under
  `/usr/share/frogger/voices/<lang>/`.
* Next: the voices CI checks completeness against the prompts of a given
  FROGGER version; recordings by native speakers replace the generated clips,
  starting with the languages whose clips are licensed for non-commercial
  use only (Meta MMS).

**GNOME**
* Optional GNOME Shell extension (`frogger-gnome`): face recognition in
  parallel to the password field on the lock screen and in GDM – see
  [GNOME-SHELL.md](GNOME-SHELL.md).
* Daemon: stop the camera as soon as the PAM client disconnects; retry
  policy for the parallel service.
* Optional confirmation after recognition (unlock only after a key press).

**Enrollment**
* Live feedback per frame: "move closer", "move back a little", "keep
  your face centred", "too dark" – the checks exist internally, the user
  just doesn't see them yet.
* `frogger refine`: add samples to an existing enrollment later (with
  glasses, different light, another position) without starting over.

**Liveness hardening** (passive, no new models)
* **Corneal glint check:** the IR emitter must produce a specular highlight
  on both corneas that appears in lit frames and disappears in unlit frames
  of a strobing emitter. A printed highlight stays visible without the
  emitter.
* **Parallax check:** during natural head movement, the landmarks of a flat
  image follow a single planar transformation (homography); a real head does
  not (nose vs. cheeks). Targets the "photo with cut-out eyes" attack, which
  a blink challenge alone does not stop.
* Reject virtual cameras (v4l2loopback etc.) unless explicitly allowed.

**Test data** (see [policy](#test-data-policy))
* `frogger doctor --record`: records per-frame *measurements* (glint
  position and brightness, landmark parallax residual, lit/unlit brightness
  ratios, eye scores) as JSON, which testers can share safely.
* Liveness checks operate on these measurements, so regression tests run on
  recorded traces without images.
* First synthetic sequences (freely licensed 3D heads under simulated IR
  light, plus the same heads as flat prints).

## 0.4

* **Emitter shading check:** compare lit and unlit frames of a strobing
  emitter; a 3D face shows distance falloff and shading (nose tip brighter
  than cheeks), a print is nearly uniform. Only after real-world traces
  show it separates reliably (another project measured a related approach
  and rejected it).
* **Attention option** (like Face ID): unlock only with open eyes looking at
  the screen – protects against holding the laptop up to a sleeping person.
  Candidate models: MediaPipe iris landmarks or the OpenVINO gaze model
  (both Apache-2.0), to be validated on IR images.
* **Random challenge as fallback** (turn head / smile) when the passive
  checks are inconclusive, instead of failing.
* Optional TPM PCR 7 (Secure Boot state) binding of the template key, with
  re-sealing guidance for firmware updates.
* Duress gesture (e.g. keeping the eyes closed) that refuses the match and
  disables face login until the next password login.
* Built-in database of IR emitter controls per camera model (USB id), so
  most cameras work without linux-enable-ir-emitter.
* Talk to the GDM developers about a native face slot (like `kde-face` in
  Plasma 6.8), which would make the extension unnecessary.

## Not planned

* Pretrained anti-spoofing CNNs: mostly trained on RGB, generalise poorly
  to unknown IR hardware, and their training data is usually licensed for
  non-commercial use only.
* Pulse detection from video (rPPG): needs 5–10 s, too slow for a login.
* Storing the user's password to unlock the keyring: face login must never
  reveal the password.

## Test data policy

* **No real face images or recordings in this repository** – not even of
  maintainers with consent. Face images are biometric data (GDPR Art. 9),
  cannot be removed from Git history, and public IR images of a person are
  exactly the material needed for an IR-print attack against them.
* The repository contains **derived measurements** (JSON/CSV) from which no
  face can be reconstructed, and **synthetic** data with a free license.
* Raw recordings stay local or in private, encrypted storage with documented
  consent; only measurements extracted from them are published.
* Public anti-spoofing research datasets are used locally only and only if
  their license permits it; they are never redistributed.
* Printed photos used for attack tests are shredded afterwards.
