//! `froggerd` – system daemon owning the camera, the models and the
//! biometric templates. Clients (`pam_frogger.so`, `frogger`) talk to it over
//! a Unix socket; caller identity is taken from `SO_PEERCRED`, never from the
//! request.

use anyhow::{Context, Result};
use frogger_core::camera;
use frogger_core::config::{Config, CONFIG_PATH, DATA_DIR};
use frogger_core::engine::{Engine, Outcome, Signal};
use frogger_core::session::{self, Roots};
use frogger_i18n::tr;
use frogger_proto::{
    is_valid_username, to_line, AuthStatus, Event, Op, Request, MAX_LINE, PROTOCOL_VERSION, SOCKET_PATH,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
struct Peer {
    pid: u32,
    uid: u32,
}

#[derive(Default)]
struct Lockout {
    failures: u32,
    locked_until: Option<Instant>,
}

struct Daemon {
    engine: Mutex<Engine>,
    lockouts: Mutex<HashMap<String, Lockout>>,
    roots: Roots,
}

fn main() {
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,tract=warn,tract_core=warn,tract_hir=warn,tract_onnx=warn"),
    )
    .format_timestamp(None)
    .init();
    if let Err(e) = run() {
        log::error!("{e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let config_path = std::env::var("FROGGER_CONFIG").unwrap_or_else(|_| CONFIG_PATH.into());
    let data_dir = std::env::var("FROGGER_DATA_DIR").unwrap_or_else(|_| DATA_DIR.into());
    let cfg = Config::load(Path::new(&config_path))?;
    let engine = Engine::new(cfg, Path::new(&data_dir)).context("opening face data")?;
    // Load the models while already accepting connections: policy answers
    // (e.g. "first login needs the password") must not wait for them.
    engine.preload_in_background();
    let daemon = Arc::new(Daemon {
        engine: Mutex::new(engine),
        lockouts: Mutex::default(),
        roots: Roots::default(),
    });

    let listener = listener()?;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let d = daemon.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle(&d, stream) {
                        log::debug!("connection ended: {e:#}");
                    }
                });
            }
            Err(e) => log::warn!("accept: {e}"),
        }
    }
    Ok(())
}

/// Socket from systemd (`frogger.socket`) or bound ourselves.
fn listener() -> Result<UnixListener> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    let fds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse::<i32>().ok())
        .unwrap_or(0);
    if pid_ok && fds >= 1 {
        log::info!("using socket from systemd");
        // SAFETY: fd 3 is handed over by systemd per sd_listen_fds(3).
        return Ok(unsafe { UnixListener::from_raw_fd(3) });
    }
    let path = std::env::var("FROGGER_SOCKET").unwrap_or_else(|_| SOCKET_PATH.into());
    let p = Path::new(&path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(p);
    let l = UnixListener::bind(p).with_context(|| format!("binding {path}"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o666))?;
    log::info!("listening on {path}");
    Ok(l)
}

/// `SO_PEERPIDFD` (Linux >= 6.5): a pidfd for the process that connected.
/// Same value on all architectures FROGGER supports (asm-generic).
const SO_PEERPIDFD: libc::c_int = 77;
/// `pidfd_send_signal` has the same number on x86_64 and aarch64.
const SYS_PIDFD_SEND_SIGNAL: libc::c_long = 424;

/// pidfd of the peer, if the kernel supports it.
fn peer_pidfd(s: &UnixStream) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid pointers to a properly sized int.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            &mut fd as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    // SAFETY: on success the kernel handed us a new, owned file descriptor.
    (r == 0 && fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// PID a pidfd refers to (from /proc/self/fdinfo), `None` once it exited.
fn pidfd_pid(fd: &std::os::fd::OwnedFd) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).ok()?;
    let pid: i64 = info
        .lines()
        .find_map(|l| l.strip_prefix("Pid:"))?
        .trim()
        .parse()
        .ok()?;
    (pid > 0).then_some(pid as u32)
}

