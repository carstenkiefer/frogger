//! Drives the camera and the [`Verifier`] for authentication, enrollment and
//! diagnostics. Owned by the daemon; one operation at a time (the camera is a
//! single shared resource).

use crate::auth::{FailReason, Observation, Progress, StrobeFilter, Verifier};
use crate::camera::{self, Camera, CameraKind};
use crate::config::{Config, LivenessMode};
use crate::store::{self, Sample, Store, UserTemplate, FORMAT_VERSION};
use crate::vision::eyes::{average, EyeCalibration, EyePair};
use crate::vision::{self, Analysis, Pipeline};
use anyhow::Result;
use frogger_i18n::tr;
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// Interval between full analyses (detection + embedding); frames in between
/// only sample the eyes.
const FULL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_SAMPLES: usize = 40;
const ENROLL_SAMPLES: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure(String),
    Unavailable(String),
    Denied(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptReport {
    pub success: bool,
    pub reason: Option<FailReason>,
    pub stats: crate::auth::Stats,
    pub camera_kind: CameraKind,
    pub elapsed_ms: u128,
}

pub struct Engine {
    pub cfg: Config,
    pipeline: Pipeline,
    pub store: Store,
    model_fp: String,
}

/// Callback for user-facing (already translated) progress messages.
pub type Emit<'a> = &'a mut dyn FnMut(String);

impl Engine {
    pub fn load(cfg: Config, data_dir: &Path) -> Result<Engine> {
        let mut pipeline = Pipeline::load(&cfg.models.detector, &cfg.models.recognizer)?;
        pipeline.detector.score_threshold = cfg.models.detection_threshold;
        let model_fp = store::model_fingerprint(&cfg.models.recognizer)?;
        let key = crate::crypto::TemplateKey::from_credentials()?;
        let store = Store::with_key(data_dir, key);
        match store.migrate() {
            Ok(0) => {}
            Ok(n) => log::info!("encrypted {n} existing template(s) with the TPM-sealed key"),
            Err(e) => log::warn!("template migration failed: {e:#}"),
        }
        log::info!(
            "template encryption: {}",
            if store.encrypted() {
                "active (systemd credential)"
            } else {
                "not configured"
            }
        );
        Ok(Engine {
            cfg,
            pipeline,
            store,
            model_fp,
        })
    }

    fn open_camera(&self) -> Result<(Camera, CameraKind)> {
        let cam = Camera::open(&self.cfg.camera)?;
        let kind = cam.info.kind;
        Ok((cam, kind))
    }

    /// Run one recognition attempt against `template`.
    pub fn attempt(&self, template: &UserTemplate, lang: &str, emit: Emit) -> Result<AttemptReport> {
        let start = Instant::now();
        let (mut cam, mut kind) = self.open_camera()?;
        let infrared = template.camera_kind == "infrared";
        if infrared && kind == CameraKind::Unknown {
            kind = CameraKind::Infrared; // verified per frame via chroma
        }
        let ir = infrared && kind != CameraKind::Rgb;
        let mut verifier = Verifier::new(&self.cfg, template, ir);
        let mut strobe = StrobeFilter::default();
        let timeout = Duration::from_secs_f32(self.cfg.auth.timeout_secs);
        let mut last_full: Option<(Instant, Analysis)> = None;
        let mut prompted_blink = false;
        let mut success = false;
        while start.elapsed() < timeout {
            let frame = match cam.frame() {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("frame capture failed: {e:#}");
                    continue;
                }
            };
            let t = start.elapsed().as_secs_f32();
            // Off phase of a strobing emitter: skip before spending CPU on it.
            if ir && strobe.is_off(frame.mean) {
                verifier.feed(&Observation {
                    t,
                    chroma: frame.chroma,
                    analysis: None,
                    eyes_only: None,
                    emitter_off: true,
                });
                continue;
            }
            let full_due = last_full
                .as_ref()
                .map(|(at, _)| at.elapsed() >= FULL_INTERVAL)
                .unwrap_or(true);
            let obs = if full_due {
                let analysis = self.pipeline.analyse(&frame.image)?;
                last_full = analysis.clone().map(|a| (Instant::now(), a));
                Observation {
                    t,
                    chroma: frame.chroma,
                    analysis,
                    eyes_only: None,
                    emitter_off: false,
                }
            } else {
                let (_, a) = last_full.as_ref().expect("checked above");
                Observation {
                    t,
                    chroma: frame.chroma,
                    analysis: None,
                    eyes_only: Some(vision::eye_sample(&frame.image, a)),
                    emitter_off: false,
                }
            };
            match verifier.feed(&obs) {
                Progress::Success { similarity } => {
                    log::info!("user {} matched (similarity {similarity:.3})", template.user);
                    success = true;
                    break;
                }
                Progress::NeedBlink if !prompted_blink => {
                    prompted_blink = true;
                    emit(tr(lang, "auth.blink", &[]));
                }
                _ => {}
            }
        }
        if !success {
            log::info!(
                "attempt for {} without success: {:?}, stats {}",
                template.user,
                verifier.failure_reason(),
                serde_json::to_string(&verifier.stats).unwrap_or_default()
            );
        }
        Ok(AttemptReport {
            success,
            reason: (!success).then(|| verifier.failure_reason()),
            stats: verifier.stats.clone(),
            camera_kind: kind,
            elapsed_ms: start.elapsed().as_millis(),
        })
    }

    /// Full authentication flow for PAM (policy checks are done by the caller).
    pub fn authenticate(&self, user: &str, lang: &str, emit: Emit) -> Outcome {
        let template = match self.usable_template(user) {
            Ok(t) => t,
            Err(o) => return o,
        };
        if template.camera_kind != "infrared" {
            emit(tr(lang, "auth.insecure_rgb", &[]));
            emit(tr(lang, "auth.start", &[]));
        } else if self.cfg.effective_liveness() == LivenessMode::Blink {
            emit(tr(lang, "auth.start", &[]));
        } else {
            emit(tr(lang, "auth.start_ir_only", &[]));
        }
        match self.attempt(&template, lang, emit) {
            Ok(r) if r.success => Outcome::Success,
            Ok(r) => Outcome::Failure(tr(
                lang,
                r.reason.unwrap_or(FailReason::NotRecognized).message_key(),
                &[],
            )),
            Err(e) => {
                log::warn!("authentication attempt failed: {e:#}");
                Outcome::Unavailable(tr(lang, "auth.camera_unavailable", &[]))
            }
        }
    }

    fn usable_template(&self, user: &str) -> std::result::Result<UserTemplate, Outcome> {
        let t = match self.store.load(user) {
            Ok(Some(t)) => t,
            Ok(None) => return Err(Outcome::Unavailable("not enrolled".into())),
            Err(e) => return Err(Outcome::Unavailable(format!("template error: {e:#}"))),
        };
        if t.model != self.model_fp {
            return Err(Outcome::Unavailable(
                "template was created with another model; re-enroll".into(),
            ));
        }
        if t.camera_kind != "infrared" && !self.cfg.rgb_permitted() {
            return Err(Outcome::Unavailable(
                "RGB enrollment not permitted by policy".into(),
            ));
        }
        if let (Some(pinned), Some(enrolled)) = (&self.cfg.camera.expected_usb_id, &t.camera_usb_id) {
            if pinned != enrolled {
                return Err(Outcome::Unavailable(
                    "template was enrolled with another camera".into(),
                ));
            }
        }
        Ok(t)
    }

    /// Diagnostic attempt that reports scores; never counts as a login.
    pub fn test(&self, user: &str, lang: &str, emit: Emit) -> Result<AttemptReport> {
        let t = self.usable_template(user).map_err(|o| match o {
            Outcome::Unavailable(m) if m == "not enrolled" => {
                anyhow::anyhow!("{}", tr(lang, "cli.not_enrolled", &[("user", user)]))
            }
            Outcome::Unavailable(m) | Outcome::Failure(m) | Outcome::Denied(m) => anyhow::anyhow!("{m}"),
            Outcome::Success => anyhow::anyhow!("unexpected state"),
        })?;
        emit(tr(lang, "auth.start", &[]));
        self.attempt(&t, lang, emit)
    }

    /// Determine camera type from probe + a short capture.
    pub fn detect_kind(&self, cam: &mut Camera) -> (CameraKind, Vec<String>) {
        let stats = camera::capture_stats(cam, 10).ok();
        camera::classify(&cam.info, stats.as_ref())
    }

    /// Interactive enrollment.
    pub fn enroll(
        &self,
        user: &str,
        label: Option<String>,
        accept_rgb: bool,
        lang: &str,
        emit: Emit,
    ) -> Outcome {
        let (mut cam, _) = match self.open_camera() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("enroll: {e:#}");
                return Outcome::Unavailable(tr(lang, "auth.camera_unavailable", &[]));
            }
        };
        let (kind, _evidence) = self.detect_kind(&mut cam);
        let infrared = kind == CameraKind::Infrared;
        if !infrared {
            if !self.cfg.rgb_permitted() {
                return Outcome::Denied(tr(lang, "enroll.rgb_refused", &[]));
            }
            if !accept_rgb {
                return Outcome::Denied(tr(lang, "enroll.rgb_needs_ack", &[]));
            }
            emit(tr(lang, "enroll.rgb_warning", &[]));
        }

        // Phase 1: open-eye samples from slightly varying poses.
        emit(tr(lang, "enroll.start", &[("user", user)]));
        let mut samples: Vec<Analysis> = Vec::new();
        let mut last_kept = Instant::now() - Duration::from_secs(1);
        let start = Instant::now();
        let mut turned = false;
        let mut multi = 0;
        let mut strobe = StrobeFilter::default();
        while samples.len() < ENROLL_SAMPLES && start.elapsed() < Duration::from_secs(20) {
            let Ok(frame) = cam.frame() else { continue };
            if infrared && (frame.chroma >= 2.0 || strobe.is_off(frame.mean)) {
                continue;
            }
            let Ok(Some(a)) = self.pipeline.analyse(&frame.image) else {
                continue;
            };
            if a.face_count > 1 {
                multi += 1;
                if multi == 5 {
                    emit(tr(lang, "enroll.multiple_faces", &[]));
                }
                continue;
            }
            if a.face.interocular() < 28.0 {
                continue; // too far away / too small for reliable features
            }
            if infrared && a.face_brightness < self.cfg.liveness.min_ir_face_brightness {
                continue;
            }
            if last_kept.elapsed() >= Duration::from_millis(350) {
                samples.push(a);
                last_kept = Instant::now();
                if !turned && samples.len() == ENROLL_SAMPLES / 2 {
                    turned = true;
                    emit(tr(lang, "enroll.turn", &[]));
                }
            }
        }
        if samples.len() < ENROLL_SAMPLES / 2 {
            return Outcome::Failure(tr(lang, "enroll.no_face", &[]));
        }
        // Consistency: every sample must resemble the others (one person).
        let mean = mean_embedding(&samples);
        let worst = samples
            .iter()
            .map(|s| vision::cosine(&s.embedding, &mean))
            .fold(1.0f32, f32::min);
        if worst < 0.6 {
            return Outcome::Failure(tr(lang, "enroll.inconsistent", &[]));
        }

        // Phase 2: closed-eye references for the blink challenge.
        emit(tr(lang, "enroll.close_eyes", &[]));
        std::thread::sleep(Duration::from_millis(1200));
        let open_eyes: Vec<EyePair> = samples.iter().map(|s| s.eyes.clone()).collect();
        let open_ref = EyePair {
            left: average(&open_eyes.iter().map(|e| e.left.as_slice()).collect::<Vec<_>>()),
            right: average(&open_eyes.iter().map(|e| e.right.as_slice()).collect::<Vec<_>>()),
        };
        let mut closed: Vec<(f32, EyePair)> = Vec::new();
        let phase = Instant::now();
        while phase.elapsed() < Duration::from_millis(1600) {
            let Ok(frame) = cam.frame() else { continue };
            // Only lit frames of exactly one face: emitter-off frames would
            // otherwise end up as "closed eye" references (pure noise).
            if infrared && (frame.chroma >= 2.0 || strobe.is_off(frame.mean)) {
                continue;
            }
            if let Ok(Some(a)) = self.pipeline.analyse(&frame.image) {
                if a.face_count > 1
                    || (infrared && a.face_brightness < self.cfg.liveness.min_ir_face_brightness)
                    || vision::cosine(&a.embedding, &mean) < 0.5
                {
                    continue;
                }
                let sim = dot(&a.eyes.left, &open_ref.left).min(dot(&a.eyes.right, &open_ref.right));
                closed.push((sim, a.eyes));
            }
        }
        emit(tr(lang, "enroll.open_eyes", &[]));
        // Keep the frames least similar to the open reference.
        closed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let keep = (closed.len() / 2).max(1).min(closed.len());
        let closed: Vec<EyePair> = closed.into_iter().take(keep).map(|(_, e)| e).collect();
        let eyes = EyeCalibration::from_samples(&open_eyes, &closed);
        if eyes.is_none() && (self.cfg.effective_liveness() == LivenessMode::Blink || !infrared) {
            return Outcome::Failure(tr(lang, "enroll.calibration_failed", &[]));
        }

        // Merge with an existing template from the same model/camera type.
        let now = store::now();
        let mut template = match self.store.load(user) {
            Ok(Some(t)) if t.model == self.model_fp && t.camera_kind == kind_name(kind) => t,
            _ => UserTemplate {
                version: FORMAT_VERSION,
                user: user.to_string(),
                model: self.model_fp.clone(),
                camera_kind: kind_name(kind).into(),
                camera_usb_id: cam.info.usb_id.clone(),
                label: None,
                samples: Vec::new(),
                eyes: None,
                created: now,
            },
        };
        template.label = label.or(template.label);
        template.eyes = eyes;
        template.samples.extend(samples.iter().map(|s| Sample {
            embedding: s.embedding.clone(),
            created: now,
        }));
        let excess = template.samples.len().saturating_sub(MAX_SAMPLES);
        template.samples.drain(..excess);
        drop(cam);

        // Phase 3: verify the fresh template with the real challenge.
        std::thread::sleep(Duration::from_millis(500));
        emit(tr(lang, "enroll.verify", &[]));
        match self.attempt(&template, lang, emit) {
            Ok(r) if r.success => {}
            Ok(r) => {
                let why = tr(
                    lang,
                    r.reason.unwrap_or(FailReason::NotRecognized).message_key(),
                    &[],
                );
                return Outcome::Failure(format!("{} {why}", tr(lang, "enroll.verify_failed", &[])));
            }
            Err(e) => {
                log::warn!("verification failed: {e:#}");
                return Outcome::Unavailable(tr(lang, "auth.camera_unavailable", &[]));
            }
        }
        if let Err(e) = self.store.save(&template) {
            log::error!("saving template: {e:#}");
            return Outcome::Unavailable(format!("{e:#}"));
        }
        let count = template.samples.len().to_string();
        Outcome::Success.with_message(emit, tr(lang, "enroll.done", &[("count", &count)]))
    }
}

impl Outcome {
    fn with_message(self, emit: Emit, msg: String) -> Outcome {
        emit(msg);
        self
    }
}

pub fn kind_name(k: CameraKind) -> &'static str {
    match k {
        CameraKind::Infrared => "infrared",
        CameraKind::Rgb | CameraKind::Unknown => "rgb",
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn mean_embedding(s: &[Analysis]) -> Vec<f32> {
    let mut m = vec![0f32; s[0].embedding.len()];
    for a in s {
        for (x, y) in m.iter_mut().zip(&a.embedding) {
            *x += y;
        }
    }
    vision::embed::normalize(&mut m);
    m
}
