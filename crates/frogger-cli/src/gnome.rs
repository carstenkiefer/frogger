//! `frogger gnome enable|disable|status`: face login next to the password
//! field via the GNOME Shell extension (package `frogger-gnome`).
//!
//! Enabling
//! * writes the PAM service `/etc/pam.d/gdm-frogger`, derived from the
//!   system's `gdm-password` (its account/session/password stacks, our auth
//!   stack), so it fits every distribution;
//! * moves face authentication from `gdm-password` to `gdm-frogger` in the
//!   configuration, so recognition does not run twice;
//! * enables the extension for the GDM login screen (dconf database `gdm`)
//!   and for the invoking user.

use crate::{edit_config, require_root, restart_daemon, target_user};
use anyhow::{bail, Context, Result};
use frogger_i18n::tr;
use std::path::Path;
use std::process::Command;

pub const UUID: &str = "frogger@carstenkiefer.github.io";
const EXTENSION_DIR: &str = "/usr/share/gnome-shell/extensions/frogger@carstenkiefer.github.io";
const SERVICE: &str = "gdm-frogger";
const PAM_FILE: &str = "/etc/pam.d/gdm-frogger";
const GDM_PASSWORD: &str = "/etc/pam.d/gdm-password";
const MARKER: &str = "# Written by `frogger gnome enable`";
const DCONF_DB_DIR: &str = "/etc/dconf/db/gdm.d";
const DCONF_KEYFILE: &str = "/etc/dconf/db/gdm.d/90-frogger";
const DCONF_PROFILES: [&str; 2] = ["/etc/dconf/profile/gdm", "/usr/share/dconf/profile/gdm"];

pub fn run(action: &str, config: &Path, lang: &str) -> Result<()> {
    match action {
        "enable" => enable(config, lang),
        "disable" => disable(config, lang),
        _ => status(config, lang),
    }
}