/// Is the process behind the pidfd still alive? (signal 0 = existence check)
///
/// The daemon runs as the unprivileged `frogger` user and may not signal
/// most requesters (sudo, gdm, other users' processes): the kernel then
/// answers EPERM – which, as with kill(2), means the process exists. Only
/// ESRCH means it is gone.
fn pidfd_alive(fd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: plain syscall with a valid fd and null siginfo.
    let r = unsafe {
        libc::syscall(
            SYS_PIDFD_SEND_SIGNAL,
            fd.as_raw_fd(),
            0,
            std::ptr::null::<libc::c_void>(),
            0,
        )
    };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn peer_cred(s: &UnixStream) -> Result<Peer> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid pointers to a properly sized ucred.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    anyhow::ensure!(r == 0, "SO_PEERCRED failed: {}", std::io::Error::last_os_error());
    Ok(Peer {
        pid: cred.pid as u32,
        uid: cred.uid,
    })
}

fn uid_of(user: &str) -> Option<u32> {
    let name = std::ffi::CString::new(user).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: buffers are valid for the duration of the call.
    let r = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    (r == 0 && !result.is_null()).then_some(pwd.pw_uid)
}

struct Conn {
    stream: UnixStream,
}

impl Conn {
    fn send(&mut self, ev: &Event) {
        // The client may have gone away (PAM conversation aborted): ignore.
        let _ = self.stream.write_all(&to_line(ev));
    }
    fn info(&mut self, text: String) {
        self.send(&Event::info(text));
    }
    fn finish(&mut self, status: AuthStatus, message: impl Into<String>) {
        self.send(&Event::result(status, message));
    }
    fn finish_data(&mut self, status: AuthStatus, data: serde_json::Value) {
        self.send(&Event::Result {
            status,
            message: String::new(),
            data: Some(data),
        });
    }
}

/// Whether the client has closed its end of the connection (PAM gave up,
/// the password was typed, Ctrl+C). Non-blocking; cheap enough to ask once
/// per camera frame.
fn peer_gone(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: one valid pollfd, timeout 0 (never blocks).
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

fn read_request(stream: &UnixStream) -> Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_LINE as u64)).read_until(b'\n', &mut line)?;
    let req: Request = serde_json::from_slice(&line).context("malformed request")?;
    anyhow::ensure!(
        req.v == PROTOCOL_VERSION,
        "protocol version mismatch ({} != {PROTOCOL_VERSION})",
        req.v
    );
    Ok(req)
}

