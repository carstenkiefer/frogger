//! Configuration (`/etc/frogger/config.toml`) and admin policy.
//!
//! Every field has a secure default so an empty or missing file is valid.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_PATH: &str = "/etc/frogger/config.toml";
pub const DATA_DIR: &str = "/var/lib/frogger";
pub const MODEL_DIR: &str = "/usr/share/frogger/models";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub camera: CameraConfig,
    pub auth: AuthConfig,
    pub liveness: LivenessConfig,
    pub policy: PolicyConfig,
    pub models: ModelConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraConfig {
    /// V4L2 device. `auto` picks the best IR camera. Prefer stable paths like
    /// `/dev/v4l/by-id/usb-...-video-index0` over `/dev/videoN`.
    pub device: String,
    /// Requested capture size; the driver may choose the nearest supported one.
    pub width: u32,
    pub height: u32,
    /// Frames discarded after opening (auto exposure / emitter warm-up).
    pub warmup_frames: u32,
    /// Optional UVC extension-unit control that switches the IR emitter on.
    /// Values can be imported from linux-enable-ir-emitter with
    /// `frogger emitter import`.
    pub emitter: Option<EmitterControl>,
    /// Optional "vendor:product" the device must report (camera pinning).
    /// Written by `frogger setup`; makes swapping in another camera fail.
    pub expected_usb_id: Option<String>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        CameraConfig {
            device: "auto".into(),
            width: 640,
            height: 480,
            warmup_frames: 4,
            emitter: None,
            expected_usb_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitterControl {
    pub unit: u8,
    pub selector: u8,
    pub control: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Total time budget of one authentication attempt.
    pub timeout_secs: f32,
    /// Cosine similarity required for a match (SFace; 0.363 is OpenCV's
    /// equal-error point on LFW, we are deliberately stricter).
    pub similarity_threshold: f32,
    /// Number of frames that must match before success.
    pub min_matching_frames: u32,
    /// PAM services face authentication may be used for. Empty = all.
    pub services: Vec<String>,
    /// Services that are display managers (initial login). For those, face
    /// auth is skipped when the user has no running session yet, so the
    /// password unlocks the GNOME keyring / KWallet.
    pub login_services: Vec<String>,
    pub require_password_for_first_login: bool,
    /// Consecutive failures before a temporary lockout.
    pub max_failures: u32,
    pub lockout_secs: u64,
    /// Skip face auth while the laptop lid is closed.
    pub skip_when_lid_closed: bool,
    /// Refuse face auth for non-display-manager services while the
    /// requesting user also has a remote (SSH) session.
    pub deny_while_remote_session: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            timeout_secs: 5.0,
            similarity_threshold: 0.50,
            min_matching_frames: 3,
            services: vec![
                "sudo".into(),
                "sudo-i".into(),
                "su".into(),
                "polkit-1".into(),
                "login".into(),
                "gdm-password".into(),
                "sddm".into(),
                "lightdm".into(),
                "plasmalogin".into(),
                "kde".into(),
                "kde-fingerprint".into(),
                "kde-face".into(),
                "frogger-test".into(),
            ],
            login_services: vec![
                "gdm-password".into(),
                "sddm".into(),
                "lightdm".into(),
                "plasmalogin".into(),
                "login".into(),
            ],
            require_password_for_first_login: true,
            max_failures: 5,
            lockout_secs: 300,
            skip_when_lid_closed: true,
            deny_while_remote_session: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LivenessMode {
    /// IR camera + blink challenge (default, defeats printed photos and
    /// screens).
    Blink,
    /// IR camera only. Screens are invisible in IR, printed IR photos are
    /// NOT reliably rejected. Not recommended.
    IrOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LivenessConfig {
    pub mode: LivenessMode,
    /// Maximum duration of the "closed" phase of a blink.
    pub max_closed_secs: f32,
    /// Minimum mean brightness of the face region on an IR frame. A face on a
    /// phone/laptop screen is (nearly) black under IR.
    pub min_ir_face_brightness: f32,
    /// Minimum IoU between consecutive face boxes; a jump means a different
    /// object was put in front of the camera and resets the challenge.
    pub min_track_iou: f32,
    /// Reject frames with more than one face (prevents a second person or a
    /// photo next to the user from contributing).
    pub reject_multiple_faces: bool,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        LivenessConfig {
            mode: LivenessMode::Blink,
            max_closed_secs: 1.5,
            min_ir_face_brightness: 25.0,
            min_track_iou: 0.45,
            reject_multiple_faces: true,
        }
    }
}

/// Fleet policy. Intended to be managed centrally (Ansible, Intune, Landscape…).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Allow setup on devices without an IR camera. RGB face auth can be
    /// fooled with a photo or video and must never be used on devices with
    /// sensitive data.
    pub allow_insecure_rgb: bool,
    /// Mark this device as holding sensitive data. Forces IR + blink
    /// liveness and overrides `allow_insecure_rgb`.
    pub sensitive_device: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub detector: PathBuf,
    pub recognizer: PathBuf,
    pub detection_threshold: f32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            detector: Path::new(MODEL_DIR).join("face_detection_yunet_2023mar.onnx"),
            recognizer: Path::new(MODEL_DIR).join("face_recognition_sface_2021dec.onnx"),
            detection_threshold: 0.8,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Config =
                    toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let a = &self.auth;
        anyhow::ensure!(
            a.timeout_secs > 0.5 && a.timeout_secs <= 30.0,
            "auth.timeout_secs out of range (0.5..30)"
        );
        anyhow::ensure!(
            (0.30..=0.95).contains(&a.similarity_threshold),
            "auth.similarity_threshold out of range (0.30..0.95)"
        );
        anyhow::ensure!(
            a.min_matching_frames >= 1,
            "auth.min_matching_frames must be >= 1"
        );
        anyhow::ensure!(
            (0.3..=0.99).contains(&self.models.detection_threshold),
            "models.detection_threshold out of range"
        );
        Ok(())
    }

    /// Effective liveness mode after applying policy (sensitive devices
    /// always get the blink challenge).
    pub fn effective_liveness(&self) -> LivenessMode {
        if self.policy.sensitive_device {
            LivenessMode::Blink
        } else {
            self.liveness.mode
        }
    }

    /// Whether running on a non-IR camera is permitted at all.
    pub fn rgb_permitted(&self) -> bool {
        self.policy.allow_insecure_rgb && !self.policy.sensitive_device
    }

    pub fn service_allowed(&self, service: &str) -> bool {
        self.auth.services.is_empty() || self.auth.services.iter().any(|s| s == service)
    }

    pub fn is_login_service(&self, service: &str) -> bool {
        self.auth.login_services.iter().any(|s| s == service)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_secure_default() {
        let c: Config = toml::from_str("").unwrap();
        c.validate().unwrap();
        assert!(!c.rgb_permitted());
        assert_eq!(c.effective_liveness(), LivenessMode::Blink);
    }

    #[test]
    fn sensitive_overrides_rgb() {
        let c: Config = toml::from_str(
            "[policy]\nallow_insecure_rgb = true\nsensitive_device = true\n[liveness]\nmode = \"ir-only\"\n",
        )
        .unwrap();
        assert!(!c.rgb_permitted());
        assert_eq!(c.effective_liveness(), LivenessMode::Blink);
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("[auth]\nthreshhold = 0.1\n").is_err());
    }

    #[test]
    fn shipped_example_parses() {
        let s = include_str!("../../../packaging/config.toml");
        let c: Config = toml::from_str(s).unwrap();
        c.validate().unwrap();
    }
}
