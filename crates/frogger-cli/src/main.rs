//! `frogger` – setup, enrollment and diagnostics for FROGGER.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use frogger_core::camera::{self, Camera, CameraKind};
use frogger_core::config::{Config, CONFIG_PATH};
use frogger_i18n::tr;
use frogger_proto::{AuthStatus, Event, Op, Request, MAX_LINE, SOCKET_PATH};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const PAM_PROFILE: &str = "frogger";
const CONFIG_EXAMPLE: &str = "/usr/share/frogger/config.toml.example";
const KDE_FACE_TEMPLATE: &str = "/usr/share/frogger/pam.d/kde-face";
const KDE_FACE: &str = "/etc/pam.d/kde-face";
const PAM_LINE: &str = "auth [success=end default=ignore] pam_frogger.so";

#[derive(Parser)]
#[command(
    name = "frogger",
    version = frogger_proto::version(),
    about = "FROGGER – face authentication for Linux with IR cameras"
)]
struct Cli {
    /// Configuration file
    #[arg(long, global = true, default_value = CONFIG_PATH)]
    config: PathBuf,
    /// Daemon socket
    #[arg(long, global = true, default_value = SOCKET_PATH)]
    socket: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check cameras, IR capability, policy, daemon and PAM integration
    Doctor,
    /// Guided setup: pick the camera, enable PAM, enroll the invoking user
    Setup {
        /// Acknowledge that face login without an IR camera is insecure
        #[arg(long)]
        accept_insecure_rgb: bool,
        /// Only configure, do not enroll
        #[arg(long)]
        no_enroll: bool,
    },
    /// Enroll a face (defaults to the user who invoked sudo)
    Enroll {
        user: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        accept_insecure_rgb: bool,
    },
    /// Try to recognize a user and print scores (does not log in)
    Test { user: Option<String> },
    /// List enrolled users
    List { user: Option<String> },
    /// Delete a user's face data
    Remove { user: Option<String> },
    /// Show daemon status
    Status,
    /// Enable or disable FROGGER in the system PAM configuration
    Pam {
        #[arg(value_parser = ["enable", "disable", "status"])]
        action: String,
    },
    /// Confirm every use of your SSH keys (ssh-add -c) with your face
    Ssh {
        #[arg(value_parser = ["enable", "disable", "status"])]
        action: String,
    },
    /// Import IR emitter settings from linux-enable-ir-emitter
    Emitter {
        #[arg(value_parser = ["import"])]
        action: String,
        /// linux-enable-ir-emitter TOML file
        path: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let lang = frogger_i18n::detect();
    if let Err(e) = run(cli, lang) {
        eprintln!("frogger: {e:#}");
        std::process::exit(1);
    }
}

fn is_root() -> bool {
    // SAFETY: trivial libc call.
    unsafe { libc::geteuid() == 0 }
}

fn require_root(lang: &str) -> Result<()> {
    if !is_root() {
        bail!("{}", tr(lang, "cli.need_root", &[]));
    }
    Ok(())
}

/// Target user: explicit argument, else the user who ran sudo, else $USER.
fn target_user(arg: Option<String>) -> Result<String> {
    arg.or_else(|| std::env::var("SUDO_USER").ok().filter(|u| u != "root"))
        .or_else(|| {
            std::env::var("PKEXEC_UID")
                .ok()
                .and_then(|uid| user_name(uid.parse().ok()?))
        })
        .or_else(|| std::env::var("USER").ok())
        .context("cannot determine user name")
}

fn user_name(uid: u32) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid buffers.
    let r = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
    if r != 0 || res.is_null() {
        return None;
    }
    // SAFETY: pw_name points into buf.
    Some(
        unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// Send a request, print info lines, return the terminal event.
fn call(socket: &str, op: Op, lang: &str) -> Result<(AuthStatus, String, Option<serde_json::Value>)> {
    let mut s = UnixStream::connect(socket).map_err(|e| {
        anyhow::anyhow!(
            "{}",
            tr(lang, "cli.daemon_unreachable", &[("error", &e.to_string())])
        )
    })?;
    s.set_read_timeout(Some(Duration::from_secs(120)))?;
    let mut req = Request::new(op);
    req.lang = Some(lang.to_string());
    s.write_all(&frogger_proto::to_line(&req))?;
    let mut reader = BufReader::new(s.take(64 * MAX_LINE as u64));
    loop {
        let mut line = Vec::new();
        if reader.read_until(b'\n', &mut line)? == 0 {
            bail!("daemon closed the connection");
        }
        match serde_json::from_slice::<Event>(&line)? {
            Event::Info { text } => println!("  » {text}"),
            Event::Result {
                status,
                message,
                data,
            } => return Ok((status, message, data)),
            Event::Error { message } => bail!("{message}"),
        }
    }
}

fn run(cli: Cli, lang: &str) -> Result<()> {
    match cli.cmd {
        Cmd::Doctor => doctor(&cli.config, &cli.socket, lang),
        Cmd::Setup {
            accept_insecure_rgb,
            no_enroll,
        } => setup(&cli.config, &cli.socket, accept_insecure_rgb, no_enroll, lang),
        Cmd::Enroll {
            user,
            label,
            accept_insecure_rgb,
        } => {
            require_root(lang)?;
            enroll(&cli.socket, target_user(user)?, label, accept_insecure_rgb, lang)
        }
        Cmd::Test { user } => {
            let user = target_user(user)?;
            let (status, message, data) = call(&cli.socket, Op::Test { user }, lang)?;
            let score = data
                .as_ref()
                .and_then(|d| d["stats"]["best_similarity"].as_f64())
                .map(|s| format!("{s:.3}"))
                .unwrap_or_else(|| "-".into());
            match status {
                AuthStatus::Success => println!("{}", tr(lang, "cli.test.ok", &[("score", &score)])),
                AuthStatus::Failure => {
                    let reason = data
                        .as_ref()
                        .and_then(|d| d["reason"].as_str())
                        .map(|r| tr(lang, &format!("auth.{}", reason_key(r)), &[]))
                        .unwrap_or_default();
                    println!(
                        "{}",
                        tr(lang, "cli.test.fail", &[("reason", &reason), ("score", &score)])
                    );
                }
                _ => println!("{message}"),
            }
            if let Some(d) = data {
                println!("{}", serde_json::to_string_pretty(&d["stats"])?);
            }
            Ok(())
        }
        Cmd::List { user } => {
            let (status, message, data) = call(&cli.socket, Op::List { user }, lang)?;
            if status != AuthStatus::Success {
                bail!("{message}");
            }
            let list = data.and_then(|d| d.as_array().cloned()).unwrap_or_default();
            if list.is_empty() {
                println!("{}", tr(lang, "cli.list.empty", &[]));
            }
            for e in list {
                println!(
                    "{:<24} {:<9} samples={:<3} blink={}",
                    e["user"].as_str().unwrap_or("?"),
                    e["camera_kind"].as_str().unwrap_or("?"),
                    e["samples"],
                    e["blink_calibrated"]
                );
            }
            Ok(())
        }
        Cmd::Remove { user } => {
            let (_, message, _) = call(
                &cli.socket,
                Op::Remove {
                    user: target_user(user)?,
                },
                lang,
            )?;
            println!("{message}");
            Ok(())
        }
        Cmd::Status => {
            let (_, _, data) = call(&cli.socket, Op::Status, lang)?;
            println!("{}", serde_json::to_string_pretty(&data.unwrap_or_default())?);
            Ok(())
        }
        Cmd::Pam { action } => {
            match action.as_str() {
                "status" => println!("{}", pam_status_text(lang)),
                a => {
                    require_root(lang)?;
                    pam_toggle(a == "enable", lang)?;
                }
            }
            Ok(())
        }
        Cmd::Ssh { action } => ssh(&action, lang),
        Cmd::Emitter { path, .. } => {
            require_root(lang)?;
            let src =
                std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let controls = camera::import_emitter_config(&src)?;
            let Some(c) = controls.first() else {
                bail!(
                    "{}",
                    tr(lang, "cli.emitter.none", &[("path", &path.display().to_string())])
                );
            };
            edit_config(&cli.config, |doc| {
                let mut t = toml_edit::InlineTable::new();
                t.insert("unit", (c.unit as i64).into());
                t.insert("selector", (c.selector as i64).into());
                let arr: toml_edit::Array = c.control.iter().map(|&b| b as i64).collect();
                t.insert("control", arr.into());
                doc["camera"]["emitter"] = toml_edit::value(t);
            })?;
            println!(
                "{}",
                tr(
                    lang,
                    "cli.emitter.imported",
                    &[("count", &controls.len().to_string())]
                )
            );
            restart_daemon();
            Ok(())
        }
    }
}

fn reason_key(r: &str) -> &str {
    match r {
        "no_face" | "not_recognized" | "no_blink" | "not_illuminated" | "multiple_faces" | "not_infrared" => {
            r
        }
        _ => "not_recognized",
    }
}

fn enroll(socket: &str, user: String, label: Option<String>, accept: bool, lang: &str) -> Result<()> {
    let (status, message, _) = call(
        socket,
        Op::Enroll {
            user,
            label,
            accept_insecure_rgb: accept,
        },
        lang,
    )?;
    match status {
        AuthStatus::Success => Ok(()),
        _ => bail!("{message}"),
    }
}

// ---------------------------------------------------------------------------
// doctor / setup

struct Detected {
    cameras: Vec<(
        camera::CameraInfo,
        CameraKind,
        Vec<String>,
        Option<camera::CaptureStats>,
    )>,
}

fn detect_cameras(cfg: &Config) -> Detected {
    let mut cameras = Vec::new();
    for info in camera::list_cameras() {
        let mut cam_cfg = cfg.camera.clone();
        cam_cfg.device = info.path.display().to_string();
        cam_cfg.expected_usb_id = None;
        // The emitter control is a vendor extension unit: never send it to a
        // colour camera.
        if info.kind == CameraKind::Rgb {
            cam_cfg.emitter = None;
        }
        let (kind, ev, stats) = match Camera::open(&cam_cfg) {
            Ok(mut cam) => {
                let stats = camera::capture_stats(&mut cam, 12).ok();
                let (k, e) = camera::classify(&info, stats.as_ref());
                (k, e, stats)
            }
            Err(e) => {
                let mut ev = info.evidence.clone();
                ev.push(format!("could not capture: {e:#}"));
                (info.kind, ev, None)
            }
        };
        cameras.push((info, kind, ev, stats));
    }
    Detected { cameras }
}

fn doctor(config: &Path, socket: &str, lang: &str) -> Result<()> {
    let cfg = Config::load(config)?;
    println!("{}\n", tr(lang, "cli.doctor.title", &[]));
    if !is_root() {
        println!("(!) {}\n", tr(lang, "cli.doctor.not_root", &[]));
    }
    let det = detect_cameras(&cfg);
    println!("{}", tr(lang, "cli.doctor.cameras", &[]));
    if det.cameras.is_empty() {
        println!("  {}", tr(lang, "cli.doctor.no_cameras", &[]));
    }
    for (info, kind, ev, stats) in &det.cameras {
        let stable = info
            .stable_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        println!(
            "  {} {:?} – {} [{}] {}",
            info.path.display(),
            kind,
            info.name,
            info.formats.join(","),
            info.usb_id.clone().unwrap_or_default()
        );
        if !stable.is_empty() {
            println!("      {stable}");
        }
        for e in ev {
            println!("      · {e}");
        }
        if let Some(st) = stats {
            println!("      · mean brightness {:.0}/255", st.mean_brightness);
            if *kind == CameraKind::Infrared && camera::emitter_looks_off(st) {
                println!("      (!) {}", tr(lang, "cli.doctor.emitter_dark", &[]));
            }
        }
    }
    let ir = det.cameras.iter().find(|(_, k, _, _)| *k == CameraKind::Infrared);
    println!();
    match ir {
        Some((info, _, _, _)) => println!(
            "✔ {}",
            tr(
                lang,
                "cli.doctor.ir_found",
                &[("name", &info.name), ("device", &info.path.display().to_string())]
            )
        ),
        None => {
            println!("✘ {}", tr(lang, "cli.doctor.no_ir", &[]));
            println!("\n{}\n", tr(lang, "cli.insecure_banner", &[]));
        }
    }
    if cfg.camera.emitter.is_some() {
        println!("✔ {}", tr(lang, "cli.doctor.emitter_configured", &[]));
    }
    println!(
        "  {}",
        tr(
            lang,
            "cli.doctor.policy",
            &[
                ("rgb", &cfg.rgb_permitted().to_string()),
                ("sensitive", &cfg.policy.sensitive_device.to_string())
            ]
        )
    );
    for m in [&cfg.models.detector, &cfg.models.recognizer] {
        if !m.exists() {
            println!(
                "✘ {}",
                tr(
                    lang,
                    "cli.doctor.models_missing",
                    &[("path", &m.display().to_string())]
                )
            );
        }
    }
    if camera::lid_closed() == Some(true) {
        println!("(!) {}", tr(lang, "cli.doctor.lid_closed", &[]));
    }
    match call(socket, Op::Status, lang) {
        Ok((_, _, Some(d))) => println!(
            "✔ {}",
            tr(
                lang,
                "cli.doctor.daemon_ok",
                &[("version", d["version"].as_str().unwrap_or("?"))]
            )
        ),
        Ok(_) => {}
        Err(e) => println!("✘ {e:#}"),
    }
    println!("  {}", pam_status_text(lang));
    let encrypted = call(socket, Op::Status, lang)
        .ok()
        .and_then(|(_, _, d)| d)
        .and_then(|d| d["template_encryption"].as_bool())
        .unwrap_or(false);
    println!(
        "  {}",
        tr(
            lang,
            if encrypted {
                "cli.doctor.encryption_on"
            } else {
                "cli.doctor.encryption_off"
            },
            &[]
        )
    );
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    if desktop.contains("gnome")
        || Path::new("/usr/sbin/gdm3").exists()
        || Path::new("/usr/sbin/gdm").exists()
    {
        println!("\n{}", tr(lang, "cli.desktop.gnome", &[]));
    }
    if desktop.contains("kde")
        || Path::new("/usr/bin/kscreenlocker_greet").exists()
        || Path::new("/usr/libexec/kscreenlocker_greet").exists()
    {
        println!("\n{}", tr(lang, "cli.desktop.kde", &[]));
    }
    Ok(())
}

fn setup(config: &Path, socket: &str, accept_rgb: bool, no_enroll: bool, lang: &str) -> Result<()> {
    require_root(lang)?;
    let cfg = Config::load(config)?;
    let det = detect_cameras(&cfg);
    let chosen = det
        .cameras
        .iter()
        .find(|(_, k, _, _)| *k == CameraKind::Infrared)
        .or_else(|| det.cameras.first())
        .context(tr(lang, "cli.doctor.no_cameras", &[]))?;
    let (info, kind, _, _) = chosen;
    let mut accept = accept_rgb;
    if *kind != CameraKind::Infrared {
        println!("\n{}\n", tr(lang, "cli.insecure_banner", &[]));
        if !cfg.rgb_permitted() {
            bail!("{}", tr(lang, "enroll.rgb_refused", &[]));
        }
        if !accept {
            print!("{}", tr(lang, "cli.setup.confirm_rgb", &[]));
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            let yes = tr(lang, "cli.setup.yes_word", &[]);
            if answer.trim() != yes && answer.trim() != "YES" {
                bail!("{}", tr(lang, "cli.setup.aborted", &[]));
            }
            accept = true;
        }
    }
    let device = info.stable_path.clone().unwrap_or_else(|| info.path.clone());
    println!(
        "{}",
        tr(
            lang,
            "cli.setup.camera",
            &[("device", &device.display().to_string())]
        )
    );
    edit_config(config, |doc| {
        doc["camera"]["device"] = toml_edit::value(device.display().to_string());
        match &info.usb_id {
            Some(id) => doc["camera"]["expected_usb_id"] = toml_edit::value(id.clone()),
            None => {
                if let Some(t) = doc["camera"].as_table_like_mut() {
                    t.remove("expected_usb_id");
                }
            }
        }
    })?;
    ensure_template_key(lang);
    enable_socket();
    restart_daemon();
    // Enable PAM only once a face is enrolled successfully.
    if !no_enroll {
        enroll(socket, target_user(None)?, None, accept, lang)?;
    }
    pam_toggle(true, lang)?;
    println!(
        "{}",
        tr(
            lang,
            "cli.setup.done",
            &[("services", &cfg.auth.services.join(", "))]
        )
    );
    Ok(())
}

const CREDSTORE: &str = "/etc/credstore.encrypted";

/// systemd major version (`systemctl --version`).
fn systemd_version() -> Option<u32> {
    let out = std::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Create the template encryption key and seal it to the TPM 2.0 chip as a
/// systemd credential. Skipped without TPM 2.0 or with systemd < 254
/// (`ImportCredential=` unavailable); templates then rely on file
/// permissions. The key is bound to the TPM only, not to PCRs, so firmware
/// or Secure Boot updates do not lock users out of face login.
fn ensure_template_key(lang: &str) {
    let path = Path::new(CREDSTORE).join(frogger_core::crypto::CREDENTIAL_NAME);
    if path.exists() {
        println!("{}", tr(lang, "cli.setup.tpm_on", &[]));
        return;
    }
    let tpm = std::process::Command::new("systemd-creds")
        .arg("has-tpm2")
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !tpm || systemd_version().unwrap_or(0) < 254 {
        println!("{}", tr(lang, "cli.setup.tpm_off", &[]));
        return;
    }
    let result = (|| -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(CREDSTORE)?;
        let key = frogger_core::crypto::TemplateKey::generate()?;
        let mut child = std::process::Command::new("systemd-creds")
            .args([
                "encrypt",
                "--name=frogger.key",
                "--with-key=tpm2",
                "--tpm2-pcrs=",
                "-",
            ])
            .arg(&path)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().context("stdin")?.write_all(key.bytes())?;
        if !child.wait()?.success() {
            let _ = std::fs::remove_file(&path);
            bail!("systemd-creds encrypt failed");
        }
        Ok(())
    })();
    match result {
        Ok(()) => println!("{}", tr(lang, "cli.setup.tpm_on", &[])),
        Err(e) => {
            eprintln!("frogger: {e:#}");
            println!("{}", tr(lang, "cli.setup.tpm_off", &[]));
        }
    }
}

/// Enable and start the socket (no-op when already enabled). Needed on
/// distributions whose packages do not enable services (Arch, Fedora presets).
fn enable_socket() {
    let _ = std::process::Command::new("systemctl")
        .args(["enable", "--now", "frogger.socket"])
        .status();
}

fn restart_daemon() {
    // Pick up the new configuration; harmless if systemd is absent.
    let _ = std::process::Command::new("systemctl")
        .args(["try-restart", "frogger.service"])
        .status();
}

/// Edit the config file preserving comments and layout.
fn edit_config(path: &Path, f: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
    // A missing config starts from the documented example shipped by the package.
    let src = std::fs::read_to_string(path)
        .or_else(|_| std::fs::read_to_string(CONFIG_EXAMPLE))
        .unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = src
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    if !doc.contains_key("camera") {
        doc["camera"] = toml_edit::table();
    }
    f(&mut doc);
    let out = doc.to_string();
    // Refuse to write something the daemon would reject.
    let parsed: Config = toml_edit::de::from_str(&out).context("resulting configuration is invalid")?;
    parsed.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PAM integration

fn pam_enabled() -> bool {
    [
        "/etc/pam.d/common-auth",
        "/etc/pam.d/system-auth",
        "/etc/pam.d/sudo",
    ]
    .iter()
    .any(|p| {
        std::fs::read_to_string(p)
            .map(|s| {
                s.lines()
                    .any(|l| !l.trim_start().starts_with('#') && l.contains("pam_frogger.so"))
            })
            .unwrap_or(false)
    })
}

fn pam_status_text(lang: &str) -> String {
    if pam_enabled() {
        tr(lang, "cli.pam.status_on", &[])
    } else {
        tr(lang, "cli.pam.status_off", &[])
    }
}

fn pam_toggle(enable: bool, lang: &str) -> Result<()> {
    let has_pam_auth_update = Path::new("/usr/sbin/pam-auth-update").exists()
        && Path::new("/usr/share/pam-configs").join(PAM_PROFILE).exists();
    if has_pam_auth_update {
        let flag = if enable { "--enable" } else { "--remove" };
        let st = std::process::Command::new("/usr/sbin/pam-auth-update")
            .args(["--package", flag, PAM_PROFILE])
            .status()
            .context("running pam-auth-update")?;
        if !st.success() {
            bail!("pam-auth-update failed");
        }
        println!(
            "{}",
            tr(
                lang,
                if enable {
                    "cli.pam.enabled"
                } else {
                    "cli.pam.disabled"
                },
                &[]
            )
        );
        kde_face(enable)?;
    } else if Path::new("/usr/bin/authselect").exists() {
        println!("{}", tr(lang, "cli.pam.authselect", &[("line", PAM_LINE)]));
    } else {
        println!("{}", tr(lang, "cli.pam.manual", &[("line", PAM_LINE)]));
    }
    Ok(())
}

/// Plasma >= 6.8 runs a dedicated, non-interactive "kde-face" PAM service on
/// the lock screen. KDE ships no face recogniser itself – it only provides the
/// slot – so we install our stack there (only if nobody else owns the file).
fn kde_face(enable: bool) -> Result<()> {
    let path = Path::new(KDE_FACE);
    let ours = std::fs::read_to_string(path)
        .map(|s| s.contains("FROGGER"))
        .unwrap_or(false);
    if enable {
        if !path.exists() && Path::new(KDE_FACE_TEMPLATE).exists() {
            std::fs::copy(KDE_FACE_TEMPLATE, path)?;
        } else if path.exists() && !ours {
            eprintln!(
                "note: {KDE_FACE} exists and is managed by another package; add `{PAM_LINE}` there manually"
            );
        }
    } else if ours {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH key confirmation (per user, no root needed)

const ASKPASS_CANDIDATES: &[&str] = &["/usr/libexec/frogger-askpass", "/usr/lib/frogger/frogger-askpass"];
const SSH_ENV_FILE: &str = "60-frogger-askpass.conf";
/// ssh-agent units of Debian/Ubuntu/KDE (openssh) and GNOME (gcr).
const SSH_AGENT_UNITS: &[&str] = &["ssh-agent.service", "gcr-ssh-agent.service"];
const DROPIN_FILE: &str = "frogger-askpass.conf";

fn config_home() -> Result<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot determine the configuration directory")
}

/// Files written by `frogger ssh enable`.
fn ssh_files() -> Result<(PathBuf, Vec<PathBuf>)> {
    let base = config_home()?;
    let env = base.join("environment.d").join(SSH_ENV_FILE);
    let dropins = SSH_AGENT_UNITS
        .iter()
        .map(|u| base.join("systemd/user").join(format!("{u}.d")).join(DROPIN_FILE))
        .collect();
    Ok((env, dropins))
}

/// Keys added with `ssh-add -c` are confirmed with the face on every use.
///
/// * `SSH_ASKPASS` (environment.d) points ssh tools of the session at
///   frogger-askpass; it only takes over confirmations and hands passphrase
///   prompts to the previous askpass. Terminal prompts are unaffected.
/// * `SSH_ASKPASS_REQUIRE=force` is set for the agent services only
///   (systemd drop-ins), so ssh-agent asks even without an X11 `DISPLAY`.
fn ssh(action: &str, lang: &str) -> Result<()> {
    if is_root() {
        bail!("run `frogger ssh {action}` as the user whose ssh-agent should use it (without sudo)");
    }
    let (env_file, dropins) = ssh_files()?;
    match action {
        "enable" => {
            let askpass = ASKPASS_CANDIDATES
                .iter()
                .find(|p| Path::new(p).exists())
                .context(tr(lang, "cli.ssh.not_installed", &[]))?;
            let fallback = std::env::var("SSH_ASKPASS")
                .ok()
                .filter(|p| !p.is_empty() && !p.contains("frogger-askpass"));
            let mut env = String::from(
                "# FROGGER: confirm ssh-agent key use (ssh-add -c) with face authentication.\n\
                 # Remove with: frogger ssh disable\n",
            );
            env += &format!("SSH_ASKPASS={askpass}\n");
            let mut unit_env = format!("SSH_ASKPASS={askpass} SSH_ASKPASS_REQUIRE=force");
            if let Some(fb) = &fallback {
                env += &format!("FROGGER_ASKPASS_FALLBACK={fb}\n");
                unit_env += &format!(" FROGGER_ASKPASS_FALLBACK={fb}");
            }
            std::fs::create_dir_all(env_file.parent().expect("has parent"))?;
            std::fs::write(&env_file, env)?;
            for d in &dropins {
                std::fs::create_dir_all(d.parent().expect("has parent"))?;
                std::fs::write(
                    d,
                    format!("# FROGGER: face confirmation for ssh-add -c keys\n[Service]\nEnvironment={unit_env}\n"),
                )?;
            }
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .stderr(std::process::Stdio::null())
                .status();
            println!("{}", tr(lang, "cli.ssh.enabled", &[]));
        }
        "disable" => {
            for f in std::iter::once(&env_file).chain(dropins.iter()) {
                match std::fs::remove_file(f) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .stderr(std::process::Stdio::null())
                .status();
            println!("{}", tr(lang, "cli.ssh.disabled", &[]));
        }
        _ => {
            let on = env_file.exists();
            println!(
                "{}",
                tr(
                    lang,
                    if on {
                        "cli.ssh.status_on"
                    } else {
                        "cli.ssh.status_off"
                    },
                    &[]
                )
            );
        }
    }
    Ok(())
}
