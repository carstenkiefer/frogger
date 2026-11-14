//! Is the authentication request coming from someone physically at this
//! machine?
//!
//! Face auth is *passive*: whoever sits in front of the camera authenticates.
//! Without this check a remote attacker with the user's SSH access could run
//! `sudo` while the legitimate user happens to look at the screen. We derive
//! locality from systemd-logind state for the *requesting process* (obtained
//! by the daemon via `SO_PEERCRED`, so it cannot be spoofed by the client).

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub uid: Option<u32>,
    pub remote: bool,
    pub seat: Option<String>,
    pub class: Option<String>,
    pub kind: Option<String>,
    pub active: bool,
    pub service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Locality {
    /// Request originates from a local seat (or a local display manager).
    Local(String),
    /// Request originates from a remote session or cannot be attributed to a
    /// local seat.
    Remote(String),
}

impl Locality {
    pub fn is_local(&self) -> bool {
        matches!(self, Locality::Local(_))
    }
    pub fn reason(&self) -> &str {
        match self {
            Locality::Local(r) | Locality::Remote(r) => r,
        }
    }
}

/// File system roots, overridable for tests.
#[derive(Clone, Debug)]
pub struct Roots {
    pub proc: PathBuf,
    pub run_systemd: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Roots {
            proc: "/proc".into(),
            run_systemd: "/run/systemd".into(),
        }
    }
}

const DISPLAY_MANAGER_UNITS: &[&str] = &[
    "gdm.service",
    "gdm3.service",
    "sddm.service",
    "lightdm.service",
    "plasmalogin.service",
    "display-manager.service",
    "greetd.service",
];

