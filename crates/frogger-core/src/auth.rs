//! Authentication decision logic.
//!
//! [`Verifier`] is a pure state machine fed with per-frame observations. It
//! does no I/O, which keeps the security-relevant rules unit-testable.
//!
//! Success requires, within one *continuous face track*:
//! * at least `min_matching_frames` frames whose embedding matches the
//!   enrolled user (cosine similarity ≥ threshold),
//! * on IR cameras: every frame monochrome and the face lit by the emitter
//!   (screens show up black under IR, so phone/monitor replays fail),
//! * in `blink` mode: an open → closed → open eye sequence measured against
//!   the user's own eye calibration (a printed photo cannot blink),
//! * exactly one face in view (configurable).
//!
//! A track breaks when the face disappears, jumps (IoU below threshold) or a
//! clearly different identity appears; a break resets all evidence.

use crate::config::{Config, LivenessMode};
use crate::store::UserTemplate;
use crate::vision::eyes::EyeState;
use crate::vision::{cosine, Analysis, EyeSample, Face};
use serde::Serialize;

/// One processed camera frame.
pub struct Observation {
    /// Seconds since the start of the attempt.
    pub t: f32,
    /// Chroma of the raw frame (0 = monochrome).
    pub chroma: f32,
    pub analysis: Option<Analysis>,
    /// Eye-only measurement at the last known face position, used between
    /// full analyses so short blinks are not missed on slow CPUs.
    pub eyes_only: Option<EyeSample>,
    /// Frame taken while a strobing IR emitter was off. Carries no evidence
    /// either way and is ignored (see [`StrobeFilter`]).
    pub emitter_off: bool,
}

/// Recognises the "off" frames of IR emitters that strobe (many laptop IR
/// cameras light every second frame): a frame much darker than the recent
/// maximum is an off frame.
#[derive(Default)]
pub struct StrobeFilter {
    recent: std::collections::VecDeque<f32>,
}

impl StrobeFilter {
    const WINDOW: usize = 4;
    const RATIO: f32 = 0.7;

