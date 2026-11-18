//! `pam_frogger.so` – thin PAM client of `froggerd`.
//!
//! The module never looks at camera data itself. It forwards the request to
//! the daemon, relays progress messages through the PAM conversation and maps
//! the result to a PAM return code:
//!
//! | daemon result        | PAM code               |
//! |----------------------|------------------------|
//! | success              | `PAM_SUCCESS`          |
//! | failure / locked     | `PAM_AUTH_ERR`         |
//! | unavailable / error  | `PAM_AUTHINFO_UNAVAIL` |
//!
//! Recommended stack line (Debian `pam-auth-update` does this for you):
//! `auth [success=end default=ignore] pam_frogger.so`
//!
//! Module arguments: `debug`, `silent`, `socket=<path>`, `timeout=<secs>`.

#![allow(clippy::missing_safety_doc)]

use frogger_proto::{AuthStatus, Event, Op, Request, MAX_LINE, SOCKET_PATH};
use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const PAM_SUCCESS: c_int = 0;
pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_AUTHINFO_UNAVAIL: c_int = 9;
pub const PAM_IGNORE: c_int = 25;
const PAM_SILENT: c_int = 0x8000;

const PAM_SERVICE: c_int = 1;
const PAM_TTY: c_int = 3;
const PAM_RHOST: c_int = 4;
const PAM_CONV: c_int = 5;

const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[allow(non_camel_case_types)]
type pam_handle_t = c_void;

extern "C" {
    fn pam_get_user(pamh: *mut pam_handle_t, user: *mut *const c_char, prompt: *const c_char) -> c_int;
    fn pam_get_item(pamh: *const pam_handle_t, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_getenv(pamh: *mut pam_handle_t, name: *const c_char) -> *const c_char;
}

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub debug: bool,
    pub silent: bool,
    pub socket: Option<String>,
    pub timeout_secs: Option<u64>,
}

pub fn parse_args<'a>(args: impl IntoIterator<Item = &'a str>) -> Args {
    let mut a = Args::default();
    for arg in args {
        match arg {
            "debug" => a.debug = true,
            "silent" => a.silent = true,
            _ => {
                if let Some(v) = arg.strip_prefix("socket=") {
                    a.socket = Some(v.to_string());
                } else if let Some(v) = arg.strip_prefix("timeout=") {
                    a.timeout_secs = v.parse().ok();
                }
            }
        }
    }
    a
}

struct Handle {
    pamh: *mut pam_handle_t,
    silent: bool,
}

impl Handle {
    unsafe fn item_str(&self, item: c_int) -> Option<String> {
        let mut p: *const c_void = std::ptr::null();
        if pam_get_item(self.pamh, item, &mut p) != PAM_SUCCESS || p.is_null() {
            return None;
        }
        Some(CStr::from_ptr(p as *const c_char).to_string_lossy().into_owned())
    }

    unsafe fn user(&self) -> Option<String> {
        let mut p: *const c_char = std::ptr::null();
        if pam_get_user(self.pamh, &mut p, std::ptr::null()) != PAM_SUCCESS || p.is_null() {
            return None;
        }
        Some(CStr::from_ptr(p).to_string_lossy().into_owned())
    }

