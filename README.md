# 🐸 FROGGER

*Face Recognition On Gnome – Made in Germany*

Windows-Hello-style face authentication for Linux: log in, unlock the screen,
and authorise `sudo`, `su` and polkit prompts with your face – on devices with
an **infrared camera**, with a liveness check so that **a photo alone can never
log you in**.

> **Status: early development.** The pipeline is tested with the real models
> and simulated camera input, but not yet validated on a broad range of IR
> camera hardware ([test procedure](docs/HARDWARE-TESTING.md)). Do not rely on
> it as the only safeguard yet; the password always keeps working.

## Highlights

* **IR + blink liveness.** Screens are invisible to IR cameras; printed photos
  cannot blink. See [SECURITY.md](SECURITY.md) for the full threat model.
* **No face login from remote sessions.** Locality is derived from
  systemd-logind for the requesting process – SSH users can't ride on your
  face.
* **Safe defaults for fleets.** Devices without an IR camera are refused unless
  an administrator explicitly allows it (`policy.allow_insecure_rgb`), and
  `policy.sensitive_device` forbids it altogether.
* **Pure Rust, free models.** Inference with [tract](https://github.com/sonos/tract)
  (no C++ runtime), models from the OpenCV Zoo (YuNet: MIT, SFace: Apache-2.0)
  – commercially usable, unlike InsightFace or dlib's 68-point model
  ([THIRD_PARTY.md](THIRD_PARTY.md)).
* **16 languages**: English, Deutsch, Français, Español, Português, 日本語,
  中文, 한국어, हिन्दी, Kiswahili, Русский, العربية, Bahasa Indonesia, Bahasa
  Melayu, Ikinyarwanda, Runyankore – chosen from
  `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG` of the caller (Runyankore has no
  glibc locale: use `LANGUAGE=nyn`).
* **Depth cameras (experimental):** with an Intel RealSense or another V4L2
  depth camera (`Z16`), `camera.depth.mode = "auto"` additionally rejects
  flat faces; `"required"` demands depth confirmation. Off by default until
  validated on real hardware.
* **SSH, securely:** confirm every use of your SSH keys with your face on
  the laptop (`frogger ssh enable`, `ssh-add -c`), including `sudo` on
  servers via `pam_ssh_agent_auth` – see [docs/SSH.md](docs/SSH.md).
* **Face data sealed to the TPM** (systemd ≥ 254): templates are encrypted
  with a key that only this device's TPM 2.0 chip can unlock.
* **amd64 and arm64**, `.deb` and `.rpm`, built and install-tested in CI.

## Supported systems

| | Status |
|---|---|
| Ubuntu 26.04 LTS (GNOME 50, sudo-rs) / Kubuntu 26.04 (Plasma 6.6) | primary target, `.deb` |
| Debian 13 "trixie" (GNOME 48 / Plasma 6.3) | primary target, `.deb` |
| Fedora, RHEL / AlmaLinux / Rocky 10 | `.rpm`, PAM set up manually (authselect) |
| Arch Linux and derivatives | `PKGBUILD` in `packaging/arch` |
| Debian 12, Ubuntu 22.04/24.04, RHEL 9 and derivatives | best effort: packages install and run (tested in CI); [Howdy](https://github.com/boltgolt/howdy) is an established alternative there |
| Other glibc distributions (Gentoo, Void, Slackware, …) | generic `.tar.gz` with `install.sh` / `uninstall.sh` |
| musl-based (Alpine), NixOS | not supported yet |
| CPU | amd64 (x86-64 baseline) and arm64; binaries need glibc ≥ 2.34 |
| Cameras | V4L2/UVC devices (GREY, Y8I, Y10, Y12, Y16, YUYV, UYVY, NV12/NV21, YU12/YV12, MJPG), IR emitters that are always on or strobe, combined RGB+IR laptop cameras. MIPI/IPU6 cameras need a V4L2 bridge (libcamera/v4l2loopback). |

## Installation

Download the package for your architecture from the
[releases](https://github.com/carstenkiefer/frogger/releases) (or the CI
artifacts) and run:

```bash
sudo apt install ./frogger_*_amd64.deb
```

```bash
sudo frogger setup
```

`setup` checks the cameras, explains whether the device is suitable, pins the
camera, enables the PAM integration (`pam-auth-update` on Debian/Ubuntu) and
enrolls the invoking user. Enrollment takes ~15 seconds: look at the camera,
turn your head slightly, close your eyes when asked, then **blink slowly** once to
verify.

Many IR cameras need their emitter switched on via a UVC extension unit. If
the image is dark, configure it with
[linux-enable-ir-emitter](https://github.com/EmixamPP/linux-enable-ir-emitter)
and import the result (the daemon applies it itself afterwards):

```bash
sudo frogger emitter import ~/.config/linux-enable-ir-emitter.toml
```

### Fedora / RHEL

```bash
sudo dnf install ./frogger-*.x86_64.rpm
```

PAM is configured with `authselect` there; create a custom profile and add
`auth [success=end default=ignore] pam_frogger.so` at the top of the `auth`
section of `system-auth` (`frogger pam enable` prints the exact steps).

### Arch Linux

```bash
cd packaging/arch && makepkg -si
```

### Other distributions (generic archive)

```bash
tar xzf frogger-*-amd64-linux.tar.gz
```

```bash
sudo ./frogger-*-amd64-linux/install.sh
```

The installer finds the PAM module directory of the distribution, creates the
`frogger` system user and enables the socket. Remove everything again with
`sudo /usr/share/frogger/uninstall.sh`.

## Tips for reliable recognition

* **Blink slowly.** Liveness requires an open → closed → open eye movement.
  Many laptop IR cameras deliver only 15 frames per second, and emitters that
  strobe light only every second frame – a quick blink (100–150 ms) can fall
  between two usable frames. A slow, deliberate blink (≈ ½ s) is always
  seen. `frogger doctor` shows the measured frame rate of your camera.
* **Distance and posture:** sit at your usual working distance (40–70 cm)
  and look at the screen; enroll in the same position you usually work in.
* **Glasses:** strong reflections of the IR emitter can hide the eyes. If
  enrollment reports that open and closed eyes cannot be told apart, tilt
  the glasses slightly or enroll without them.
* **Diagnosis:** `frogger test` prints similarity and liveness statistics;
  failed attempts are also logged (`journalctl -u frogger`).

## Usage

| Command | Purpose |
|---|---|
| `frogger doctor` | Cameras, IR detection, policy, daemon and PAM status |
| `sudo frogger setup` | Guided setup |
| `sudo frogger enroll [user]` | (Re-)enroll a face |
| `frogger test` | Try recognition, print similarity scores |
| `frogger list` / `frogger remove` | Manage your face data |
| `sudo frogger pam enable\|disable` | Toggle the PAM integration |

## Desktop integration

**GNOME (GDM + lock screen).** GDM runs only its built-in services
(`gdm-password`, `gdm-fingerprint`, `gdm-smartcard`) and has no slot for an
additional face service. FROGGER therefore runs inside `gdm-password` (via
`common-auth`): select your user / wake the lock screen, look at the camera,
blink slowly – done. If recognition fails, the password prompt appears. The first
login after boot always asks for the password so that the GNOME Keyring is
unlocked (`require_password_for_first_login`).

**KDE Plasma.** SDDM and the lock screen use `common-auth` as well. Plasma
6.3/6.6 starts the PAM conversation of the lock screen when you press Enter on
the empty password field. Plasma ≥ 6.8 has a dedicated, opt-in face
authenticator slot (PAM service `kde-face`). It does *not* contain face
recognition itself – it calls whatever PAM module is configured.
`frogger pam enable` installs `/etc/pam.d/kde-face` for it; enable "Face" in
the lock screen settings.

**Terminal.** `sudo`, `sudo-rs`, `su` and `pkexec` show the prompts from the
PAM conversation ("look at the camera and blink slowly once"). Over SSH, face
authentication is skipped.

## Configuration

`/etc/frogger/config.toml` – see the documented
[example](packaging/config.toml). All keys have secure defaults; unknown keys
are rejected. Fleet policy:

```toml
[policy]
allow_insecure_rgb = false   # default: no face login without IR camera
sensitive_device = true      # forces IR + blink, forbids RGB
```

## Architecture

```
 sudo / su / pkexec / gdm / sddm / kscreenlocker
        │  libpam
        ▼
 pam_frogger.so ── JSON lines over /run/frogger/frogger.sock ──► froggerd
 (thin client, no ML,            (SO_PEERCRED identity)          │ runs as user "frogger", sandboxed
  relays messages)                                               │ camera (V4L2) → YuNet → align → SFace
                                                                 │ eye calibration → blink challenge
 frogger (CLI) ─────────────────────────────────────────────────►│ templates in /var/lib/frogger
```

| Crate | |
|---|---|
| `frogger-proto` | wire protocol |
| `frogger-i18n` | message catalogs (`i18n/*.toml`, compiled in) |
| `frogger-core` | camera, vision pipeline, liveness (`auth.rs`), locality (`session.rs`), storage |
| `froggerd` | daemon |
| `frogger-cli` | `frogger` command |
| `pam-frogger` | `pam_frogger.so` |

## Building

```bash
sudo apt install build-essential libpam0g-dev libclang-dev
cargo build --release --workspace
cargo test --workspace
packaging/fetch-models.sh                         # downloads + verifies models
packaging/build-packages.sh amd64 target/release  # needs dpkg-deb, rpmbuild
```

## Packaging decisions

* **deb + rpm**: built natively on GitHub's amd64 and arm64 runners with only
  `dpkg-deb`/`rpmbuild`; every package is installed and smoke-tested in Debian
  13, Ubuntu 26.04 and Fedora containers. To save runner time this happens
  for pull requests, release tags and manual runs ("Run workflow"); plain
  pushes to `main` only run lint and tests, docs-only changes nothing. Tags
  `v*` publish a release with `SHA256SUMS`.
* **No Flatpak / Snap.** FROGGER consists of a PAM module loaded by `sudo`,
  GDM and SDDM, a privileged system service and changes to `/etc/pam.d`.
  Flatpak is designed for sandboxed desktop apps and cannot install any of
  that; a Snap would require classic confinement (manual store review) and
  would still have to inject a module from `/snap` into the host's PAM
  stacks. Neither is sensible for an authentication component.
* **No x86-64-v3 build needed.** Debian 13 and Ubuntu 26.04 default to the
  x86-64 baseline (Ubuntu's amd64v3 archive variant is opt-in). The packages
  target the baseline so they run on every amd64 CPU; the inference engine
  detects AVX2/FMA (and NEON on arm64) at runtime and uses optimised kernels
  anyway. A v3-only build would exclude older and low-end CPUs for no
  measurable gain.

## Translations

Catalogs live in [`i18n/`](i18n). Add a language by copying `en.toml`,
translating the values (keep `{placeholders}`) and registering it in
`crates/frogger-i18n/src/lib.rs`; `cargo test -p frogger-i18n` checks
completeness. Native-speaker review of the existing catalogs is very welcome.

## License

MIT OR Apache-2.0. Models: see [THIRD_PARTY.md](THIRD_PARTY.md).