    /// Feed the mean brightness of a frame; returns true for an off frame.
    pub fn is_off(&mut self, mean: f32) -> bool {
        let max = self.recent.iter().cloned().fold(0.0f32, f32::max);
        if self.recent.len() == Self::WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(mean);
        max > 0.0 && mean < max * Self::RATIO
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    NoFace,
    NotRecognized,
    NoBlink,
    NotIlluminated,
    MultipleFaces,
    NotInfrared,
}

impl FailReason {
    /// i18n key for the user facing message.
    pub fn message_key(self) -> &'static str {
        match self {
            FailReason::NoFace => "auth.no_face",
            FailReason::NotRecognized => "auth.not_recognized",
            FailReason::NoBlink => "auth.no_blink",
            FailReason::NotIlluminated => "auth.not_illuminated",
            FailReason::MultipleFaces => "auth.multiple_faces",
            FailReason::NotInfrared => "auth.not_infrared",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blink {
    NeedOpen,
    Open,
    Closed { since_ms: u32 },
    Done,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    Continue,
    /// Identity confirmed but liveness still pending – UI may prompt.
    NeedBlink,
    Success {
        similarity: f32,
    },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Stats {
    pub frames: u32,
    pub frames_with_face: u32,
    pub best_similarity: f32,
    pub matching_frames: u32,
    pub multi_face_frames: u32,
    pub dark_frames: u32,
    pub emitter_off_frames: u32,
    pub lit_face_frames: u32,
    /// Mean brightness (0-255) of faces in lit frames – compare with
    /// `liveness.min_ir_face_brightness` when tuning weak emitters.
    pub mean_face_brightness: f32,
    /// Brightest face seen in a frame rejected as unlit.
    pub max_dark_face_brightness: f32,
    pub colour_frames: u32,
    pub track_breaks: u32,
    pub blink_detected: bool,
}

pub struct Verifier<'a> {
    cfg: &'a Config,
    template: &'a UserTemplate,
    infrared: bool,
    mode: LivenessMode,
    last_face: Option<Face>,
    track_matches: u32,
    track_best: f32,
    last_sim: f32,
    /// Consecutive analysed frames without a face.
    missing: u32,
    blink: Blink,
    pub stats: Stats,
}

/// Analysed frames without a face tolerated before the track is dropped
/// (motion blur, a frame the detector misses).
const MAX_MISSING: u32 = 2;

/// Similarity below which an open-eyed face is treated as a *different*
/// person and breaks the track.
const IMPOSTOR_MARGIN: f32 = 0.15;

impl<'a> Verifier<'a> {
    pub fn new(cfg: &'a Config, template: &'a UserTemplate, infrared: bool) -> Self {
        let mut mode = cfg.effective_liveness();
        // Without eye calibration we can't run the blink challenge; refuse to
        // silently degrade on devices where policy demands it.
        if mode == LivenessMode::Blink && template.eyes.is_none() {
            log::warn!(
                "template of {} lacks eye calibration; blink challenge cannot pass",
                template.user
            );
        }
        if !infrared {
            // RGB is insecure regardless; blink still raises the bar a little.
            mode = LivenessMode::Blink;
        }
        Verifier {
            cfg,
            template,
            infrared,
            mode,
            last_face: None,
            track_matches: 0,
            track_best: 0.0,
            last_sim: 0.0,
            missing: 0,
            blink: Blink::NeedOpen,
            stats: Stats::default(),
        }
    }

    fn reset_track(&mut self) {
        if self.track_matches > 0 || self.blink != Blink::NeedOpen {
            self.stats.track_breaks += 1;
        }
        self.track_matches = 0;
        self.track_best = 0.0;
        self.blink = Blink::NeedOpen;
        self.last_face = None;
    }

    pub fn similarity(&self, a: &Analysis) -> f32 {
        self.template
            .samples
            .iter()
            .map(|s| cosine(&s.embedding, &a.embedding))
            .fold(f32::NEG_INFINITY, f32::max)
    }

    pub fn feed(&mut self, obs: &Observation) -> Progress {
        self.stats.frames += 1;
        let lv = &self.cfg.liveness;

        if self.infrared && obs.chroma >= 2.0 {
            self.stats.colour_frames += 1;
            self.reset_track();
            return Progress::Continue;
        }
        if obs.emitter_off {
            self.stats.emitter_off_frames += 1;
            return Progress::Continue;
        }
        if let (None, Some(e)) = (&obs.analysis, &obs.eyes_only) {
            self.feed_eyes(obs.t, e);
            return Progress::Continue;
        }
        let Some(a) = &obs.analysis else {
            self.missing += 1;
            if self.missing > MAX_MISSING {
                self.reset_track();
            }
            return Progress::Continue;
        };
        self.stats.frames_with_face += 1;
        if lv.reject_multiple_faces && a.face_count > 1 {
            self.stats.multi_face_frames += 1;
            self.reset_track();
            return Progress::Continue;
        }
        if self.infrared && a.face_brightness < lv.min_ir_face_brightness {
            // Not lit by the emitter: a screen (black in IR) or an emitter
            // off phase. Such a frame proves nothing, so it is ignored – it
            // can neither contribute to nor reset a track.
            self.stats.dark_frames += 1;
            self.stats.max_dark_face_brightness = self.stats.max_dark_face_brightness.max(a.face_brightness);
            return Progress::Continue;
        }
        self.missing = 0;
        self.stats.lit_face_frames += 1;
        let n = self.stats.lit_face_frames as f32;
        self.stats.mean_face_brightness += (a.face_brightness - self.stats.mean_face_brightness) / n;
        if let Some(prev) = &self.last_face {
            if prev.iou(&a.face) < lv.min_track_iou {
                self.reset_track();
            }
        }
        self.last_face = Some(a.face.clone());

        let eye_state = match (&self.template.eyes, self.mode) {
            (Some(cal), LivenessMode::Blink) => cal.state(&a.eyes),
            _ => EyeState::Unknown,
        };
        let sim = self.similarity(a);
        self.last_sim = sim;
        self.stats.best_similarity = self.stats.best_similarity.max(sim);
        let threshold = self.cfg.auth.similarity_threshold;

        if sim >= threshold {
            self.track_matches += 1;
            self.track_best = self.track_best.max(sim);
            self.stats.matching_frames += 1;
        } else if eye_state != EyeState::Closed && sim < threshold - IMPOSTOR_MARGIN {
            // Clearly someone/something else with open eyes: start over.
            self.reset_track();
            self.last_face = Some(a.face.clone());
            return Progress::Continue;
        }

        self.advance_blink(obs.t, eye_state, sim);

        let identity_ok = self.track_matches >= self.cfg.auth.min_matching_frames;
        let liveness_ok = match self.mode {
            LivenessMode::Blink => self.blink == Blink::Done,
            LivenessMode::IrOnly => self.infrared,
        };
        if identity_ok && liveness_ok {
            Progress::Success {
                similarity: self.track_best,
            }
        } else if identity_ok {
            Progress::NeedBlink
        } else {
            Progress::Continue
        }
    }

    /// Eye-only frames only advance the blink state of a live track; success
    /// is decided exclusively on fully analysed frames.
    fn feed_eyes(&mut self, t: f32, e: &EyeSample) {
        if self.last_face.is_none() {
            return;
        }
        if self.infrared && e.face_brightness < self.cfg.liveness.min_ir_face_brightness {
            self.stats.dark_frames += 1;
            return;
        }
        let state = match (&self.template.eyes, self.mode) {
            (Some(cal), LivenessMode::Blink) => cal.state(&e.eyes),
            _ => return,
        };
        self.advance_blink(t, state, self.last_sim);
    }

    fn advance_blink(&mut self, t: f32, eye_state: EyeState, sim: f32) {
        let lv = &self.cfg.liveness;
        let threshold = self.cfg.auth.similarity_threshold;
        let t_ms = (t * 1000.0) as u32;
        self.blink = match (self.blink, eye_state) {
            (Blink::Done, _) => Blink::Done,
            (Blink::NeedOpen, EyeState::Open) if sim >= threshold => Blink::Open,
            (Blink::NeedOpen, _) => Blink::NeedOpen,
            (Blink::Open, EyeState::Closed) => Blink::Closed { since_ms: t_ms },
            (Blink::Open, _) => Blink::Open,
            (Blink::Closed { since_ms }, s) => {
                let closed_for = t_ms.saturating_sub(since_ms) as f32 / 1000.0;
                if closed_for > lv.max_closed_secs {
                    Blink::NeedOpen
                } else if s == EyeState::Open && sim >= threshold {
                    Blink::Done
                } else {
                    Blink::Closed { since_ms }
                }
            }
        };
        self.stats.blink_detected = self.blink == Blink::Done;
    }

    /// Most specific reason why the attempt did not succeed.
    pub fn failure_reason(&self) -> FailReason {
        let s = &self.stats;
        if s.colour_frames > 0 && s.colour_frames * 2 >= s.frames {
            FailReason::NotInfrared
        } else if s.frames_with_face == 0 {
            FailReason::NoFace
        } else if s.multi_face_frames * 2 >= s.frames_with_face {
            FailReason::MultipleFaces
        } else if s.lit_face_frames == 0 && s.dark_frames > 0 {
            FailReason::NotIlluminated
        } else if s.matching_frames >= self.cfg.auth.min_matching_frames {
            FailReason::NoBlink
        } else {
            FailReason::NotRecognized
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Sample, FORMAT_VERSION};
    use crate::vision::eyes::{standardize, EyeCalibration, EyePair, PATCH_COLS, PATCH_ROWS};

    fn eye(seed: f32) -> EyePair {
        let mut v: Vec<f32> = (0..PATCH_COLS * PATCH_ROWS)
            .map(|i| ((i as f32) * seed).sin())
            .collect();
        standardize(&mut v);
        EyePair {
            left: v.clone(),
            right: v,
        }
    }
    const OPEN: f32 = 0.11;
    const CLOSED: f32 = 0.47;

    fn emb(angle: f32) -> Vec<f32> {
        let mut v = vec![0f32; 128];
        v[0] = angle.cos();
        v[1] = angle.sin();
        v
    }

    fn template() -> UserTemplate {
        UserTemplate {
            version: FORMAT_VERSION,
            user: "alice".into(),
            model: "m".into(),
            camera_kind: "infrared".into(),
            camera_usb_id: None,
            label: None,
            samples: vec![Sample {
                embedding: emb(0.0),
                created: 0,
            }],
            eyes: EyeCalibration::from_samples(&[eye(OPEN)], &[eye(CLOSED)]),
            created: 0,
        }
    }

    fn face(x: f32) -> Face {
        Face {
            x,
            y: 100.0,
            w: 150.0,
            h: 180.0,
            score: 0.9,
            landmarks: [[0.0; 2]; 5],
        }
    }

    /// angle 0 = enrolled person; cos(angle) is the similarity.
    fn obs(t: f32, angle: f32, eyes: f32, x: f32) -> Observation {
        Observation {
            t,
            chroma: 0.0,
            eyes_only: None,
            emitter_off: false,
            analysis: Some(Analysis {
                face: face(x),
                face_count: 1,
                embedding: emb(angle),
                eyes: eye(eyes),
                face_brightness: 120.0,
                transform: crate::vision::align::Similarity {
                    a: 1.0,
                    b: 0.0,
                    tx: 0.0,
                    ty: 0.0,
                },
            }),
        }
    }

    fn run(cfg: &Config, seq: &[Observation]) -> (Progress, FailReason) {
        let t = template();
        let mut v = Verifier::new(cfg, &t, true);
        let mut last = Progress::Continue;
        for o in seq {
            last = v.feed(o);
            if matches!(last, Progress::Success { .. }) {
                break;
            }
        }
        (last, v.failure_reason())
    }

    #[test]
    fn genuine_user_with_blink_succeeds() {
        let cfg = Config::default();
        let seq = [
            obs(0.0, 0.1, OPEN, 100.0),
            obs(0.1, 0.1, OPEN, 101.0),
            obs(0.2, 0.1, OPEN, 102.0),
            obs(0.3, 0.6, CLOSED, 102.0),
            obs(0.4, 0.1, OPEN, 103.0),
        ];
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn static_photo_never_succeeds() {
        let cfg = Config::default();
        let seq: Vec<Observation> = (0..60).map(|i| obs(i as f32 * 0.1, 0.05, OPEN, 100.0)).collect();
        let (p, reason) = run(&cfg, &seq);
        assert_eq!(p, Progress::NeedBlink);
        assert_eq!(reason, FailReason::NoBlink);
    }

    #[test]
    fn photo_swap_breaks_track() {
        // Open-eye photo, then a jump to another position (swapped print with
        // closed eyes), then back: IoU breaks reset the challenge.
        let cfg = Config::default();
        let seq = [
            obs(0.0, 0.1, OPEN, 100.0),
            obs(0.1, 0.1, OPEN, 100.0),
            obs(0.2, 0.1, OPEN, 100.0),
            obs(0.3, 0.1, CLOSED, 260.0),
            obs(0.4, 0.1, OPEN, 100.0),
        ];
        assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn impostor_fails() {
        let cfg = Config::default();
        let seq = [
            obs(0.0, 1.3, OPEN, 100.0),
            obs(0.1, 1.3, OPEN, 100.0),
            obs(0.2, 1.3, CLOSED, 100.0),
            obs(0.3, 1.3, OPEN, 100.0),
        ];
        let (p, r) = run(&cfg, &seq);
        assert_eq!(p, Progress::Continue);
        assert_eq!(r, FailReason::NotRecognized);
    }

    #[test]
    fn dark_face_on_ir_is_rejected() {
        let cfg = Config::default();
        let mut seq: Vec<Observation> = (0..5).map(|i| obs(i as f32 * 0.1, 0.1, OPEN, 100.0)).collect();
        for o in &mut seq {
            o.analysis.as_mut().unwrap().face_brightness = 3.0;
        }
        let (p, r) = run(&cfg, &seq);
        assert_eq!(p, Progress::Continue);
        assert_eq!(r, FailReason::NotIlluminated);
    }

    #[test]
    fn colour_frames_on_ir_camera_rejected() {
        let cfg = Config::default();
        let mut seq: Vec<Observation> = (0..5).map(|i| obs(i as f32 * 0.1, 0.1, OPEN, 100.0)).collect();
        for o in &mut seq {
            o.chroma = 20.0;
        }
        assert_eq!(run(&cfg, &seq).1, FailReason::NotInfrared);
    }

    #[test]
    fn blink_seen_only_in_eye_frames_counts() {
        let cfg = Config::default();
        let eyes = |t: f32, e: f32| Observation {
            t,
            chroma: 0.0,
            analysis: None,
            eyes_only: Some(EyeSample {
                eyes: eye(e),
                face_brightness: 120.0,
            }),
            emitter_off: false,
        };
        let seq = [
            obs(0.0, 0.1, OPEN, 100.0),
            obs(0.2, 0.1, OPEN, 100.0),
            obs(0.4, 0.1, OPEN, 100.0),
            eyes(0.45, CLOSED),
            eyes(0.5, OPEN),
            obs(0.6, 0.1, OPEN, 101.0),
        ];
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
        // Eye frames without any analysed face are ignored.
        let seq2 = [eyes(0.0, OPEN), eyes(0.1, CLOSED), eyes(0.2, OPEN)];
        assert_eq!(run(&cfg, &seq2).0, Progress::Continue);
    }

    fn off(t: f32) -> Observation {
        Observation {
            t,
            chroma: 0.0,
            analysis: None,
            eyes_only: None,
            emitter_off: true,
        }
    }

    fn dark(mut o: Observation) -> Observation {
        o.analysis.as_mut().unwrap().face_brightness = 5.0;
        o
    }

    #[test]
    fn strobing_emitter_still_succeeds() {
        // Every second frame is an emitter-off frame, either filtered before
        // analysis or analysed with a dark face.
        let cfg = Config::default();
        let seq = [
            obs(0.00, 0.1, OPEN, 100.0),
            off(0.03),
            obs(0.07, 0.1, OPEN, 101.0),
            dark(obs(0.10, 0.1, OPEN, 101.0)),
            obs(0.13, 0.1, OPEN, 101.0),
            off(0.17),
            obs(0.20, 0.6, CLOSED, 102.0),
            dark(obs(0.23, 0.1, CLOSED, 102.0)),
            obs(0.27, 0.1, OPEN, 102.0),
        ];
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn photo_swap_hidden_in_dark_frame_is_detected() {
        // The swap happens while the emitter is off; continuity is still
        // checked against the last lit frame.
        let cfg = Config::default();
        let seq = [
            obs(0.0, 0.1, OPEN, 100.0),
            obs(0.1, 0.1, OPEN, 100.0),
            obs(0.2, 0.1, OPEN, 100.0),
            off(0.25),
            obs(0.3, 0.1, CLOSED, 260.0),
            off(0.35),
            obs(0.4, 0.1, OPEN, 100.0),
        ];
        assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn screen_only_reports_not_illuminated() {
        let cfg = Config::default();
        let seq: Vec<Observation> = (0..10)
            .map(|i| dark(obs(i as f32 * 0.1, 0.1, OPEN, 100.0)))
            .collect();
        let (p, r) = run(&cfg, &seq);
        assert_eq!(p, Progress::Continue);
        assert_eq!(r, FailReason::NotIlluminated);
    }

    #[test]
    fn strobe_filter_flags_off_frames() {
        let mut f = StrobeFilter::default();
        let flags: Vec<bool> = [120.0, 30.0, 118.0, 32.0, 121.0, 29.0]
            .iter()
            .map(|&m| f.is_off(m))
            .collect();
        assert_eq!(flags, vec![false, true, false, true, false, true]);
        // A steady camera never produces off frames.
        let mut g = StrobeFilter::default();
        assert!((0..20).all(|i| !g.is_off(100.0 + (i % 3) as f32)));
    }

    #[test]
    fn long_eye_closure_is_not_a_blink() {
        let cfg = Config::default();
        let mut seq = vec![
            obs(0.0, 0.1, OPEN, 100.0),
            obs(0.1, 0.1, OPEN, 100.0),
            obs(0.2, 0.1, OPEN, 100.0),
        ];
        for i in 0..30 {
            seq.push(obs(0.3 + i as f32 * 0.1, 0.1, CLOSED, 100.0));
        }
        seq.push(obs(3.5, 0.1, OPEN, 100.0));
        assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn ir_only_mode_needs_no_blink() {
        let mut cfg = Config::default();
        cfg.liveness.mode = LivenessMode::IrOnly;
        let seq: Vec<Observation> = (0..3).map(|i| obs(i as f32 * 0.1, 0.1, OPEN, 100.0)).collect();
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
        // …but a sensitive device forces the blink challenge back on.
        cfg.policy.sensitive_device = true;
        assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn multiple_faces_rejected() {
        let cfg = Config::default();
        let mut seq: Vec<Observation> = (0..6).map(|i| obs(i as f32 * 0.1, 0.1, OPEN, 100.0)).collect();
        for o in &mut seq {
            o.analysis.as_mut().unwrap().face_count = 2;
        }
        assert_eq!(run(&cfg, &seq).1, FailReason::MultipleFaces);
    }
}