    unsafe fn env(&self, name: &str) -> Option<String> {
        let n = CString::new(name).ok()?;
        let p = pam_getenv(self.pamh, n.as_ptr());
        if p.is_null() {
            std::env::var(name).ok()
        } else {
            Some(CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }

    /// Show a message through the application's conversation function.
    unsafe fn say(&self, style: c_int, text: &str) {
        if self.silent || text.is_empty() {
            return;
        }
        let mut conv: *const c_void = std::ptr::null();
        if pam_get_item(self.pamh, PAM_CONV, &mut conv) != PAM_SUCCESS || conv.is_null() {
            return;
        }
        let conv = &*(conv as *const PamConv);
        let Some(f) = conv.conv else { return };
        let Ok(c) = CString::new(text.replace('\0', "")) else {
            return;
        };
        let msg = PamMessage {
            msg_style: style,
            msg: c.as_ptr(),
        };
        let mut msgs: [*const PamMessage; 1] = [&msg];
        let mut resp: *mut PamResponse = std::ptr::null_mut();
        f(1, msgs.as_mut_ptr(), &mut resp, conv.appdata_ptr);
        if !resp.is_null() {
            if !(*resp).resp.is_null() {
                libc::free((*resp).resp as *mut c_void);
            }
            libc::free(resp as *mut c_void);
        }
    }
}

fn log(debug: bool, msg: &str) {
    if !debug {
        return;
    }
    if let Ok(c) = CString::new(format!("pam_frogger: {msg}")) {
        // SAFETY: constant format string, valid C string argument.
        unsafe { libc::syslog(libc::LOG_AUTHPRIV | libc::LOG_DEBUG, c"%s".as_ptr(), c.as_ptr()) };
    }
}

/// True when the process environment indicates an SSH login. This is only a
/// cheap pre-filter; the daemon performs the authoritative logind check.
fn looks_remote(rhost: Option<&str>) -> bool {
    rhost.is_some_and(|h| !h.is_empty() && h != "localhost")
        || ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|v| std::env::var_os(v).is_some())
}

/// Signals that mean "the user wants to abort" (Ctrl+C, Ctrl+\\, the
/// terminal closed, kill). sudo blocks SIGINT/SIGQUIT while PAM runs, so a
/// Ctrl+C stays pending instead of interrupting us.
const ABORT_SIGNALS: [c_int; 4] = [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP];
/// How often a waiting module checks for pending abort signals.
const SIGNAL_POLL: Duration = Duration::from_millis(100);

/// Whether an abort signal is pending for this thread or process. The
/// signal stays pending: the application handles it after we return.
fn abort_signal_pending() -> bool {
    // SAFETY: sigpending fills a local, zero-initialised set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        if libc::sigpending(&mut set) != 0 {
            return false;
        }
        ABORT_SIGNALS.iter().any(|&sig| libc::sigismember(&set, sig) == 1)
    }
}

/// Talk to the daemon; returns the terminal event. Returns early (closing
/// the connection, which makes the daemon stop the camera) when the user
/// aborts with Ctrl+C or the overall `timeout` passes.
fn converse(
    socket: &str,
    req: &Request,
    timeout: Duration,
    mut on_info: impl FnMut(&str),
) -> Result<(AuthStatus, String), String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut s = UnixStream::connect(socket).map_err(|e| format!("connect {socket}: {e}"))?;
    s.set_read_timeout(Some(SIGNAL_POLL)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    s.write_all(&frogger_proto::to_line(req))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(s.take(16 * MAX_LINE as u64));
    let mut line = Vec::new();
    loop {
        // On a read timeout, bytes read so far stay in `line`.
        let n = match reader.read_until(b'\n', &mut line) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if abort_signal_pending() {
                    return Err("aborted by the user (signal pending)".into());
                }
                if std::time::Instant::now() >= deadline {
                    return Err("timeout waiting for the daemon".into());
                }
                if line.len() > MAX_LINE {
                    return Err("oversized message".into());
                }
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        if n == 0 {
            return Err("daemon closed connection".into());
        }
        if line.len() > MAX_LINE {
            return Err("oversized message".into());
        }
        let event = serde_json::from_slice::<Event>(&line).map_err(|e| e.to_string())?;
        line.clear();
        match event {
            Event::Info { text } => on_info(&text),
            Event::Result { status, message, .. } => return Ok((status, message)),
            Event::Error { message } => return Err(message),
            Event::Cue { .. } | Event::Say { .. } => {} // enrollment only
        }
    }
}

unsafe fn authenticate(pamh: *mut pam_handle_t, flags: c_int, args: Args) -> c_int {
    let h = Handle {
        pamh,
        silent: args.silent || flags & PAM_SILENT != 0,
    };
    let Some(user) = h.user() else {
        return PAM_AUTHINFO_UNAVAIL;
    };
    let service = h.item_str(PAM_SERVICE).unwrap_or_default();
    let rhost = h.item_str(PAM_RHOST);
    if looks_remote(rhost.as_deref()) {
        log(
            args.debug,
            &format!("skipping remote login of {user} ({service})"),
        );
        return PAM_AUTHINFO_UNAVAIL;
    }
    let lang = frogger_i18n::detect_with(|k| h.env(k));
    let mut req = Request::new(Op::Authenticate {
        user: user.clone(),
        service: service.clone(),
        tty: h.item_str(PAM_TTY),
        rhost,
    });
    req.lang = Some(lang.to_string());
    let socket = args.socket.clone().unwrap_or_else(|| SOCKET_PATH.to_string());
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(30));
    let result = converse(&socket, &req, timeout, |text| h.say(PAM_TEXT_INFO, text));
    match result {
        Ok((AuthStatus::Success, _)) => {
            log(
                args.debug,
                &format!("face authentication succeeded for {user} ({service})"),
            );
            PAM_SUCCESS
        }
        Ok((AuthStatus::Failure, _)) => PAM_AUTH_ERR,
        Ok((AuthStatus::Locked, msg)) => {
            h.say(PAM_ERROR_MSG, &msg);
            PAM_AUTH_ERR
        }
        Ok((status, msg)) => {
            log(
                args.debug,
                &format!("unavailable for {user} ({service}): {status:?} {msg}"),
            );
            PAM_AUTHINFO_UNAVAIL
        }
        Err(e) => {
            log(args.debug, &format!("daemon error: {e}"));
            PAM_AUTHINFO_UNAVAIL
        }
    }
}

