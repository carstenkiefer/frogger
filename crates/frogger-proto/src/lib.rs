//! Wire protocol shared by `pam_frogger.so`, the `frogger` CLI and `froggerd`.
//!
//! The transport is a Unix stream socket. Every message is a single line of
//! JSON terminated by `\n`. The client sends exactly one [`Request`]; the
//! daemon answers with any number of [`Event::Info`] lines followed by exactly
//! one terminal event ([`Event::Result`] or [`Event::Error`]).
//!
//! The daemon never trusts identity claims made inside a request: it derives
//! the caller's uid/pid from `SO_PEERCRED`.

use serde::{Deserialize, Serialize};

/// Release version: the git tag set by CI (`FROGGER_VERSION`), otherwise the
/// crate version.
pub fn version() -> &'static str {
    match option_env!("FROGGER_VERSION") {
        Some(v) if !v.is_empty() => v.strip_prefix('v').unwrap_or(v),
        _ => env!("CARGO_PKG_VERSION"),
    }
}

/// Protocol version; bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default location of the daemon socket (created by `frogger.socket`).
pub const SOCKET_PATH: &str = "/run/frogger/frogger.sock";

/// Upper bound for a single protocol line, protects both sides from
/// unbounded allocations.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(flatten)]
    pub op: Op,
    /// Preferred language for user facing messages ("de", "en").
    #[serde(default)]
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Is the daemon alive, which camera/security level is configured?
    Status,
    /// Authenticate `user` for PAM service `service`.
    Authenticate {
        user: String,
        service: String,
        #[serde(default)]
        tty: Option<String>,
        #[serde(default)]
        rhost: Option<String>,
    },
    /// Enroll a face for `user` (root only).
    Enroll {
        user: String,
        #[serde(default)]
        label: Option<String>,
        /// Set when the operator explicitly acknowledged the RGB warning.
        #[serde(default)]
        accept_insecure_rgb: bool,
    },
    /// List enrolled users / models (root only, or own user).
    List {
        #[serde(default)]
        user: Option<String>,
    },
    /// Remove a user's enrollment (root only, or own user).
    Remove { user: String },
    /// Run the recognition pipeline without granting anything and report
    /// scores (root only, or own user).
    Test { user: String },
    /// Record a measurement trace (no images) for `seconds` while the user
    /// looks at the camera; returned as `data` of the result (root only, or
    /// own user).
    Record {
        user: String,
        seconds: u32,
        #[serde(default)]
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStatus {
    /// Face matched and all liveness checks passed.
    Success,
    /// A face was seen but did not match or liveness failed.
    Failure,
    /// Face auth is not applicable right now (not enrolled, remote session,
    /// service not allowed, camera missing, lid closed, ...). PAM should fall
    /// through to the next module.
    Unavailable,
    /// Too many failures; temporarily locked out.
    Locked,
    /// Caller is not allowed to perform this operation.
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Progress / instruction text that should be shown to the user.
    Info { text: String },
    /// Terminal event of an operation.
    Result {
        status: AuthStatus,
        #[serde(default)]
        message: String,
        /// Optional structured payload (status, list, test scores ...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    /// Protocol / internal error. Treated like `Unavailable` by PAM.
    Error { message: String },
    /// Audible signal for the user (enrollment: the eyes may be closed, so
    /// the terminal cannot be read). Only sent to the `frogger` CLI.
    Cue { cue: Cue },
    /// A prompt that may be spoken (message key, e.g. `enroll.close_eyes`):
    /// the CLI plays a clip of an installed voice package. Enrollment only.
    Say { key: String },
}

/// Kinds of audible signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cue {
    /// A step is complete; the next instruction follows.
    Step,
    /// Closed eyes were recorded: open them again.
    EyesClosed,
    /// The operation succeeded.
    Done,
    /// The operation failed.
    Failed,
}

impl Event {
    pub fn info(text: impl Into<String>) -> Self {
        Event::Info { text: text.into() }
    }
    pub fn result(status: AuthStatus, message: impl Into<String>) -> Self {
        Event::Result {
            status,
            message: message.into(),
            data: None,
        }
    }
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Event::Info { .. })
    }
}

impl Request {
    pub fn new(op: Op) -> Self {
        Request {
            v: PROTOCOL_VERSION,
            op,
            lang: None,
        }
    }
}

/// Serialize a value as one protocol line (including the trailing newline).
pub fn to_line<T: Serialize>(value: &T) -> Vec<u8> {
    let mut v = serde_json::to_vec(value).expect("protocol types always serialize");
    v.push(b'\n');
    v
}

/// Minimal sanity check for user names before they are used anywhere
/// (file names, lookups). Mirrors the conservative subset accepted by
/// shadow-utils plus `@`/`\` used by SSSD/winbind domain users.
pub fn is_valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '$' | '\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request() {
        let r = Request::new(Op::Authenticate {
            user: "alice".into(),
            service: "sudo".into(),
            tty: Some("/dev/pts/1".into()),
            rhost: None,
        });
        let line = to_line(&r);
        let back: Request = serde_json::from_slice(&line).unwrap();
        match back.op {
            Op::Authenticate { user, service, .. } => {
                assert_eq!(user, "alice");
                assert_eq!(service, "sudo");
            }
            _ => panic!("wrong op"),
        }
    }

    #[test]
    fn usernames() {
        assert!(is_valid_username("alice"));
        assert!(is_valid_username("max.mustermann@corp.example"));
        assert!(!is_valid_username("../etc/passwd"));
        assert!(!is_valid_username("-rf"));
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("a b"));
    }
}
