//! `frogger-askpass` – an `SSH_ASKPASS` program for ssh-agent.
//!
//! Keys added with `ssh-add -c` must be confirmed on every use; ssh-agent
//! then runs `$SSH_ASKPASS` with `SSH_ASKPASS_PROMPT=confirm`. We answer that
//! confirmation with local FROGGER face authentication (IR + blink, same
//! rules as everywhere else). The face is checked on the machine where the
//! person actually sits – the SSH server only ever sees a normal signature.
//!
//! Password prompts and notifications are not ours: they are handed to the
//! desktop's regular askpass program unchanged.
//!
//! Exit status: 0 = allow (prints "yes"), 1 = deny.

use frogger_proto::{AuthStatus, Event, Op, Request, MAX_LINE, SOCKET_PATH};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::Duration;

/// PAM-like service name under which the daemon applies its policy.
const SERVICE: &str = "frogger-ssh";

/// Regular askpass programs of the common desktops, tried in this order.
const FALLBACKS: &[&str] = &[
    "/usr/bin/ksshaskpass",
    "/usr/lib/openssh/gnome-ssh-askpass",
    "/usr/libexec/openssh/gnome-ssh-askpass",
    "/usr/lib/ssh/gnome-ssh-askpass",
    "/usr/libexec/seahorse/ssh-askpass",
    "/usr/lib/seahorse/ssh-askpass",
    "/usr/lib/ssh/ssh-askpass",
    "/usr/bin/ssh-askpass",
    "/usr/lib/ssh/x11-ssh-askpass",
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let prompt = args.join(" ");
    let kind = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
    if kind != "confirm" {
        delegate(&args);
    }
    let lang = frogger_i18n::detect();
    let allowed = confirm(&prompt, lang);
    if allowed {
        println!("yes");
        std::process::exit(0);
    }
    std::process::exit(1);
}

/// Hand password prompts / notifications to the desktop's askpass.
fn delegate(args: &[String]) -> ! {
    let configured = std::env::var("FROGGER_ASKPASS_FALLBACK")
        .ok()
        .filter(|p| !p.is_empty());
    let own = std::env::current_exe().ok();
    let candidate = configured
        .into_iter()
        .chain(FALLBACKS.iter().map(|s| s.to_string()))
        .find(|p| {
            let path = std::path::Path::new(p);
            path.exists() && own.as_deref() != Some(path)
        });
    match candidate {
        Some(p) => {
            let err = std::process::Command::new(&p).args(args).exec();
            eprintln!("frogger-askpass: cannot run {p}: {err}");
        }
        None => eprintln!("frogger-askpass: no askpass program for password prompts found"),
    }
    std::process::exit(1);
}

fn notify(summary: &str, body: &str) {
    // Desktop notification (GNOME/KDE/… via org.freedesktop.Notifications).
    let _ = std::process::Command::new("notify-send")
        .args([
            "--app-name=FROGGER",
            "--icon=camera-web",
            "--expire-time=6000",
            summary,
            body,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if std::io::stderr().is_terminal() {
        eprintln!("{summary} {body}");
    }
}

fn user_name() -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut res: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid buffers for the duration of the call.
    let r = unsafe { libc::getpwuid_r(libc::getuid(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) };
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

/// First line of ssh-agent's prompt names the key ("Allow use of key …?").
fn key_line(prompt: &str) -> &str {
    prompt.lines().next().unwrap_or("").trim()
}

fn confirm(prompt: &str, lang: &str) -> bool {
    let Some(user) = user_name() else { return false };
    notify(&frogger_i18n::tr(lang, "ssh.confirm", &[]), key_line(prompt));
    let socket = std::env::var("FROGGER_SOCKET").unwrap_or_else(|_| SOCKET_PATH.into());
    let mut req = Request::new(Op::Authenticate {
        user,
        service: SERVICE.into(),
        tty: None,
        rhost: None,
    });
    req.lang = Some(lang.to_string());
    let result = (|| -> Result<AuthStatus, String> {
        let mut s = UnixStream::connect(&socket).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        s.write_all(&frogger_proto::to_line(&req))
            .map_err(|e| e.to_string())?;
        let mut r = BufReader::new(s.take(16 * MAX_LINE as u64));
        loop {
            let mut line = Vec::new();
            if r.read_until(b'\n', &mut line).map_err(|e| e.to_string())? == 0 {
                return Err("daemon closed the connection".into());
            }
            match serde_json::from_slice::<Event>(&line).map_err(|e| e.to_string())? {
                Event::Info { .. } => {}
                Event::Result { status, .. } => return Ok(status),
                Event::Error { message } => return Err(message),
            }
        }
    })();
    let allowed = matches!(result, Ok(AuthStatus::Success));
    let key = if allowed { "ssh.allowed" } else { "ssh.denied" };
    notify(&frogger_i18n::tr(lang, key, &[]), key_line(prompt));
    if let Err(e) = result {
        eprintln!("frogger-askpass: {e}");
    }
    allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_line_is_first_line() {
        let p = "Allow use of key /home/a/.ssh/id_ed25519?\nKey fingerprint SHA256:abc.";
        assert_eq!(key_line(p), "Allow use of key /home/a/.ssh/id_ed25519?");
        assert_eq!(key_line(""), "");
    }
}
