# Validating FROGGER on real hardware

The CI tests the pipeline with the real models and simulated camera input.
Thresholds and the blink heuristic must additionally be validated on the IR
cameras you deploy. Please record results per device model.

## 1. Camera

```bash
sudo frogger doctor
```

* The IR camera must be listed as `Infrared` (monochrome formats or neutral
  chroma). Note its USB id.
* If frames are dark, configure the emitter with linux-enable-ir-emitter and
  `sudo frogger emitter import …`; `doctor` should then report the emitter.
* An alternating-brightness note means the emitter strobes – that is fine.

## 2. Enrollment and genuine-user acceptance

```bash
sudo frogger setup        # or: sudo frogger enroll
frogger test              # repeat 20x, vary lighting, glasses on/off, distance
```

Blink slowly (≈ ½ s) as the prompt asks; additionally try quick natural
blinks and note how often they are detected together with the frame rate
reported by `frogger doctor`.

Record `best_similarity`, `matching_frames`, `blink_detected`, `blink_method`
(`patch` = calibrated eye patches, `model` = eye state model), `eye_samples`,
`max_eye_dip`, `max_closed_rise`, `model_max_closed`, `model_min_open`,
`model_blink_phase` and the time to
success. Target: ≥ 95 % success within the 5 s budget (4 s at display
managers and lock screens, see `auth.service_timeouts`). If genuine users stay
below the threshold, re-enroll in the typical working position before
lowering `auth.similarity_threshold`.

## 3. Impostor rejection

Let at least 10 other people run `frogger test <enrolled user>` (as root).
Record the highest `best_similarity` across all impostors – it must stay well
below `auth.similarity_threshold` (aim for a margin of ≥ 0.1).

## 4. Presentation attacks (must all fail)

| Test | Expected result |
|---|---|
| Phone/tablet/monitor showing a photo or video of the user | `not_illuminated` or `no_face` (screen is black in IR) |
| Colour print of the user, held still and moved by hand | `no_blink` |
| Print of an **IR** frame of the user (capture with `v4l2-ctl`, laser print, matte and glossy paper) | `no_blink` |
| Two IR prints (eyes open / eyes closed) swapped in front of the camera | `no_blink` (track reset) |
| IR print with the eyes cut out, held in front of a person who blinks | must fail; blink alone does not stop this – the planned parallax and glint checks target it ([roadmap](ROADMAP.md)); report the result |
| Print held next to the real user | `multiple_faces` |
| User present, request via SSH (`ssh localhost sudo -k true`) | face auth skipped, password prompt |
| Same as above after `systemd-run --user --scope sudo -k true` inside the SSH session | skipped (`journalctl -u frogger` shows "not local") |

Document any success of a presentation attack as a security issue (see
[SECURITY.md](../SECURITY.md)). Do not publish IR images of test persons
(see the [test data policy](ROADMAP.md#test-data-policy)); shred prints
after testing.

## 5. Desktop flows

* GNOME: lock (`Super+L`), press a key, look and blink → unlocked. Reboot →
  first login asks for the password (keyring).
* KDE Plasma 6.3/6.6: lock, press Enter on the empty field, look and blink.
  Plasma ≥ 6.8: enable the face authenticator, recognition starts on its own.
* Terminal: `sudo -k true`, `su - <user>`, `pkexec true`.
* Laptop with closed lid and external monitor: face auth skipped immediately.
* Wake the lock screen while looking away from the camera: the password
  prompt must appear after about 2 s (`auth.no_face_timeout_secs`).
* First login after boot: the password prompt must appear without a
  noticeable delay (the daemon answers before its models are loaded).
* Fedora/RHEL with SELinux enforcing: GDM and the lock screen must reach the
  daemon socket – check `ausearch -m avc -ts recent` for denials mentioning
  `frogger`.

## 6. Performance

`journalctl -u frogger` logs the model load time; `frogger test` reports
`elapsed_ms`. On slow ARM devices check that blinks are still detected
(`blink_detected: true`) – eye frames are sampled between full analyses.