/// PAM service for the extension: our auth stack, everything else from the
/// system's `gdm-password` (Debian `@include common-*`, Fedora
/// `password-auth`, …).
pub fn pam_service(gdm_password: &str) -> String {
    let mut out = format!(
        "#%PAM-1.0\n{MARKER}: face authentication next to the password\n\
         # field (GNOME Shell extension {UUID}). The account, session and password\n\
         # stacks are copied from gdm-password.\n\
         auth    requisite                   pam_nologin.so\n\
         auth    required                    pam_succeed_if.so user != root quiet_success\n\
         auth    [success=done default=die]  pam_frogger.so\n"
    );
    for line in gdm_password.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut words = t.split_whitespace();
        let first = words.next().unwrap_or_default();
        let is_auth = if first == "@include" {
            words.next().is_some_and(|f| f.ends_with("-auth") || f == "auth")
        } else {
            first.trim_start_matches('-') == "auth"
        };
        if !is_auth {
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out
}

fn enable(config: &Path, lang: &str) -> Result<()> {
    require_root(lang)?;
    if !Path::new(EXTENSION_DIR).is_dir() {
        bail!("{}", tr(lang, "cli.gnome.not_installed", &[]));
    }
    let gdm_password = std::fs::read_to_string(GDM_PASSWORD)
        .with_context(|| format!("reading {GDM_PASSWORD} (is GDM installed?)"))?;
    if let Ok(existing) = std::fs::read_to_string(PAM_FILE) {
        if !existing.contains(MARKER) {
            bail!("{PAM_FILE} exists and was not written by FROGGER; remove it or adapt it manually");
        }
    }
    let tmp = format!("{PAM_FILE}.tmp");
    std::fs::write(&tmp, pam_service(&gdm_password))?;
    std::fs::rename(&tmp, PAM_FILE)?;

    edit_config(config, |doc| {
        set_list(doc, "services", &[SERVICE], &["gdm-password"]);
        set_list(doc, "login_services", &[SERVICE], &[]);
    })?;
    restart_daemon();

    let greeter = enable_for_greeter(true);
    let user = target_user(None).ok();
    let session = user.as_deref().map(|u| set_for_user(u, true)).unwrap_or(false);
    println!("{}", tr(lang, "cli.gnome.enabled", &[]));
    if !greeter {
        println!("(!) {}", tr(lang, "cli.gnome.greeter_manual", &[("uuid", UUID)]));
    }
    if !session {
        println!("(!) {}", tr(lang, "cli.gnome.user_manual", &[("uuid", UUID)]));
    }
    println!("{}", tr(lang, "cli.gnome.relogin", &[]));
    Ok(())
}

fn disable(config: &Path, lang: &str) -> Result<()> {
    require_root(lang)?;
    if std::fs::read_to_string(PAM_FILE).is_ok_and(|s| s.contains(MARKER)) {
        std::fs::remove_file(PAM_FILE)?;
    }
    edit_config(config, |doc| {
        set_list(doc, "services", &["gdm-password"], &[SERVICE]);
        set_list(doc, "login_services", &[], &[SERVICE]);
    })?;
    restart_daemon();
    if Path::new(DCONF_KEYFILE).exists() {
        std::fs::remove_file(DCONF_KEYFILE)?;
        let _ = Command::new("dconf").arg("update").status();
    }
    if let Ok(user) = target_user(None) {
        set_for_user(&user, false);
    }
    println!("{}", tr(lang, "cli.gnome.disabled", &[]));
    Ok(())
}

fn status(config: &Path, lang: &str) -> Result<()> {
    let cfg = frogger_core::config::Config::load(config)?;
    let yes_no = |b: bool| if b { "✔" } else { "✘" };
    println!(
        "{}",
        tr(
            lang,
            "cli.gnome.status",
            &[
                ("installed", yes_no(Path::new(EXTENSION_DIR).is_dir())),
                ("pam", yes_no(Path::new(PAM_FILE).exists())),
                (
                    "service",
                    yes_no(cfg.service_allowed(SERVICE) && !cfg.service_allowed("gdm-password"))
                ),
                ("greeter", yes_no(Path::new(DCONF_KEYFILE).exists())),
            ]
        )
    );
    Ok(())
}

/// Add `add` to and remove `remove` from the list `auth.<key>` (an empty
/// list means "all services" and is left alone).
fn set_list(doc: &mut toml_edit::DocumentMut, key: &str, add: &[&str], remove: &[&str]) {
    if !doc.contains_key("auth") {
        doc["auth"] = toml_edit::table();
    }
    let defaults = frogger_core::config::AuthConfig::default();
    let current: Vec<String> = doc["auth"]
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_else(|| match key {
            "services" => defaults.services.clone(),
            _ => defaults.login_services.clone(),
        });
    if key == "services" && current.is_empty() {
        return;
    }
    let mut list: Vec<String> = current
        .into_iter()
        .filter(|s| !remove.contains(&s.as_str()))
        .collect();
    for a in add {
        if !list.iter().any(|s| s == a) {
            list.push((*a).to_string());
        }
    }
    let arr: toml_edit::Array = list.iter().map(String::as_str).collect();
    doc["auth"][key] = toml_edit::value(arr);
}

/// Enable (or not) the extension on the GDM login screen through the dconf
/// database of the greeter. Returns false when the distribution has no
/// `gdm` dconf profile with a system database.
fn enable_for_greeter(on: bool) -> bool {
    let has_db = DCONF_PROFILES
        .iter()
        .any(|p| std::fs::read_to_string(p).is_ok_and(|s| s.contains("system-db:gdm")));
    if !has_db {
        return false;
    }
    let ok = if on {
        std::fs::create_dir_all(DCONF_DB_DIR).is_ok()
            && std::fs::write(
                DCONF_KEYFILE,
                format!(
                    "# Written by `frogger gnome enable`\n[org/gnome/shell]\nenabled-extensions=['{UUID}']\n"
                ),
            )
            .is_ok()
    } else {
        std::fs::remove_file(DCONF_KEYFILE).is_ok()
    };
    ok && Command::new("dconf")
        .arg("update")
        .status()
        .is_ok_and(|s| s.success())
}

/// Add the extension to (or remove it from) `enabled-extensions` of `user`,
/// through the user's session bus. Returns false when that is not possible
/// (no session, gsettings missing).
fn set_for_user(user: &str, on: bool) -> bool {
    let Some(uid) = uid_of(user) else { return false };
    let bus = format!("unix:path=/run/user/{uid}/bus");
    if !Path::new(&format!("/run/user/{uid}/bus")).exists() {
        return false;
    }
    let gsettings = |args: &[&str]| {
        Command::new("runuser")
            .args(["-u", user, "--", "env"])
            .arg(format!("DBUS_SESSION_BUS_ADDRESS={bus}"))
            .arg("gsettings")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let Some(current) = gsettings(&["get", "org.gnome.shell", "enabled-extensions"]) else {
        return false;
    };
    let mut list = parse_strv(&current);
    list.retain(|u| u != UUID);
    if on {
        list.push(UUID.to_string());
    }
    let value = format!(
        "[{}]",
        list.iter()
            .map(|u| format!("'{u}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    gsettings(&["set", "org.gnome.shell", "enabled-extensions", &value]).is_some()
}

/// Parse a GVariant string array as printed by gsettings: `['a', 'b']` or
/// `@as []`.
fn parse_strv(s: &str) -> Vec<String> {
    s.split('\'').skip(1).step_by(2).map(String::from).collect()
}

fn uid_of(user: &str) -> Option<u32> {
    let name = std::ffi::CString::new(user).ok()?;
    // SAFETY: getpwnam returns NULL or a pointer to static storage; the uid
    // is copied immediately (the CLI is single-threaded here).
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        (!pw.is_null()).then(|| (*pw).pw_uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBIAN: &str = "#%PAM-1.0
auth    requisite       pam_nologin.so
auth    required        pam_succeed_if.so user != root quiet_success
@include common-auth
auth    optional        pam_gnome_keyring.so
@include common-account
# SELinux needs to be the first session rule.
session [success=ok ignore=ignore module_unknown=ignore default=bad]        pam_selinux.so close
session required        pam_loginuid.so
@include common-session
session optional        pam_gnome_keyring.so auto_start
@include common-password
";

    const FEDORA: &str = "auth     [success=done ignore=ignore default=bad] pam_selinux_permit.so
auth        substack      password-auth
auth        optional      pam_gnome_keyring.so
auth        include       postlogin

account     required      pam_nologin.so
account     include       password-auth

password    substack       password-auth
-password   optional       pam_gnome_keyring.so use_authtok

session     required      pam_selinux.so close
session     include       password-auth
-session    optional      pam_gnome_keyring.so auto_start
session     include       postlogin
";

    fn auth_lines(s: &str) -> Vec<&str> {
        s.lines()
            .filter(|l| l.starts_with("auth") || l.starts_with("-auth") || l.contains("common-auth"))
            .collect()
    }

    #[test]
    fn debian_service_keeps_account_and_session_but_only_our_auth() {
        let s = pam_service(DEBIAN);
        assert_eq!(auth_lines(&s).len(), 3);
        assert!(s.contains("pam_frogger.so"));
        assert!(!s.contains("common-auth"));
        for kept in [
            "@include common-account",
            "@include common-session",
            "@include common-password",
            "pam_loginuid.so",
        ] {
            assert!(s.contains(kept), "{kept} missing:\n{s}");
        }
    }

    #[test]
    fn fedora_service_drops_password_auth_substack() {
        let s = pam_service(FEDORA);
        assert_eq!(auth_lines(&s).len(), 3);
        assert!(!s.contains("substack      password-auth\nauth"));
        assert!(s.contains("account     include       password-auth"));
        assert!(s.contains("-session    optional      pam_gnome_keyring.so auto_start"));
        assert!(!s.contains("pam_selinux_permit.so"));
    }

    #[test]
    fn service_lists_are_edited_in_place() {
        let mut doc: toml_edit::DocumentMut = "[auth]\nservices = [\"sudo\", \"gdm-password\"]\n"
            .parse()
            .unwrap();
        set_list(&mut doc, "services", &[SERVICE], &["gdm-password"]);
        set_list(&mut doc, "login_services", &[SERVICE], &[]);
        let s = doc.to_string();
        assert!(s.contains("services = [\"sudo\", \"gdm-frogger\"]"), "{s}");
        assert!(s.contains("login_services = ["), "{s}");
        // "All services" (empty list) stays as it is.
        let mut doc: toml_edit::DocumentMut = "[auth]\nservices = []\n".parse().unwrap();
        set_list(&mut doc, "services", &[SERVICE], &["gdm-password"]);
        assert!(doc.to_string().contains("services = []"));
    }

    #[test]
    fn gsettings_lists_are_parsed() {
        assert_eq!(parse_strv("@as []"), Vec::<String>::new());
        assert_eq!(parse_strv("['a@b', 'c@d']\n"), vec!["a@b", "c@d"]);
    }
}