fn handle(d: &Daemon, stream: UnixStream) -> Result<()> {
    let peer = peer_cred(&stream)?;
    let req = match read_request(&stream) {
        Ok(r) => r,
        Err(e) => {
            let mut c = Conn { stream };
            c.send(&Event::Error {
                message: format!("{e:#}"),
            });
            return Ok(());
        }
    };
    let lang = req
        .lang
        .as_deref()
        .and_then(frogger_i18n::normalize)
        .unwrap_or("en");
    let mut c = Conn { stream };
    log::debug!("request {:?} from uid {} pid {}", req.op, peer.uid, peer.pid);

    // Root may act for anyone; other users only for themselves.
    let may_act_for = |user: &str| peer.uid == 0 || uid_of(user) == Some(peer.uid);

    match req.op {
        Op::Status => {
            let e = d.engine.lock().unwrap_or_else(|p| p.into_inner());
            let data = serde_json::json!({
                "version": frogger_proto::version(),
                "liveness": e.cfg.effective_liveness(),
                "allow_insecure_rgb": e.cfg.rgb_permitted(),
                "sensitive_device": e.cfg.policy.sensitive_device,
                "template_encryption": e.store.encrypted(),
                "camera": e.cfg.camera.device,
                "enrolled_users": if peer.uid == 0 { serde_json::json!(e.store.users()) } else { serde_json::Value::Null },
            });
            c.finish_data(AuthStatus::Success, data);
        }
        Op::Authenticate {
            user, service, rhost, ..
        } => {
            if !is_valid_username(&user) || !may_act_for(&user) {
                log::warn!("uid {} may not authenticate {user:?}", peer.uid);
                c.finish(AuthStatus::Denied, tr(lang, "cli.not_permitted", &[]));
                return Ok(());
            }
            let pidfd = peer_pidfd(&c.stream);
            authenticate(d, &mut c, peer, pidfd, &user, &service, rhost.as_deref(), lang);
        }
        Op::Enroll {
            user,
            label,
            accept_insecure_rgb,
        } => {
            if peer.uid != 0 {
                c.finish(AuthStatus::Denied, tr(lang, "cli.need_root", &[]));
                return Ok(());
            }
            if !is_valid_username(&user) || uid_of(&user).is_none() {
                c.finish(
                    AuthStatus::Denied,
                    tr(lang, "cli.unknown_user", &[("user", &user)]),
                );
                return Ok(());
            }
            let fd = c.stream.as_raw_fd();
            let cancel = || peer_gone(fd);
            let engine = d.engine.lock().unwrap_or_else(|p| p.into_inner());
            let conn = std::cell::RefCell::new(&mut c);
            let mut emit = |s: String| conn.borrow_mut().info(s);
            let mut cue = |sig: Signal| {
                conn.borrow_mut().send(&match sig {
                    Signal::Cue(k) => Event::Cue { cue: k },
                    Signal::Say(key) => Event::Say { key: key.into() },
                })
            };
            let outcome = engine.enroll(
                &user,
                label,
                accept_insecure_rgb,
                lang,
                &mut emit,
                &mut cue,
                &cancel,
            );
            drop(engine);
            log::info!("enrollment of {user}: {outcome:?}");
            send_outcome(&mut c, outcome);
        }
        Op::List { user } => {
            let engine = d.engine.lock().unwrap_or_else(|p| p.into_inner());
            let users: Vec<String> = match user {
                Some(u) if may_act_for(&u) => vec![u],
                Some(_) => {
                    c.finish(AuthStatus::Denied, tr(lang, "cli.not_permitted", &[]));
                    return Ok(());
                }
                None if peer.uid == 0 => engine.store.users(),
                None => {
                    c.finish(AuthStatus::Denied, tr(lang, "cli.need_root", &[]));
                    return Ok(());
                }
            };
            let list: Vec<serde_json::Value> = users
                .iter()
                .filter_map(|u| engine.store.load(u).ok().flatten())
                .map(|t| {
                    serde_json::json!({
                        "user": t.user, "camera_kind": t.camera_kind, "samples": t.samples.len(),
                        "blink_calibrated": t.eyes.is_some(), "label": t.label, "created": t.created,
                    })
                })
                .collect();
            c.finish_data(AuthStatus::Success, serde_json::json!(list));
        }
        Op::Remove { user } => {
            if !is_valid_username(&user) || !may_act_for(&user) {
                c.finish(AuthStatus::Denied, tr(lang, "cli.not_permitted", &[]));
                return Ok(());
            }
            let engine = d.engine.lock().unwrap_or_else(|p| p.into_inner());
            match engine.store.remove(&user) {
                Ok(true) => c.finish(AuthStatus::Success, tr(lang, "cli.removed", &[("user", &user)])),
                Ok(false) => c.finish(
                    AuthStatus::Unavailable,
                    tr(lang, "cli.not_enrolled", &[("user", &user)]),
                ),
                Err(e) => c.finish(AuthStatus::Unavailable, format!("{e:#}")),
            }
        }
        Op::Test { user } => {
            if !is_valid_username(&user) || !may_act_for(&user) {
                c.finish(AuthStatus::Denied, tr(lang, "cli.not_permitted", &[]));
                return Ok(());
            }
            let fd = c.stream.as_raw_fd();
            let engine = d.engine.lock().unwrap_or_else(|p| p.into_inner());
            let mut emit = |s: String| c.info(s);
            let r = engine.test(&user, lang, &mut emit, &|| peer_gone(fd));
            drop(engine);
            match r {
                Ok(report) => {
                    let status = if report.success {
                        AuthStatus::Success
                    } else {
                        AuthStatus::Failure
                    };
                    c.finish_data(status, serde_json::to_value(&report)?);
                }
                Err(e) => c.finish(AuthStatus::Unavailable, format!("{e:#}")),
            }
        }
    }
    Ok(())
}

