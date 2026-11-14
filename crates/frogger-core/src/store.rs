//! Biometric template storage.
//!
//! Templates are special-category personal data (GDPR Art. 9). We store only
//! 128-d embeddings and small normalised eye patches – never camera images –
//! in files readable by the daemon user only (0600 inside a 0700 directory).
//! Full-disk encryption is strongly recommended on top.

use crate::vision::eyes::EyeCalibration;
use anyhow::{bail, Context, Result};
use frogger_proto::is_valid_username;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub embedding: Vec<f32>,
    pub created: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserTemplate {
    pub version: u32,
    pub user: String,
    /// Fingerprint of the recognition model; templates from another model
    /// are not comparable and are rejected.
    pub model: String,
    /// "infrared" or "rgb" – the camera type used at enrollment.
    pub camera_kind: String,
    pub camera_usb_id: Option<String>,
    pub label: Option<String>,
    pub samples: Vec<Sample>,
    pub eyes: Option<EyeCalibration>,
    pub created: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Store {
        Store {
            dir: data_dir.join("users"),
        }
    }

    fn path(&self, user: &str) -> Result<PathBuf> {
        if !is_valid_username(user) {
            bail!("invalid user name");
        }
        Ok(self.dir.join(format!("{user}.json")))
    }

    pub fn load(&self, user: &str) -> Result<Option<UserTemplate>> {
        let p = self.path(user)?;
        match std::fs::read(&p) {
            Ok(b) => {
                let t: UserTemplate =
                    serde_json::from_slice(&b).with_context(|| format!("parsing {}", p.display()))?;
                if t.version != FORMAT_VERSION || t.user != user {
                    bail!("template {} has an unexpected format/owner", p.display());
                }
                Ok(Some(t))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    /// Atomically replace the user's template.
    pub fn save(&self, t: &UserTemplate) -> Result<()> {
        let p = self.path(&t.user)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        let tmp = p.with_extension("json.tmp");
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec(t)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    pub fn remove(&self, user: &str) -> Result<bool> {
        let p = self.path(user)?;
        match std::fs::remove_file(&p) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn users(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut v: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_string))
            .filter(|u| is_valid_username(u))
            .collect();
        v.sort();
        v
    }
}

/// Cheap, stable fingerprint of a model file (FNV-1a 64 over contents).
pub fn model_fingerprint(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Ok(format!("fnv1a64:{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_remove() {
        let dir = std::env::temp_dir().join(format!("frogger-store-{}", std::process::id()));
        let s = Store::new(&dir);
        let t = UserTemplate {
            version: FORMAT_VERSION,
            user: "alice".into(),
            model: "m".into(),
            camera_kind: "infrared".into(),
            camera_usb_id: None,
            label: None,
            samples: vec![Sample {
                embedding: vec![1.0; 4],
                created: 1,
            }],
            eyes: None,
            created: 1,
        };
        s.save(&t).unwrap();
        let mode = std::fs::metadata(dir.join("users/alice.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(s.load("alice").unwrap().unwrap().samples.len(), 1);
        assert_eq!(s.users(), vec!["alice".to_string()]);
        assert!(s.load("../x").is_err());
        assert!(s.remove("alice").unwrap());
        assert!(s.load("alice").unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
