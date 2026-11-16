//! Audible cues during enrollment: with closed eyes the terminal cannot be
//! read, so the important moments are signalled with system sounds, and the
//! prompts are spoken when a voice package (`frogger-voice-<lang>`) is
//! installed.
//!
//! `frogger enroll` runs as root, but the audio server (PipeWire/PulseAudio)
//! belongs to the desktop session of the invoking user. Sounds are therefore
//! played by short-lived player processes running as that user, one after
//! another from a queue so that clips never overlap.

use frogger_proto::Cue;
use std::collections::VecDeque;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cleared by `--no-sound` (system sounds and voice).
pub static ENABLED: AtomicBool = AtomicBool::new(true);

const SOUND_DIR: &str = "/usr/share/sounds/freedesktop/stereo";
/// Installed by the `frogger-voice-<lang>` packages: `<lang>/<key>.opus`.
pub const VOICE_DIR: &str = "/usr/share/frogger/voices";
const PLAYERS: [&str; 3] = ["pw-play", "paplay", "canberra-gtk-play"];
/// Upper bound for a single clip; a hanging player is killed.
const MAX_CLIP: Duration = Duration::from_secs(15);
/// How long `finish` waits for queued sounds before the CLI exits.
const MAX_DRAIN: Duration = Duration::from_secs(10);

/// Sound of the freedesktop sound theme for a cue.
fn sound_name(cue: Cue) -> &'static str {
    match cue {
        Cue::Step => "message",
        Cue::EyesClosed => "complete",
        Cue::Done => "service-login",
        Cue::Failed => "dialog-warning",
    }
}

/// Directory with the voice clips for `lang`, if a voice package is
/// installed for it.
pub fn voice_dir(lang: &str) -> Option<PathBuf> {
    let dir = Path::new(VOICE_DIR).join(lang);
    dir.join("enroll.close_eyes.opus").is_file().then_some(dir)
}

/// Play the sound for `cue`; falls back to the terminal bell when no sound
/// theme, player or user session is available. With a voice installed the
/// step sound is left out – the spoken prompt that follows replaces it.
pub fn play(cue: Cue, lang: &str) {
    if !ENABLED.load(Ordering::Relaxed) || (cue == Cue::Step && voice_dir(lang).is_some()) {
        return;
    }
    let file = Path::new(SOUND_DIR).join(format!("{}.oga", sound_name(cue)));
    if !(file.is_file() && player_available()) {
        bell();
        return;
    }
    enqueue(Item::Tone(file));
}

/// Speak the prompt `key` if a voice package for `lang` has a clip for it.
pub fn say(key: &str, lang: &str) {
    if !ENABLED.load(Ordering::Relaxed) || !valid_key(key) {
        return;
    }
    let Some(file) = voice_dir(lang).map(|d| d.join(format!("{key}.opus"))) else {
        return;
    };
    if file.is_file() && player_available() {
        enqueue(Item::Voice(file));
    }
}

/// Message keys are plain identifiers (`enroll.close_eyes`); anything else
/// must never become part of a path.
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('.')
        && !key.contains("..")
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
}

/// Wait (bounded) until all queued sounds have been played, so the last
/// prompt is not cut off when the CLI exits.
pub fn finish() {
    let Some(q) = QUEUE.get() else { return };
    let deadline = Instant::now() + MAX_DRAIN;
    let mut st = q.state.lock().unwrap_or_else(|e| e.into_inner());
    while !(st.items.is_empty() && !st.busy) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        st = q.done.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
    }
}

#[derive(Debug, PartialEq)]
enum Item {
    Tone(PathBuf),
    Voice(PathBuf),
}

#[derive(Default)]
struct State {
    items: VecDeque<Item>,
    busy: bool,
}

struct Queue {
    state: Mutex<State>,
    ready: Condvar,
    done: Condvar,
}

static QUEUE: OnceLock<Queue> = OnceLock::new();

fn enqueue(item: Item) {
    let q = QUEUE.get_or_init(|| {
        std::thread::spawn(worker);
        Queue {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            done: Condvar::new(),
        }
    });
    let mut st = q.state.lock().unwrap_or_else(|e| e.into_inner());
    push(&mut st.items, item);
    q.ready.notify_one();
}

/// Queue policy: a new prompt supersedes prompts that have not started yet
/// (the user only needs the current instruction); sounds are never dropped.
fn push(items: &mut VecDeque<Item>, item: Item) {
    if matches!(item, Item::Voice(_)) {
        items.retain(|i| !matches!(i, Item::Voice(_)));
    }
    items.push_back(item);
}

fn worker() {
    let q = loop {
        // The queue is initialised right after spawning this thread.
        if let Some(q) = QUEUE.get() {
            break q;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    loop {
        let item = {
            let mut st = q.state.lock().unwrap_or_else(|e| e.into_inner());
            st.busy = false;
            q.done.notify_all();
            loop {
                if let Some(i) = st.items.pop_front() {
                    st.busy = true;
                    break i;
                }
                st = q.ready.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let (Item::Tone(file) | Item::Voice(file)) = item;
        match spawn_player(&file) {
            Some(child) => wait_bounded(child),
            None => bell(),
        }
    }
}

fn wait_bounded(mut child: Child) {
    let deadline = Instant::now() + MAX_CLIP;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
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

/// Runtime directory of the session user's audio server, if they have a
/// session at all (otherwise nobody would hear anything).
fn runtime_dir() -> Option<(u32, u32, PathBuf)> {
    let (uid, gid) = session_user();
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    runtime.is_dir().then_some((uid, gid, runtime))
}

fn player_available() -> bool {
    runtime_dir().is_some()
}

fn spawn_player(file: &Path) -> Option<Child> {
    let (uid, gid, runtime) = runtime_dir()?;
    // SAFETY: plain libc getter.
    let root = unsafe { libc::geteuid() } == 0;
    let opus = file.extension().is_some_and(|e| e == "opus");
    for player in PLAYERS {
        // libcanberra decodes only Vorbis and WAV.
        if opus && player == "canberra-gtk-play" {
            continue;
        }
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
            Ok(child) => return Some(child),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
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
        play(Cue::Done, "en"); // must return without spawning or ringing
        say("enroll.done", "en");
        ENABLED.store(true, Ordering::Relaxed);
        assert!(QUEUE.get().is_none());
    }

    #[test]
    fn keys_cannot_escape_the_voice_directory() {
        assert!(valid_key("enroll.close_eyes"));
        assert!(valid_key("auth.blink"));
        for bad in ["", "../x", "a/b", ".hidden", "a..b", "x y", "enroll.done\0"] {
            assert!(!valid_key(bad), "{bad}");
        }
    }

    #[test]
    fn a_new_prompt_supersedes_pending_prompts_but_not_sounds() {
        let mut q = VecDeque::new();
        push(&mut q, Item::Voice("a".into()));
        push(&mut q, Item::Tone("t".into()));
        push(&mut q, Item::Voice("b".into()));
        assert_eq!(q, [Item::Tone("t".into()), Item::Voice("b".into())]);
    }
}