fn parse_env_file(s: &str) -> HashMap<String, String> {
    s.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

impl Roots {
    fn cgroup(&self, pid: u32) -> Option<String> {
        let s = std::fs::read_to_string(self.proc.join(pid.to_string()).join("cgroup")).ok()?;
        // cgroup v2: "0::/user.slice/…"; on hybrid systems take the systemd line.
        s.lines()
            .find(|l| l.starts_with("0::"))
            .or_else(|| s.lines().find(|l| l.contains("name=systemd")))
            .and_then(|l| l.splitn(3, ':').nth(2))
            .map(str::to_string)
    }

    /// Real uid of a process (not the effective one: for setuid `sudo`/`su`
    /// this is the human who started it).
    pub fn real_uid(&self, pid: u32) -> Option<u32> {
        let s = std::fs::read_to_string(self.proc.join(pid.to_string()).join("status")).ok()?;
        s.lines()
            .find(|l| l.starts_with("Uid:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    pub fn session(&self, id: &str) -> Option<SessionInfo> {
        if id.is_empty() || id.contains('/') || id.contains("..") {
            return None;
        }
        let s = std::fs::read_to_string(self.run_systemd.join("sessions").join(id)).ok()?;
        let m = parse_env_file(&s);
        Some(SessionInfo {
            id: id.to_string(),
            uid: m.get("UID").and_then(|v| v.parse().ok()),
            remote: m.get("REMOTE").map(|v| v == "1").unwrap_or(false)
                || m.get("REMOTE_HOST").map(|v| !v.is_empty()).unwrap_or(false),
            seat: m.get("SEAT").filter(|v| !v.is_empty()).cloned(),
            class: m.get("CLASS").cloned(),
            kind: m.get("TYPE").cloned(),
            active: m.get("ACTIVE").map(|v| v == "1").unwrap_or(false),
            service: m.get("SERVICE").cloned(),
        })
    }

    /// All logind sessions of `uid`.
    pub fn user_sessions(&self, uid: u32) -> Vec<SessionInfo> {
        let Ok(s) = std::fs::read_to_string(self.run_systemd.join("users").join(uid.to_string())) else {
            return Vec::new();
        };
        let m = parse_env_file(&s);
        m.get("SESSIONS")
            .map(|v| v.split_whitespace().filter_map(|id| self.session(id)).collect())
            .unwrap_or_default()
    }

    /// Session the process belongs to (from its cgroup path).
    pub fn session_of_pid(&self, pid: u32) -> Option<SessionInfo> {
        let cg = self.cgroup(pid)?;
        let scope = cg
            .split('/')
            .find(|c| c.starts_with("session-") && c.ends_with(".scope"))?;
        self.session(&scope["session-".len()..scope.len() - ".scope".len()])
    }

    fn in_unit(&self, pid: u32, pred: impl Fn(&str) -> bool) -> bool {
        self.cgroup(pid)
            .map(|cg| cg.split('/').any(&pred))
            .unwrap_or(false)
    }

    fn is_display_manager(&self, pid: u32) -> bool {
        self.in_unit(pid, |u| DISPLAY_MANAGER_UNITS.contains(&u))
    }

    fn all_sessions(&self) -> Vec<SessionInfo> {
        let Ok(rd) = std::fs::read_dir(self.run_systemd.join("sessions")) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| !n.contains('.'))
            .filter_map(|n| self.session(&n))
            .collect()
    }

    /// Decide whether a request from `pid` may use the camera.
    ///
    /// Rules:
    /// 1. If the process is inside a logind session, that session must be
    ///    local (not `REMOTE`).
    /// 2. Otherwise a root process of a display manager unit is local.
    /// 3. Otherwise (processes started by the user manager, e.g. terminal
    ///    emulators under GNOME/KDE) the *real* uid of the process must own a
    ///    local session on a seat.
    /// 4. With `deny_if_remote`, any remote session of the requesting user
    ///    makes rules 1/3 fail – this closes the `systemd-run --user` escape
    ///    from an SSH session scope.
    pub fn locality(&self, pid: u32, deny_if_remote: bool) -> Locality {
        let ruid = self.real_uid(pid);
        let requester_remote = |uid: Option<u32>| -> bool {
            deny_if_remote
                && uid
                    .map(|u| self.user_sessions(u).iter().any(|s| s.remote))
                    .unwrap_or(false)
        };
        if let Some(s) = self.session_of_pid(pid) {
            if s.remote {
                return Locality::Remote(format!("session {} is remote", s.id));
            }
            if requester_remote(ruid) {
                return Locality::Remote("requesting user also has a remote session".into());
            }
            return Locality::Local(format!("local session {}", s.id));
        }
        if ruid == Some(0) && self.is_display_manager(pid) {
            return Locality::Local("display manager".into());
        }
        // polkit >= 121 runs polkit-agent-helper as a socket-activated
        // service: the human behind the request is not visible to us. Only
        // allow it while nobody at all is logged in remotely.
        if ruid == Some(0) && self.in_unit(pid, |u| u.starts_with("polkit-agent-helper")) {
            let all = self.all_sessions();
            if all.iter().any(|s| s.remote) {
                return Locality::Remote("polkit helper while remote sessions exist".into());
            }
            if all.iter().any(|s| s.seat.is_some() && s.active) {
                return Locality::Local("polkit helper, only local sessions".into());
            }
            return Locality::Remote("polkit helper without an active local session".into());
        }
        let Some(uid) = ruid else {
            return Locality::Remote("cannot determine requesting user".into());
        };
        let sessions = self.user_sessions(uid);
        let local_seat = sessions.iter().any(|s| !s.remote && s.seat.is_some());
        if !local_seat {
            return Locality::Remote("requesting user has no local seat session".into());
        }
        if requester_remote(Some(uid)) {
            return Locality::Remote("requesting user also has a remote session".into());
        }
        Locality::Local("user owns a local seat session".into())
    }
}

/// Does `uid` already have a running graphical/console session? Used to
/// require the password on first login (keyring unlock).
pub fn has_running_session(roots: &Roots, uid: u32) -> bool {
    roots
        .user_sessions(uid)
        .iter()
        .any(|s| s.class.as_deref() == Some("user") && !s.remote)
}

pub fn path_exists(p: &Path) -> bool {
    p.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fake {
        dir: PathBuf,
        roots: Roots,
    }

    impl Fake {
        fn new(name: &str) -> Fake {
            let dir = std::env::temp_dir().join(format!("frogger-sess-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("proc")).unwrap();
            fs::create_dir_all(dir.join("run/sessions")).unwrap();
            fs::create_dir_all(dir.join("run/users")).unwrap();
            let roots = Roots {
                proc: dir.join("proc"),
                run_systemd: dir.join("run"),
            };
            Fake { dir, roots }
        }
        fn proc(&self, pid: u32, ruid: u32, cgroup: &str) {
            let p = self.dir.join("proc").join(pid.to_string());
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("cgroup"), format!("0::{cgroup}\n")).unwrap();
            fs::write(p.join("status"), format!("Name:\tx\nUid:\t{ruid}\t0\t0\t0\n")).unwrap();
        }
        fn session(&self, id: &str, uid: u32, remote: bool, seat: Option<&str>) {
            let mut s = format!("UID={uid}\nCLASS=user\nACTIVE=1\nREMOTE={}\n", remote as u8);
            if let Some(seat) = seat {
                s += &format!("SEAT={seat}\n");
            }
            fs::write(self.dir.join("run/sessions").join(id), s).unwrap();
        }
        fn user(&self, uid: u32, sessions: &str) {
            fs::write(
                self.dir.join("run/users").join(uid.to_string()),
                format!("SESSIONS=\"{sessions}\"\n"),
            )
            .unwrap();
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn ssh_session_is_remote() {
        let f = Fake::new("ssh");
        f.session("7", 1000, true, None);
        f.user(1000, "7");
        f.proc(42, 1000, "/user.slice/user-1000.slice/session-7.scope");
        assert!(!f.roots.locality(42, true).is_local());
    }

    #[test]
    fn terminal_under_user_manager_is_local() {
        let f = Fake::new("term");
        f.session("2", 1000, false, Some("seat0"));
        f.user(1000, "2");
        f.proc(
            42,
            1000,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-1.scope",
        );
        assert!(f.roots.locality(42, true).is_local());
    }

    #[test]
    fn systemd_run_escape_from_ssh_is_blocked() {
        let f = Fake::new("escape");
        f.session("2", 1000, false, Some("seat0"));
        f.session("9", 1000, true, None);
        f.user(1000, "2 9");
        f.proc(
            42,
            1000,
            "/user.slice/user-1000.slice/user@1000.service/app.slice/run-u1.scope",
        );
        assert!(!f.roots.locality(42, true).is_local());
        // Explicitly relaxed policy allows it.
        assert!(f.roots.locality(42, false).is_local());
    }

    #[test]
    fn other_user_without_seat_is_remote() {
        let f = Fake::new("other");
        f.session("3", 1001, true, None);
        f.user(1001, "3");
        // `su alice` started by uid 1001 via systemd-run --user: no session.
        f.proc(
            42,
            1001,
            "/user.slice/user-1001.slice/user@1001.service/app.slice/run-u5.scope",
        );
        assert!(!f.roots.locality(42, true).is_local());
    }

    #[test]
    fn display_manager_is_local() {
        let f = Fake::new("dm");
        f.proc(42, 0, "/system.slice/gdm.service");
        assert!(f.roots.locality(42, true).is_local());
        f.proc(43, 0, "/system.slice/cron.service");
        assert!(!f.roots.locality(43, true).is_local());
    }

    #[test]
    fn polkit_helper_service() {
        let f = Fake::new("polkit");
        f.session("2", 1000, false, Some("seat0"));
        f.user(1000, "2");
        f.proc(
            42,
            0,
            "/system.slice/system-polkit\\x2dagent\\x2dhelper.slice/polkit-agent-helper@1.service",
        );
        assert!(f.roots.locality(42, true).is_local());
        // Any SSH session on the machine (any user) blocks it.
        f.session("8", 1001, true, None);
        assert!(!f.roots.locality(42, true).is_local());
    }

    #[test]
    fn rejects_path_traversal_in_session_id() {
        let f = Fake::new("trav");
        assert!(f.roots.session("../users/1000").is_none());
    }
}
