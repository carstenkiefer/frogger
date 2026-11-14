//! Drives the camera and the [`Verifier`] for authentication, enrollment and
//! diagnostics. Owned by the daemon; one operation at a time (the camera is a
//! single shared resource).

use crate::auth::{FailReason, Observation, Progress, StrobeFilter, Verifier};
use crate::camera::{self, Camera, CameraKind};
use crate::config::{Config, DepthMode, LivenessMode};
use crate::store::{self, Sample, Store, UserTemplate, FORMAT_VERSION};
use crate::vision::eyes::{average, EyeCalibration, EyePair};
use crate::vision::{self, Analysis, Pipeline};
use anyhow::Result;
use frogger_i18n::tr;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Interval between full analyses (detection + embedding); frames in between
/// only sample the eyes.
// 400 ms leaves room for eye-only samples on slow CPUs and cameras with
// strobing emitters (field test: ~9 lit frames/s); identity still needs
// only ~1.2 s for three matching frames.
const FULL_INTERVAL: Duration = Duration::from_millis(400);
const MAX_SAMPLES: usize = 40;
/// Enrollment phases (time based so people can follow the prompts).
const STRAIGHT_PHASE: Duration = Duration::from_millis(2500);
const TURN_PHASE: Duration = Duration::from_millis(3500);
const PHASE_LIMIT: Duration = Duration::from_secs(12);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(300);
const MIN_ENROLL_SAMPLES: usize = 6;
const MAX_ENROLL_SAMPLES: usize = 20;
/// Closed-eye phase: wait up to this long for closed eyes, then record them
/// for CLOSED_RECORD.
const CLOSE_WAIT_LIMIT: Duration = Duration::from_secs(7);
const CLOSED_RECORD: Duration = Duration::from_millis(1000);
/// How far below the least similar open-eye sample a frame must be to count
/// as closed eyes.
const CLOSED_MARGIN: f32 = 0.05;
/// V4L2 buffers queued by the camera (see Camera::open).
const CAMERA_BUFFERS: usize = 4;
/// Analyses without any face before `auth.no_face_timeout_secs` may end an
/// attempt (slow CPUs analyse fewer frames in the same time).
const MIN_ANALYSES_WITHOUT_FACE: u32 = 3;
/// A depth frame older than this is not used for the current IR frame.
const DEPTH_MAX_AGE: Duration = Duration::from_millis(150);

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
    /// Loaded on first use or in the background (see `preload_in_background`),
    /// so the daemon can answer policy decisions – e.g. "first login needs
    /// the password" – without waiting for the models.
    models: Arc<OnceLock<std::result::Result<Models, String>>>,
    pub store: Store,
}

struct Models {
    pipeline: Pipeline,
    fingerprint: String,
}

fn load_models(m: &crate::config::ModelConfig) -> std::result::Result<Models, String> {
    let load = || -> Result<Models> {
        let mut pipeline = Pipeline::load(&m.detector, &m.recognizer)?;
        pipeline.detector.score_threshold = m.detection_threshold;
        let fingerprint = store::model_fingerprint(&m.recognizer)?;
        Ok(Models {
            pipeline,
            fingerprint,
        })
    };
    load().map_err(|e| format!("{e:#}"))
}

/// Callback for user-facing (already translated) progress messages.
pub type Emit<'a> = &'a mut dyn FnMut(String);

impl Engine {
    /// Load configuration, key and models (blocking).
    pub fn load(cfg: Config, data_dir: &Path) -> Result<Engine> {
        let engine = Engine::new(cfg, data_dir)?;
        engine.models()?;
        Ok(engine)
    }