/// Lock face authentication for `user` after too many failures.
fn lock_if_exhausted(l: &mut Lockout, cfg: &Config, user: &str) {
    if l.failures >= cfg.auth.max_failures {
        l.locked_until = Some(Instant::now() + Duration::from_secs(cfg.auth.lockout_secs));
        log::warn!(
            "face authentication for {user} locked for {}s",
            cfg.auth.lockout_secs
        );
    }
}

fn send_outcome(c: &mut Conn, o: Outcome) {
    match o {
        Outcome::Success => c.finish(AuthStatus::Success, ""),
        Outcome::Failure(m) => c.finish(AuthStatus::Failure, m),
        Outcome::Unavailable(m) => c.finish(AuthStatus::Unavailable, m),
        Outcome::Denied(m) => c.finish(AuthStatus::Denied, m),
        Outcome::Cancelled { .. } => {} // nobody is listening any more
    }
}

#[allow(clippy::too_many_arguments)]
fn authenticate(
    d: &Daemon,
    c: &mut Conn,
    peer: Peer,
    pidfd: Option<std::os::fd::OwnedFd>,
    user: &str,
    service: &str,
    rhost: Option<&str>,
    lang: &str,
) {
    let (cfg, enrolled) = {
        let e = d.engine.lock().unwrap_or_else(|p| p.into_inner());
        (e.cfg.clone(), matches!(e.store.load(user), Ok(Some(_))))
    };
    let unavailable = |c: &mut Conn, why: &str| {
        log::info!("face auth for {user} via {service} skipped: {why}");
        c.finish(AuthStatus::Unavailable, why.to_string());
    };
    if !enrolled {
        return unavailable(c, "not enrolled");
    }
    if !cfg.service_allowed(service) {
        return unavailable(c, "service not enabled for face authentication");
    }
    if rhost.is_some_and(|h| !h.is_empty() && h != "localhost") {
        return unavailable(c, "remote host set (PAM_RHOST)");
    }
    // With a pidfd the identity of the requesting process is pinned: if it
    // exits and its PID is reused while we inspect /proc/<pid>, the checks
    // below detect it (race-free on Linux >= 6.5, PID based before).
    if let Some(fd) = &pidfd {
        if pidfd_pid(fd) != Some(peer.pid) {
            return unavailable(c, "requesting process changed or exited");
        }
    }
    let locality = d.roots.locality(peer.pid, cfg.auth.deny_while_remote_session);
    if pidfd
        .as_ref()
        .is_some_and(|fd| !pidfd_alive(fd) || pidfd_pid(fd) != Some(peer.pid))
    {
        return unavailable(c, "requesting process exited during the locality check");
    }
    if !locality.is_local() {
        return unavailable(c, &format!("request is not local: {}", locality.reason()));
    }
    let uid = uid_of(user);
    if cfg.auth.require_password_for_first_login
        && cfg.is_login_service(service)
        && !uid
            .map(|u| session::has_running_session(&d.roots, u))
            .unwrap_or(false)
    {
        return unavailable(c, "first login requires the password (keyring)");
    }
    if cfg.auth.skip_when_lid_closed && camera::lid_closed() == Some(true) {
        return unavailable(c, "laptop lid closed");
    }
    {
        let mut locks = d.lockouts.lock().unwrap_or_else(|p| p.into_inner());
        let l = locks.entry(user.to_string()).or_default();
        if let Some(until) = l.locked_until {
            if Instant::now() < until {
                let minutes = (until - Instant::now()).as_secs().div_ceil(60).to_string();
                c.finish(
                    AuthStatus::Locked,
                    tr(lang, "auth.locked", &[("minutes", &minutes)]),
                );
                return;
            }
            *l = Lockout::default();
        }
    }

    let fd = c.stream.as_raw_fd();
    let engine = d.engine.lock().unwrap_or_else(|p| p.into_inner());
    // The client may have given up while another request held the engine.
    if peer_gone(fd) {
        log::info!("face auth for {user} via {service} skipped: client gone before the camera started");
        return;
    }
    let mut emit = |s: String| c.info(s);
    let outcome = engine.authenticate(user, service, lang, &mut emit, &|| peer_gone(fd));
    drop(engine);

    let mut locks = d.lockouts.lock().unwrap_or_else(|p| p.into_inner());
    let l = locks.entry(user.to_string()).or_default();
    match &outcome {
        Outcome::Success => {
            log::info!(
                "face authentication succeeded for {user} ({service}, {})",
                locality.reason()
            );
            *l = Lockout::default();
            c.info(tr(lang, "auth.success", &[]));
        }
        Outcome::Failure(m) => {
            l.failures += 1;
            log::warn!(
                "face authentication failed for {user} ({service}): {m} [{} consecutive]",
                l.failures
            );
            lock_if_exhausted(l, &cfg, user);
            c.info(m.clone());
        }
        Outcome::Cancelled { face_seen: true } => {
            l.failures += 1;
            log::info!(
                "face authentication for {user} ({service}) cancelled with a face in view: \
                 counted as failure [{} consecutive]",
                l.failures
            );
            lock_if_exhausted(l, &cfg, user);
        }
        _ => {}
    }
    drop(locks);
    send_outcome(c, outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_client_is_noticed() {
        let (daemon_end, client_end) = UnixStream::pair().unwrap();
        let fd = daemon_end.as_raw_fd();
        assert!(!peer_gone(fd), "open connection");
        // Events the daemon sent but the client never read do not matter.
        (&daemon_end).write_all(b"{}\n").unwrap();
        assert!(!peer_gone(fd), "unread data from the daemon");
        drop(client_end);
        assert!(peer_gone(fd), "client closed its end");
    }

    #[test]
    fn peer_pidfd_identifies_the_connecting_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let cred = peer_cred(&a).unwrap();
        assert_eq!(cred.pid, std::process::id());
        // Kernels before 6.5 have no SO_PEERPIDFD; the daemon then falls
        // back to PID-based checks.
        if let Some(fd) = peer_pidfd(&a) {
            assert_eq!(pidfd_pid(&fd), Some(std::process::id()));
            assert!(pidfd_alive(&fd));
        }
    }

    #[test]
    fn pidfd_of_process_we_may_not_signal_is_alive() {
        // Regression: froggerd runs as `frogger` and may not signal sudo or
        // gdm; EPERM must count as alive. PID 1 is owned by root – for a
        // non-root test run this exercises exactly that path.
        // SAFETY: pidfd_open(pid, 0) returns a new fd or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, 1 as libc::c_int, 0) };
        if raw < 0 {
            return; // no pidfd support or PID 1 not visible (container)
        }
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw as i32) };
        assert!(pidfd_alive(&fd));
        assert_eq!(pidfd_pid(&fd), Some(1));
    }

    #[test]
    fn pidfd_of_exited_process_is_detected() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        // SAFETY: pidfd_open(pid, 0) returns a new fd or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::c_int, 0) };
        if raw < 0 {
            child.wait().unwrap();
            return; // pidfd_open unsupported (kernel < 5.3)
        }
        let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw as i32) };
        child.wait().unwrap();
        assert!(!pidfd_alive(&fd));
        assert_eq!(pidfd_pid(&fd), None);
    }
}
