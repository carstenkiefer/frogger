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
* ~~Optional GNOME Shell extension~~ (done, experimental): `frogger-gnome`
  runs face recognition in parallel to the password field on the lock
  screen and in GDM (PAM service `gdm-frogger`, two retries per prompt) –
  see [GNOME-SHELL.md](GNOME-SHELL.md). Field test on GNOME 46 and 50
  pending.
* ~~Stop the camera as soon as the PAM client disconnects~~ (done).
* Optional confirmation after recognition (unlock only after a key press).

**Enrollment**
* ~~Live feedback~~ (done): "come closer", "keep your face centred", "too
  dark" when frames have been unusable for 1.5 s; spoken once the voice
  packages have clips for them.
* ~~`frogger refine`~~ (done): add samples to an existing enrollment later
  (with glasses, different light, another position) without starting over;
  keeps the eye calibration and checks that the face matches.

**Liveness hardening** (no new models)
* **Head-turn parallax (active, optional):** during a head turn the nose
  tip moves differently from the plane of eyes and mouth corners; a flat
  print cannot produce this. Field data (one laptop, three recordings): residual 0.10–0.18
  eye distances for a real head turn vs. 0.02–0.06 noise. Without a head
  turn there is no signal, so this becomes an optional challenge
  (`liveness.mode = "blink+turn"`, automatic on sensitive devices), not a
  passive check. Targets the "print with cut-out eyes" attack. Needs traces
  of moved and bent prints before the threshold is fixed.
* ~~Corneal glint check~~ (dropped for now): on a 640×360 IR camera the eye
  is only a few pixels large and no corneal glint is measurable (traces:
  peak 5 grey levels above the eye region); glasses produce strong
  reflections that say nothing about liveness. Revisit for higher-resolution
  cameras.

**Test data** (see [policy](#test-data-policy))
* ~~`frogger doctor --record`~~ (done): records per-frame *measurements*
  (landmarks, eye scores, eye-region brightness for the glint check, lit and
  unlit frames) as JSON, which testers can share safely – see
  [HARDWARE-TESTING.md](HARDWARE-TESTING.md#7-recording-measurement-traces).
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