unsafe fn collect_args(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    (0..argc.max(0) as usize)
        .filter_map(|i| {
            let p = *argv.add(i);
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        })
        .collect()
}

/// # Safety
/// Called by libpam with a valid handle and argument vector.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_authenticate(
    pamh: *mut pam_handle_t,
    flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    // Never let a panic unwind into the host process (sudo, gdm, sddm…).
    std::panic::catch_unwind(|| {
        let raw = collect_args(argc, argv);
        let args = parse_args(raw.iter().map(String::as_str));
        authenticate(pamh, flags, args)
    })
    .unwrap_or(PAM_AUTHINFO_UNAVAIL)
}

/// # Safety
/// Called by libpam.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_setcred(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// Account management is not our business; let other modules decide.
///
/// # Safety
/// Called by libpam.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_acct_mgmt(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_IGNORE
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn args() {
        let a = parse_args(["debug", "timeout=7", "socket=/tmp/x"]);
        assert!(a.debug && !a.silent);
        assert_eq!(a.timeout_secs, Some(7));
        assert_eq!(a.socket.as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn remote_detection() {
        assert!(looks_remote(Some("10.0.0.1")));
        assert!(!looks_remote(Some("localhost")) || std::env::var_os("SSH_CONNECTION").is_some());
    }

    fn fake_daemon(events: Vec<Event>) -> (String, std::thread::JoinHandle<Request>) {
        let path = std::env::temp_dir().join(format!(
            "frogger-pam-{}-{}.sock",
            std::process::id(),
            events.len()
        ));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        let p = path.to_string_lossy().into_owned();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = Vec::new();
            BufReader::new(&s).read_until(b'\n', &mut line).unwrap();
            let mut s2 = &s;
            for e in events {
                s2.write_all(&frogger_proto::to_line(&e)).unwrap();
            }
            serde_json::from_slice(&line).unwrap()
        });
        (p, h)
    }

    #[test]
    fn relays_info_and_result() {
        let (sock, h) = fake_daemon(vec![
            Event::info("look at the camera"),
            Event::info("blink"),
            Event::result(AuthStatus::Success, ""),
        ]);
        let req = Request::new(Op::Authenticate {
            user: "alice".into(),
            service: "sudo".into(),
            tty: None,
            rhost: None,
        });
        let mut infos = Vec::new();
        let r = converse(&sock, &req, Duration::from_secs(5), |t| infos.push(t.to_string())).unwrap();
        assert_eq!(r.0, AuthStatus::Success);
        assert_eq!(infos, vec!["look at the camera", "blink"]);
        let got = h.join().unwrap();
        assert!(matches!(got.op, Op::Authenticate { .. }));
    }

    #[test]
    fn ctrl_c_while_sudo_blocks_sigint_ends_the_conversation() {
        // A daemon that never answers; reports when the client hung up.
        let path = std::env::temp_dir().join(format!("frogger-pam-{}-silent.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        let daemon = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut rest = Vec::new();
            // Returns at EOF, i.e. when the module closed the connection.
            let _ = (&s).read_to_end(&mut rest);
        });
        // Like sudo: SIGINT blocked, then the user presses Ctrl+C.
        // SAFETY: signal mask and pending signal of this test thread only.
        let (old, sigint) = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            libc::pthread_kill(libc::pthread_self(), libc::SIGINT);
            (old, set)
        };
        let req = Request::new(Op::Status);
        let started = std::time::Instant::now();
        let r = converse(&path.to_string_lossy(), &req, Duration::from_secs(10), |_| {});
        assert!(r.unwrap_err().contains("aborted"));
        assert!(started.elapsed() < Duration::from_secs(2));
        daemon.join().unwrap(); // the connection was closed
                                // SAFETY: consume the pending SIGINT, restore the mask.
        unsafe {
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            libc::sigtimedwait(&sigint, std::ptr::null_mut(), &zero);
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_daemon_is_error() {
        let req = Request::new(Op::Status);
        assert!(converse("/nonexistent/frogger.sock", &req, Duration::from_secs(1), |_| {}).is_err());
    }
}