    /// Set up storage only; the models are loaded on first use or by
    /// [`Engine::preload_in_background`].
    pub fn new(cfg: Config, data_dir: &Path) -> Result<Engine> {
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
            models: Arc::new(OnceLock::new()),
            store,
        })
    }

    /// Start loading the models in a background thread. Callers that need
    /// them earlier wait for this load instead of starting a second one.
    pub fn preload_in_background(&self) {
        let cell = self.models.clone();
        let cfg = self.cfg.models.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            match cell.get_or_init(|| load_models(&cfg)) {
                Ok(_) => log::info!("models loaded in {:?}", t.elapsed()),
                Err(e) => log::error!("loading models: {e}"),
            }
        });
    }

    fn models(&self) -> Result<&Models> {
        self.models
            .get_or_init(|| load_models(&self.cfg.models))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("loading models: {e}"))
    }

    fn open_camera(&self) -> Result<(Camera, CameraKind)> {
        let cam = Camera::open(&self.cfg.camera)?;
        let kind = cam.info.kind;
        Ok((cam, kind))
    }

    /// Start the depth camera if configured (experimental). `required`
    /// without a working sensor makes face authentication unavailable.
    fn open_depth(&self, cam: &Camera) -> Result<Option<camera::DepthStream>> {
        let dc = &self.cfg.camera.depth;
        if dc.mode == DepthMode::Off {
            return Ok(None);
        }
        let stream = camera::find_depth_camera(dc, &cam.info).map(|p| (camera::DepthStream::start(&p), p));
        match (stream, dc.mode) {
            (Some((Ok(s), p)), _) => {
                log::debug!("depth camera {} active", p.display());
                Ok(Some(s))
            }
            (Some((Err(e), _)), DepthMode::Required) => Err(e.context("depth camera required")),
            (None, DepthMode::Required) => anyhow::bail!("depth camera required but none found"),
            (Some((Err(e), _)), _) => {
                log::warn!("depth camera unusable, continuing without: {e:#}");
                Ok(None)
            }
            (None, _) => Ok(None),
        }
    }

    /// Run one recognition attempt against `template`.
    pub fn attempt(
        &self,
        template: &UserTemplate,
        timeout: Duration,
        lang: &str,
        emit: Emit,
    ) -> Result<AttemptReport> {
        let start = Instant::now();
        let (mut cam, mut kind) = self.open_camera()?;
        // Wait for a background model load only after the camera started.
        let models = self.models()?;
        let infrared = template.camera_kind == "infrared";
        if infrared && kind == CameraKind::Unknown {
            kind = CameraKind::Infrared; // verified per frame via chroma
        }
        let ir = infrared && kind != CameraKind::Rgb;
        let depth = self.open_depth(&cam)?;
        let mut verifier = Verifier::new(&self.cfg, template, ir);
        let mut strobe = StrobeFilter::default();
        // Nobody in front of the camera (looking away, walked off, another
        // screen): give up early so the password prompt appears quickly.
        let no_face = Duration::from_secs_f32(self.cfg.auth.no_face_timeout_secs);
        let mut full_analyses = 0u32;
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
                    depth: None,
                });
                continue;
            }
            let full_due = last_full
                .as_ref()
                .map(|(at, _)| at.elapsed() >= FULL_INTERVAL)
                .unwrap_or(true);
            let obs = if full_due {
                let analysis = models.pipeline.analyse(&frame.image)?;
                full_analyses += 1;
                last_full = analysis.clone().map(|a| (Instant::now(), a));
                let depth_verdict = match (&depth, &analysis) {
                    (Some(d), Some(a)) => Some(match d.latest(DEPTH_MAX_AGE) {
                        Some(img) => vision::depth::measure(
                            &img,
                            &a.face,
                            frame.image.width,
                            frame.image.height,
                            self.cfg.camera.depth.min_relief_mm,
                        ),
                        None => vision::depth::DepthVerdict::Invalid,
                    }),
                    _ => None,
                };
                Observation {
                    t,
                    chroma: frame.chroma,
                    analysis,
                    eyes_only: None,
                    emitter_off: false,
                    depth: depth_verdict,
                }
            } else {
                let (_, a) = last_full.as_ref().expect("checked above");
                Observation {
                    t,
                    chroma: frame.chroma,
                    analysis: None,
                    eyes_only: Some(vision::eye_sample(&frame.image, a)),
                    emitter_off: false,
                    depth: None,
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
            if !no_face.is_zero()
                && verifier.stats.frames_with_face == 0
                && full_analyses >= MIN_ANALYSES_WITHOUT_FACE
                && start.elapsed() >= no_face
            {
                log::debug!("no face after {full_analyses} analyses, giving up early");
                break;
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
    pub fn authenticate(&self, user: &str, service: &str, lang: &str, emit: Emit) -> Outcome {
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
        let timeout = Duration::from_secs_f32(self.cfg.timeout_for(service));
        match self.attempt(&template, timeout, lang, emit) {
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
        let models = self
            .models()
            .map_err(|e| Outcome::Unavailable(format!("{e:#}")))?;
        if t.model != models.fingerprint {
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
        self.attempt(
            &t,
            Duration::from_secs_f32(self.cfg.auth.timeout_secs),
            lang,
            emit,
        )
    }

    /// Next frame usable for enrollment: lit by the emitter, exactly one face,
    /// large and bright enough. `multi` counts frames with several faces.
    fn enroll_frame(
        &self,
        cam: &mut Camera,
        infrared: bool,
        strobe: &mut StrobeFilter,
        multi: &mut u32,
    ) -> Option<Analysis> {
        let frame = cam.frame().ok()?;
        if infrared && (frame.chroma >= 2.0 || strobe.is_off(frame.mean)) {
            return None;
        }
        let a = self.models().ok()?.pipeline.analyse(&frame.image).ok()??;
        if a.face_count > 1 {
            *multi += 1;
            return None;
        }
        if a.face.interocular() < 28.0 {
            return None; // too far away / too small for reliable features
        }
        if infrared && a.face_brightness < self.cfg.liveness.min_ir_face_brightness {
            return None;
        }
        Some(a)
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
        let fingerprint = match self.models() {
            Ok(m) => m.fingerprint.as_str(),
            Err(e) => {
                log::error!("enroll: {e:#}");
                return Outcome::Unavailable(tr(lang, "auth.camera_unavailable", &[]));
            }
        };
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

        // Phase 1: open-eye samples, first looking straight, then turning the
        // head slightly. Time based, so the person has time to follow the
        // instructions regardless of the camera frame rate.
        let mut strobe = StrobeFilter::default();
        let mut multi = 0u32;
        let mut samples: Vec<Analysis> = Vec::new();
        emit(tr(lang, "enroll.start", &[("user", user)]));
        for (phase, duration) in [(0, STRAIGHT_PHASE), (1, TURN_PHASE)] {
            if phase == 1 {
                emit(tr(lang, "enroll.turn", &[]));
            }
            flush(&mut cam);
            let start = Instant::now();
            let mut last_kept = start - SAMPLE_INTERVAL;
            let mut kept = 0;
            // Keep going past `duration` until the phase has a few samples
            // (face briefly lost), but never longer than PHASE_LIMIT.
            while (start.elapsed() < duration || kept < 3) && start.elapsed() < PHASE_LIMIT {
                let Some(a) = self.enroll_frame(&mut cam, infrared, &mut strobe, &mut multi) else {
                    if multi == 5 {
                        emit(tr(lang, "enroll.multiple_faces", &[]));
                    }
                    continue;
                };
                if last_kept.elapsed() >= SAMPLE_INTERVAL && samples.len() < MAX_ENROLL_SAMPLES {
                    samples.push(a);
                    last_kept = Instant::now();
                    kept += 1;
                }
            }
        }
        if samples.len() < MIN_ENROLL_SAMPLES {
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

        // Phase 2: closed-eye references for the blink challenge. Wait until
        // the eyes are actually closed, record them for a moment, then ask
        // to open them again.
        let open_eyes: Vec<EyePair> = samples.iter().map(|s| s.eyes.clone()).collect();
        let open_ref = EyePair {
            left: average(&open_eyes.iter().map(|e| e.left.as_slice()).collect::<Vec<_>>()),
            right: average(&open_eyes.iter().map(|e| e.right.as_slice()).collect::<Vec<_>>()),
        };
        let eye_sim = |e: &EyePair| dot(&e.left, &open_ref.left).min(dot(&e.right, &open_ref.right));
        let open_floor = open_eyes.iter().map(eye_sim).fold(f32::INFINITY, f32::min);
        let closed_below = open_floor - CLOSED_MARGIN;
        emit(tr(lang, "enroll.close_eyes", &[]));
        flush(&mut cam);
        let phase = Instant::now();
        let mut closed_since: Option<Instant> = None;
        let mut closed: Vec<(f32, EyePair)> = Vec::new();
        while phase.elapsed() < CLOSE_WAIT_LIMIT {
            if closed_since.is_some_and(|t| t.elapsed() >= CLOSED_RECORD) && closed.len() >= 3 {
                break;
            }
            // Only lit frames of the same single person: emitter-off frames
            // would otherwise end up as "closed eye" references (noise).
            let Some(a) = self.enroll_frame(&mut cam, infrared, &mut strobe, &mut multi) else {
                continue;
            };
            if vision::cosine(&a.embedding, &mean) < 0.5 {
                continue;
            }
            let sim = eye_sim(&a.eyes);
            if sim < closed_below {
                closed_since.get_or_insert_with(Instant::now);
                closed.push((sim, a.eyes));
            }
        }
        emit(tr(lang, "enroll.open_eyes", &[]));
        log::info!(
            "enrollment of {user}: {} open samples, {} closed samples (open floor {open_floor:.3})",
            samples.len(),
            closed.len()
        );
        let closed: Vec<EyePair> = closed.into_iter().map(|(_, e)| e).collect();
        let eyes = EyeCalibration::from_samples(&open_eyes, &closed);
        if eyes.is_none() && (self.cfg.effective_liveness() == LivenessMode::Blink || !infrared) {
            return Outcome::Failure(tr(lang, "enroll.calibration_failed", &[]));
        }

        // Merge with an existing template from the same model/camera type.
        let now = store::now();
        let mut template = match self.store.load(user) {
            Ok(Some(t)) if t.model == fingerprint && t.camera_kind == kind_name(kind) => t,
            _ => UserTemplate {
                version: FORMAT_VERSION,
                user: user.to_string(),
                model: fingerprint.to_string(),
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
        let timeout = Duration::from_secs_f32(self.cfg.auth.timeout_secs);
        match self.attempt(&template, timeout, lang, emit) {
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

/// Discard frames the driver queued while we were not reading (e.g. during a
/// prompt), so the next frame shows the present.
fn flush(cam: &mut Camera) {
    for _ in 0..CAMERA_BUFFERS {
        let _ = cam.frame();
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
