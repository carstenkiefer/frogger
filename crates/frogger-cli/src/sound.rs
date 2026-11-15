//! Audible cues during enrollment: with closed eyes the terminal cannot be
//! read, so the important moments are signalled with system sounds.
//!
//! `frogger enroll` runs as root, but the audio server (PipeWire/PulseAudio)
//! belongs to the desktop session of the invoking user. The sound is
//! therefore played by a short-lived player process running as that user.

use frogger_proto::Cue;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Cleared by `--no-sound`.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

const SOUND_DIR: &str = "/usr/share/sounds/freedesktop/stereo";
const PLAYERS: [&str; 3] = ["pw-play", "paplay", "canberra-gtk-play"];

/// Sound of the freedesktop sound theme for a cue.
fn sound_name(cue: Cue) -> &'static str {
    match cue {
        Cue::Step => "message",
        Cue::EyesClosed => "complete",
        Cue::Done => "service-login",
        Cue::Failed => "dialog-warning",
    }
}

/// Play the sound for `cue` without blocking; falls back to the terminal
/// bell when no sound theme, player or user session is available.
pub fn play(cue: Cue) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let file = Path::new(SOUND_DIR).join(format!("{}.oga", sound_name(cue)));
    if !(file.is_file() && spawn_player(&file)) {
        bell();
    }
}

fn bell() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// The user whose desktop session should hear the sound: the one who ran
/// `sudo` / `pkexec`, otherwise the current user.
fn session_user() -> (u32, u32) {
    let env_id = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    // SAFETY: plain libc getters without arguments.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    match env_id("SUDO_UID").or_else(|| env_id("PKEXEC_UID")) {
        Some(u) => (u, env_id("SUDO_GID").or_else(|| primary_gid(u)).unwrap_or(u)),
        None => (uid, gid),
    }
}

fn primary_gid(uid: u32) -> Option<u32> {
    // SAFETY: getpwuid returns a pointer into static storage or NULL; the
    // field is copied immediately (single-threaded use in the CLI).
    unsafe {
        let pw = libc::getpwuid(uid);
        (!pw.is_null()).then(|| (*pw).pw_gid)
    }
}

fn spawn_player(file: &Path) -> bool {
    let (uid, gid) = session_user();
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    if !runtime.is_dir() {
        return false; // no session of that user, nobody would hear it
    }
    // SAFETY: plain libc getter.
    let root = unsafe { libc::geteuid() } == 0;
    for player in PLAYERS {
        let mut cmd = Command::new(player);
        if player == "canberra-gtk-play" {
            cmd.arg("-f");
        }
        cmd.arg(file)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if root && uid != 0 {
            // Drops root (and its supplementary groups) for the player only.
            cmd.uid(uid).gid(gid);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cue_has_a_distinct_purpose_sound() {
        let names = [Cue::Step, Cue::EyesClosed, Cue::Done, Cue::Failed].map(sound_name);
        assert_eq!(names.iter().collect::<std::collections::HashSet<_>>().len(), 4);
    }

    #[test]
    fn disabled_plays_nothing() {
        ENABLED.store(false, Ordering::Relaxed);
        play(Cue::Done); // must return without spawning or ringing
        ENABLED.store(true, Ordering::Relaxed);
    }
}
