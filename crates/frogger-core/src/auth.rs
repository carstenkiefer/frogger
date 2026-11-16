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

use crate::config::{Config, DepthMode, LivenessMode};
use crate::store::UserTemplate;
use crate::vision::depth::DepthVerdict;
use crate::vision::eyes::EyeState;
use crate::vision::{cosine, Analysis, EyeSample, Face};
use serde::Serialize;

/// One processed camera frame.
#[derive(Clone)]
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
    /// Depth measurement of the analysed face (experimental, only with a
    /// depth camera and `camera.depth.mode` != off).
    pub depth: Option<DepthVerdict>,
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
    /// Depth data shows a flat surface (photo) or, in `required` mode, no
    /// valid depth confirmation was obtained.
    NotThreeDimensional,
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
            FailReason::NotThreeDimensional => "auth.not_3d",
        }
    }
}

/// Per eye (left, right): similarity to the user's open / closed reference.
type EyeComponents = [(f32, f32); 2];

/// Blink detection on the calibrated eye patches, relative to a running
/// baseline of the current track (see [`Verifier::advance_blink`]).
#[derive(Clone, Debug, Default)]
struct BlinkTracker {
    /// Last raw samples, for a median filter against single-frame outliers.
    raw: std::collections::VecDeque<EyeComponents>,
    /// Smoothed samples of the last `BLINK_WINDOW` while not closed:
    /// (t, open-minus-closed score per eye, closed similarity per eye).
    hist: std::collections::VecDeque<(f32, [f32; 2], [f32; 2])>,
    phase: BlinkPhase,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum BlinkPhase {
    #[default]
    Open,
    /// Lids went down; `strong` is the eye with the larger drop.
    Closed {
        since: f32,
        mins: [f32; 2],
        strong: usize,
    },
    Done,
}

impl BlinkTracker {
    fn idle(&self) -> bool {
        self.raw.is_empty() && self.phase == BlinkPhase::Open
    }
    fn done(&self) -> bool {
        self.phase == BlinkPhase::Done
    }
}

/// Value at fraction `q` of the sorted values (0 for none).
fn quantile(values: impl Iterator<Item = f32>, q: f32) -> f32 {
    let mut v: Vec<f32> = values.collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    v[((v.len() - 1) as f32 * q).round() as usize]
}

/// Baseline window and minimum history before a blink can start.
const BLINK_WINDOW: f32 = 1.0;
const BLINK_MIN_HISTORY: usize = 2;
/// Median filter length (samples). 1 = off: with strobing emitters a quick
/// blink may be visible in a single frame only (field data: no filter
/// detected 10 of 11 real blinks, a 3-sample median 9).
const BLINK_SMOOTH: usize = 1;
/// The second eye must move the same way by this share of the dip. IR
/// field data: one eye often changes far less than the other (reflections,
/// patch position), so both eyes need not drop by the full dip.
const BLINK_WEAK_EYE: f32 = 1.0 / 3.0;

/// Blink state from the eye state model (probability "closed" per eye).
#[derive(Clone, Copy, Debug, PartialEq)]
enum ModelBlink {
    NeedOpen,
    Open,
    Closed { since_ms: u32, samples: u32 },
    Done,
}

/// Eye state model: both eyes below → open, both above → closed.
const MODEL_OPEN_BELOW: f32 = 0.35;
const MODEL_CLOSED_ABOVE: f32 = 0.65;
/// Consecutive closed samples needed (filters single misclassified frames).
const MODEL_MIN_CLOSED: u32 = 2;

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
    /// Eye measurements taken (full and eye-only frames).
    pub eye_samples: u32,
    /// Largest drop of the open-minus-closed eye score (weaker eye) and the
    /// largest rise of the similarity to the closed reference – compare with
    /// the calibrated dip when a blink is not detected.
    pub max_eye_dip: f32,
    pub max_closed_rise: f32,
    /// Calibrated thresholds of the enrolled user (margin for "open", dip
    /// for a blink) and the best open score seen (weaker eye).
    pub eye_margin: f32,
    pub eye_dip_threshold: f32,
    pub max_open_score: Option<f32>,
    /// Furthest blink phase reached: 0 = eyes never classified open,
    /// 1 = open, 2 = lids moved down, 3 = blink completed.
    pub blink_phase: u8,
    /// Brightest face seen in a frame rejected as unlit.
    pub max_dark_face_brightness: f32,
    pub depth_live_frames: u32,
    pub depth_flat_frames: u32,
    pub depth_invalid_frames: u32,
    /// Largest nose prominence measured (mm, horizontal / vertical axis).
    pub depth_relief_mm: Option<(f32, f32)>,
    pub colour_frames: u32,
    pub track_breaks: u32,
    pub blink_detected: bool,
    /// Which method confirmed the blink: "patch" (calibrated eye patches) or
    /// "model" (eye state model).
    pub blink_method: Option<&'static str>,
    /// Eye state model: samples, the most clearly closed (lower of both
    /// eyes) and most clearly open (higher of both eyes) closed-probability,
    /// furthest phase (0 none, 1 open, 2 closed, 3 blink).
    pub model_samples: u32,
    pub model_max_closed: Option<f32>,
    pub model_min_open: Option<f32>,
    pub model_blink_phase: u8,
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
    /// The current track was confirmed three-dimensional by depth data.
    track_depth_live: bool,
    blink: BlinkTracker,
    model_blink: ModelBlink,
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
            track_depth_live: false,
            blink: BlinkTracker::default(),
            model_blink: ModelBlink::NeedOpen,
            stats: Stats::default(),
        }
    }

    fn reset_track(&mut self) {
        if self.track_matches > 0 || !self.blink.idle() || self.model_blink != ModelBlink::NeedOpen {
            self.stats.track_breaks += 1;
        }
        self.model_blink = ModelBlink::NeedOpen;
        self.track_matches = 0;
        self.track_best = 0.0;
        self.track_depth_live = false;
        self.blink = BlinkTracker::default();
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

        let mut depth_live = false;
        match obs.depth {
            Some(DepthVerdict::Flat {
                horizontal_mm,
                vertical_mm,
            }) => {
                // A flat (or singly curved) face in front of a depth camera:
                // photo. Drop all evidence of this track.
                self.stats.depth_flat_frames += 1;
                self.note_relief(horizontal_mm, vertical_mm);
                self.reset_track();
                return Progress::Continue;
            }
            Some(DepthVerdict::Live {
                horizontal_mm,
                vertical_mm,
            }) => {
                self.stats.depth_live_frames += 1;
                self.note_relief(horizontal_mm, vertical_mm);
                depth_live = true;
            }
            Some(DepthVerdict::Invalid) => self.stats.depth_invalid_frames += 1,
            None => {}
        }

        let (eye_state, eye_components) = match (&self.template.eyes, self.mode) {
            (Some(cal), LivenessMode::Blink) => (cal.state(&a.eyes), Some(cal.components(&a.eyes))),
            _ => (EyeState::Unknown, None),
        };
        let sim = self.similarity(a);
        self.last_sim = sim;
        self.stats.best_similarity = self.stats.best_similarity.max(sim);
        let threshold = self.cfg.auth.similarity_threshold;

        if sim >= threshold {
            self.track_depth_live |= depth_live;
            self.track_matches += 1;
            self.track_best = self.track_best.max(sim);
            self.stats.matching_frames += 1;
        } else if eye_state != EyeState::Closed
            && !a.eye_closed.is_some_and(|p| p[0] > 0.5 && p[1] > 0.5)
            && sim < threshold - IMPOSTOR_MARGIN
        {
            // Clearly someone/something else with open eyes: start over.
            self.reset_track();
            self.last_face = Some(a.face.clone());
            return Progress::Continue;
        }

        if let Some(c) = eye_components {
            self.advance_blink(obs.t, c, sim);
        }
        if let Some(p) = a.eye_closed {
            self.advance_model_blink(obs.t, p, sim);
        }

        let identity_ok = self.track_matches >= self.cfg.auth.min_matching_frames;
        let liveness_ok = match self.mode {
            LivenessMode::Blink => self.blink.done() || self.model_blink == ModelBlink::Done,
            LivenessMode::IrOnly => self.infrared,
        };
        let depth_ok = self.cfg.camera.depth.mode != DepthMode::Required || self.track_depth_live;
        if identity_ok && liveness_ok && depth_ok {
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
        if let (Some(cal), LivenessMode::Blink) = (&self.template.eyes, self.mode) {
            let components = cal.components(&e.eyes);
            self.advance_blink(t, components, self.last_sim);
        }
        if let Some(p) = e.eye_closed {
            self.advance_model_blink(t, p, self.last_sim);
        }
    }

    /// Blink detection with the eye state model (no per-user calibration):
    /// both eyes open, then both closed for at least two samples, then both
    /// open again within `max_closed_secs`. A photo cannot change state.
    fn advance_model_blink(&mut self, t: f32, closed: [f32; 2], sim: f32) {
        if self.mode != LivenessMode::Blink || !self.cfg.liveness.eye_model {
            return;
        }
        let t_ms = (t * 1000.0) as u32;
        let (lo, hi) = (closed[0].min(closed[1]), closed[0].max(closed[1]));
        let s = &mut self.stats;
        s.model_samples += 1;
        s.model_max_closed = Some(s.model_max_closed.map_or(lo, |m| m.max(lo)));
        s.model_min_open = Some(s.model_min_open.map_or(hi, |m| m.min(hi)));
        let both_open = hi < MODEL_OPEN_BELOW;
        let both_closed = lo > MODEL_CLOSED_ABOVE;
        let threshold = self.cfg.auth.similarity_threshold;
        let max_closed_ms = (self.cfg.liveness.max_closed_secs * 1000.0) as u32;
        log::debug!(
            "eye model t={t:.2} closed L {:.2} R {:.2} sim {sim:.3} state {:?}",
            closed[0],
            closed[1],
            self.model_blink
        );
        self.model_blink = match self.model_blink {
            ModelBlink::Done => ModelBlink::Done,
            ModelBlink::NeedOpen if both_open && sim >= threshold => ModelBlink::Open,
            ModelBlink::NeedOpen => ModelBlink::NeedOpen,
            ModelBlink::Open if both_closed => ModelBlink::Closed {
                since_ms: t_ms,
                samples: 1,
            },
            ModelBlink::Open => ModelBlink::Open,
            ModelBlink::Closed { since_ms, .. } if t_ms.saturating_sub(since_ms) > max_closed_ms => {
                ModelBlink::NeedOpen
            }
            ModelBlink::Closed { since_ms, samples } if both_closed => ModelBlink::Closed {
                since_ms,
                samples: samples + 1,
            },
            ModelBlink::Closed { samples, .. } if both_open && samples >= MODEL_MIN_CLOSED => {
                ModelBlink::Done
            }
            // Opened again after a single closed sample: not a blink.
            ModelBlink::Closed { .. } if both_open => ModelBlink::Open,
            closed @ ModelBlink::Closed { .. } => closed,
        };
        let phase = match self.model_blink {
            ModelBlink::NeedOpen => 0,
            ModelBlink::Open => 1,
            ModelBlink::Closed { .. } => 2,
            ModelBlink::Done => 3,
        };
        self.stats.model_blink_phase = self.stats.model_blink_phase.max(phase);
        if self.model_blink == ModelBlink::Done && self.stats.blink_method.is_none() {
            self.stats.blink_method = Some("model");
            self.stats.blink_detected = true;
        }
    }

    /// Blink detection on the calibrated eye patches, relative to a running
    /// baseline instead of fixed thresholds (field data: open-eye scores
    /// vary a lot between people, cameras and exposure):
    ///
    /// * per eye: score = similarity to the open minus similarity to the
    ///   closed reference (optionally median-filtered, `BLINK_SMOOTH`),
    /// * baseline: upper quartile of the scores in the last second,
    /// * blink start: one eye drops by at least the calibrated dip, the
    ///   other moves the same way by a third of it, **and** the similarity
    ///   to the closed reference rises on the dropping eye – a shaking or
    ///   shifted photo lowers both similarities and never qualifies,
    /// * blink end: both eyes rise again from their minimum (by the dip /
    ///   a third of it) within `max_closed_secs`.
    fn advance_blink(&mut self, t: f32, cur: EyeComponents, sim: f32) {
        let Some(cal) = &self.template.eyes else { return };
        let dip = cal.dip_threshold();
        let max_closed = self.cfg.liveness.max_closed_secs;
        let score = |c: (f32, f32)| c.0 - c.1;
        self.stats.eye_samples += 1;
        self.stats.eye_margin = cal.margin;
        self.stats.eye_dip_threshold = dip;
        log::debug!(
            "eyes t={t:.2} score L {:.3} R {:.3} (open L {:.3} R {:.3}, closed L {:.3} R {:.3}) sim {sim:.3} \
             dip {dip:.3} state {:?}",
            score(cur[0]),
            score(cur[1]),
            cur[0].0,
            cur[1].0,
            cur[0].1,
            cur[1].1,
            self.blink.phase
        );

        let b = &mut self.blink;
        if b.raw.len() == BLINK_SMOOTH {
            b.raw.pop_front();
        }
        b.raw.push_back(cur);
        let median = |f: &dyn Fn(&EyeComponents) -> f32| {
            let mut v: Vec<f32> = b.raw.iter().map(f).collect();
            v.sort_by(f32::total_cmp);
            v[v.len() / 2]
        };
        let s = [median(&|c| score(c[0])), median(&|c| score(c[1]))];
        let c = [median(&|c| c[0].1), median(&|c| c[1].1)];
        let st = &mut self.stats;
        let open_score = s[0].min(s[1]);
        st.max_open_score = Some(st.max_open_score.map_or(open_score, |m| m.max(open_score)));

        b.phase = match b.phase {
            BlinkPhase::Done => BlinkPhase::Done,
            BlinkPhase::Open => {
                let mut next = BlinkPhase::Open;
                if b.hist.len() >= BLINK_MIN_HISTORY {
                    let base = [
                        quantile(b.hist.iter().map(|h| h.1[0]), 0.75),
                        quantile(b.hist.iter().map(|h| h.1[1]), 0.75),
                    ];
                    let closed_base = [
                        quantile(b.hist.iter().map(|h| h.2[0]), 0.5),
                        quantile(b.hist.iter().map(|h| h.2[1]), 0.5),
                    ];
                    let drops = [base[0] - s[0], base[1] - s[1]];
                    let strong = if drops[0] >= drops[1] { 0 } else { 1 };
                    let rise = c[strong] - closed_base[strong];
                    st.max_eye_dip = st.max_eye_dip.max(drops[strong]);
                    st.max_closed_rise = st.max_closed_rise.max(rise);
                    // No absolute "looks closed" level: with strobing emitters
                    // a fast blink is often caught with the lids half down.
                    if drops[strong] >= dip && drops[1 - strong] >= BLINK_WEAK_EYE * dip && rise >= dip / 3.0
                    {
                        next = BlinkPhase::Closed {
                            since: t,
                            mins: s,
                            strong,
                        };
                    }
                }
                if next == BlinkPhase::Open {
                    b.hist.push_back((t, s, c));
                    while b.hist.front().is_some_and(|h| t - h.0 > BLINK_WINDOW) {
                        b.hist.pop_front();
                    }
                }
                next
            }
            BlinkPhase::Closed { since, mins, strong } => {
                let mins = [mins[0].min(s[0]), mins[1].min(s[1])];
                if t - since > max_closed {
                    b.hist.clear();
                    BlinkPhase::Open
                } else if s[strong] >= mins[strong] + dip
                    && s[1 - strong] >= mins[1 - strong] + BLINK_WEAK_EYE * dip
                {
                    BlinkPhase::Done
                } else {
                    BlinkPhase::Closed { since, mins, strong }
                }
            }
        };
        let phase = match b.phase {
            BlinkPhase::Open if b.hist.len() < BLINK_MIN_HISTORY => 0,
            BlinkPhase::Open => 1,
            BlinkPhase::Closed { .. } => 2,
            BlinkPhase::Done => 3,
        };
        st.blink_phase = st.blink_phase.max(phase);
        if b.phase == BlinkPhase::Done {
            st.blink_detected = true;
            st.blink_method.get_or_insert("patch");
        }
    }

    /// Most specific reason why the attempt did not succeed.
    fn note_relief(&mut self, h: f32, v: f32) {
        let best = self
            .stats
            .depth_relief_mm
            .unwrap_or((f32::NEG_INFINITY, f32::NEG_INFINITY));
        if h.min(v) > best.0.min(best.1) {
            self.stats.depth_relief_mm = Some((h, v));
        }
    }

    pub fn failure_reason(&self) -> FailReason {
        let s = &self.stats;
        if s.depth_flat_frames > 0 && s.depth_flat_frames * 2 >= s.lit_face_frames.max(1) {
            return FailReason::NotThreeDimensional;
        }
        if self.cfg.camera.depth.mode == DepthMode::Required
            && s.matching_frames >= self.cfg.auth.min_matching_frames
            && s.depth_live_frames == 0
        {
            return FailReason::NotThreeDimensional;
        }
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
            depth: None,
            analysis: Some(Analysis {
                face: face(x),
                face_count: 1,
                embedding: emb(angle),
                eyes: eye(eyes),
                eye_closed: None,
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
                eye_closed: None,
                face_brightness: 120.0,
            }),
            emitter_off: false,
            depth: None,
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
            depth: None,
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

    fn mix(a: &[f32], b: &[f32], w: f32) -> Vec<f32> {
        let mut v: Vec<f32> = a.iter().zip(b).map(|(x, y)| (1.0 - w) * x + w * y).collect();
        standardize(&mut v);
        v
    }

    /// Eyes between open (0.0) and closed (1.0), per eye.
    fn lids(left: f32, right: f32) -> EyePair {
        let (o, c) = (eye(OPEN), eye(CLOSED));
        EyePair {
            left: mix(&o.left, &c.left, left),
            right: mix(&o.right, &c.right, right),
        }
    }

    fn obs_pair(t: f32, pair: EyePair) -> Observation {
        let mut o = obs(t, 0.1, OPEN, 100.0);
        o.analysis.as_mut().unwrap().eyes = pair;
        o
    }

    fn eyes_only(t: f32, pair: EyePair) -> Observation {
        Observation {
            t,
            chroma: 0.0,
            analysis: None,
            eyes_only: Some(EyeSample {
                eyes: pair,
                eye_closed: None,
                face_brightness: 120.0,
            }),
            emitter_off: false,
            depth: None,
        }
    }

    #[test]
    fn half_closed_blink_is_detected() {
        // Fast blinks are usually caught with the lids half way down.
        let cfg = Config::default();
        let seq = [
            obs_pair(0.0, lids(0.0, 0.0)),
            obs_pair(0.1, lids(0.0, 0.0)),
            obs_pair(0.2, lids(0.0, 0.0)),
            obs_pair(0.3, lids(0.5, 0.5)),
            obs_pair(0.4, lids(0.0, 0.0)),
        ];
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn blink_seen_in_single_eye_only_frame_is_detected() {
        // Strobing emitter: the blink falls between two full analyses.
        let cfg = Config::default();
        let seq = [
            obs_pair(0.0, lids(0.0, 0.0)),
            obs_pair(0.2, lids(0.0, 0.0)),
            obs_pair(0.4, lids(0.0, 0.0)),
            eyes_only(0.47, lids(0.6, 0.6)),
            obs_pair(0.6, lids(0.0, 0.0)),
        ];
        assert!(matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn shaking_or_shifted_photo_is_not_a_blink() {
        // Misalignment lowers the similarity to the open reference, but the
        // patch does not become more similar to the closed one.
        let cfg = Config::default();
        let shifted = |k: usize| {
            let mut p = eye(OPEN);
            p.left.rotate_left(k);
            p.right.rotate_left(k);
            p
        };
        let noisy = || {
            let mut p = eye(OPEN);
            for (i, v) in p.left.iter_mut().enumerate() {
                *v += ((i as f32 * 12.9898).sin() * 43758.545).fract() * 0.08;
            }
            standardize(&mut p.left);
            p.right = p.left.clone();
            p
        };
        for distortion in [shifted(1), shifted(2), shifted(5), noisy()] {
            let mut seq: Vec<Observation> =
                (0..4).map(|i| obs_pair(i as f32 * 0.1, lids(0.0, 0.0))).collect();
            seq.push(obs_pair(0.4, distortion.clone()));
            seq.push(obs_pair(0.5, lids(0.0, 0.0)));
            seq.push(obs_pair(0.6, distortion));
            seq.push(obs_pair(0.7, lids(0.0, 0.0)));
            assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
        }
    }

    #[test]
    fn one_eye_moving_is_not_a_blink() {
        // Wink, or a print with one cut-out eye.
        let cfg = Config::default();
        let seq = [
            obs_pair(0.0, lids(0.0, 0.0)),
            obs_pair(0.1, lids(0.0, 0.0)),
            obs_pair(0.2, lids(0.0, 0.0)),
            obs_pair(0.3, lids(1.0, 0.0)),
            obs_pair(0.4, lids(0.0, 0.0)),
            obs_pair(0.5, lids(0.0, 1.0)),
            obs_pair(0.6, lids(0.0, 0.0)),
        ];
        assert!(!matches!(run(&cfg, &seq).0, Progress::Success { .. }));
    }

    fn with_depth(mut o: Observation, v: DepthVerdict) -> Observation {
        o.depth = Some(v);
        o
    }
    const LIVE: DepthVerdict = DepthVerdict::Live {
        horizontal_mm: 30.0,
        vertical_mm: 25.0,
    };
    const FLAT: DepthVerdict = DepthVerdict::Flat {
        horizontal_mm: 1.0,
        vertical_mm: 0.5,
    };

    fn blink_seq(d: Option<DepthVerdict>) -> Vec<Observation> {
        [(0.0, OPEN), (0.1, OPEN), (0.2, OPEN), (0.3, CLOSED), (0.4, OPEN)]
            .iter()
            .map(|&(t, e)| {
                let o = obs(t, 0.1, e, 100.0);
                match d {
                    Some(v) => with_depth(o, v),
                    None => o,
                }
            })
            .collect()
    }

    #[test]
    fn depth_off_or_absent_changes_nothing() {
        let cfg = Config::default(); // mode = off
        assert!(matches!(run(&cfg, &blink_seq(None)).0, Progress::Success { .. }));
        let mut auto = Config::default();
        auto.camera.depth.mode = DepthMode::Auto;
        assert!(matches!(run(&auto, &blink_seq(None)).0, Progress::Success { .. }));
        // Invalid depth data (projector covered) falls back to IR + blink.
        assert!(matches!(
            run(&auto, &blink_seq(Some(DepthVerdict::Invalid))).0,
            Progress::Success { .. }
        ));
    }

    #[test]
    fn flat_face_is_rejected_in_auto_mode() {
        let mut cfg = Config::default();
        cfg.camera.depth.mode = DepthMode::Auto;
        let (p, r) = run(&cfg, &blink_seq(Some(FLAT)));
        assert!(!matches!(p, Progress::Success { .. }));
        assert_eq!(r, FailReason::NotThreeDimensional);
        assert!(matches!(
            run(&cfg, &blink_seq(Some(LIVE))).0,
            Progress::Success { .. }
        ));
    }

    #[test]
    fn required_mode_needs_live_depth() {
        let mut cfg = Config::default();
        cfg.camera.depth.mode = DepthMode::Required;
        assert!(matches!(
            run(&cfg, &blink_seq(Some(LIVE))).0,
            Progress::Success { .. }
        ));
        let (p, r) = run(&cfg, &blink_seq(Some(DepthVerdict::Invalid)));
        assert!(!matches!(p, Progress::Success { .. }));
        assert_eq!(r, FailReason::NotThreeDimensional);
        assert!(!matches!(run(&cfg, &blink_seq(None)).0, Progress::Success { .. }));
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

    // ---- eye state model ------------------------------------------------

    /// Full frame of the enrolled user whose eye patches stay open (so only
    /// the eye state model can see the blink), with model probabilities.
    fn model_obs(t: f32, closed: [f32; 2]) -> Observation {
        let mut o = obs(t, 0.1, OPEN, 100.0);
        o.analysis.as_mut().unwrap().eye_closed = Some(closed);
        o
    }

    fn model_eyes(t: f32, closed: [f32; 2]) -> Observation {
        Observation {
            t,
            chroma: 0.0,
            analysis: None,
            eyes_only: Some(EyeSample {
                eyes: eye(OPEN),
                eye_closed: Some(closed),
                face_brightness: 120.0,
            }),
            emitter_off: false,
            depth: None,
        }
    }

    fn model_cfg() -> Config {
        let mut c = Config::default();
        c.liveness.eye_model = true;
        c
    }

    const O: [f32; 2] = [0.05, 0.1];
    const C: [f32; 2] = [0.95, 0.9];

    fn run_stats(cfg: &Config, seq: &[Observation]) -> (Progress, Stats) {
        let t = template();
        let mut v = Verifier::new(cfg, &t, true);
        let mut last = Progress::Continue;
        for o in seq {
            last = v.feed(o);
            if matches!(last, Progress::Success { .. }) {
                break;
            }
        }
        (last, v.stats.clone())
    }

    #[test]
    fn model_blink_succeeds_without_patch_blink() {
        let cfg = model_cfg();
        let seq = [
            model_obs(0.0, O),
            model_obs(0.1, O),
            model_obs(0.2, O),
            model_eyes(0.3, C),
            model_eyes(0.4, C),
            model_eyes(0.5, O),
            model_obs(0.6, O),
        ];
        let (p, s) = run_stats(&cfg, &seq);
        assert!(matches!(p, Progress::Success { .. }), "{p:?}");
        assert_eq!(s.blink_method, Some("model"));
        assert_eq!(s.model_blink_phase, 3);
    }

    #[test]
    fn model_needs_two_closed_samples_both_eyes_and_time_limit() {
        let cfg = model_cfg();
        let opening = [model_obs(0.0, O), model_obs(0.1, O), model_obs(0.2, O)];
        // A single misclassified frame is not a blink.
        let mut one = opening.to_vec();
        one.extend([model_eyes(0.3, C), model_eyes(0.4, O), model_obs(0.5, O)]);
        assert!(!matches!(run_stats(&cfg, &one).0, Progress::Success { .. }));
        // Winking (one eye) is not a blink.
        let mut wink = opening.to_vec();
        wink.extend([
            model_eyes(0.3, [0.95, 0.1]),
            model_eyes(0.4, [0.95, 0.1]),
            model_eyes(0.5, O),
            model_obs(0.6, O),
        ]);
        assert!(!matches!(run_stats(&cfg, &wink).0, Progress::Success { .. }));
        // Eyes closed far longer than max_closed_secs: not a blink.
        let mut long = opening.to_vec();
        long.extend((0..40).map(|i| model_eyes(0.3 + i as f32 * 0.1, C)));
        long.extend([model_eyes(4.4, O), model_obs(4.5, O)]);
        assert!(!matches!(run_stats(&cfg, &long).0, Progress::Success { .. }));
    }

    #[test]
    fn model_static_photo_and_disabled_model_never_succeed() {
        let cfg = model_cfg();
        let photo: Vec<Observation> = (0..60).map(|i| model_obs(i as f32 * 0.1, O)).collect();
        assert_eq!(run_stats(&cfg, &photo).0, Progress::NeedBlink);

        let mut off = Config::default();
        off.liveness.eye_model = false;
        let seq = [
            model_obs(0.0, O),
            model_obs(0.1, O),
            model_obs(0.2, O),
            model_eyes(0.3, C),
            model_eyes(0.4, C),
            model_eyes(0.5, O),
            model_obs(0.6, O),
        ];
        assert!(!matches!(run_stats(&off, &seq).0, Progress::Success { .. }));
    }

    #[test]
    fn closed_eyes_by_model_do_not_count_as_impostor() {
        // A closed-eye frame with a low embedding similarity must not reset
        // the track (closed eyes lower the similarity in IR).
        let cfg = model_cfg();
        let mut closed_low = obs(0.4, 1.3, OPEN, 100.0);
        closed_low.analysis.as_mut().unwrap().eye_closed = Some(C);
        let seq = [
            model_obs(0.0, O),
            model_obs(0.1, O),
            model_obs(0.2, O),
            model_eyes(0.3, C),
            closed_low,
            model_eyes(0.5, O),
            model_obs(0.6, O),
        ];
        let (p, s) = run_stats(&cfg, &seq);
        assert!(matches!(p, Progress::Success { .. }), "{p:?}");
        assert_eq!(s.track_breaks, 0);
    }

    // ---- recorded field data ----------------------------------------------

    /// Replays `tests/data/blink-trace-ir.txt` through the patch blink
    /// detector; returns per attempt whether a blink was detected.
    fn replay_trace() -> Vec<bool> {
        let data = include_str!("../tests/data/blink-trace-ir.txt");
        let mut t = template();
        t.eyes = Some(EyeCalibration {
            open: eye(OPEN),
            closed: eye(CLOSED),
            margin: 0.25,
        });
        let cfg = Config::default();
        let mut results = Vec::new();
        let mut v: Option<Verifier> = None;
        for line in data.lines().filter(|l| !l.starts_with('#')) {
            if line.starts_with("attempt") {
                if let Some(v) = v.take() {
                    results.push(v.blink.done());
                }
                v = Some(Verifier::new(&cfg, &t, true));
                continue;
            }
            let n: Vec<f32> = line.split_whitespace().map(|x| x.parse().unwrap()).collect();
            let comps = [(n[1], n[3]), (n[2], n[4])];
            v.as_mut().unwrap().advance_blink(n[0], comps, 0.8);
        }
        results.extend(v.map(|v| v.blink.done()));
        results
    }

    #[test]
    fn recorded_ir_blinks_are_detected() {
        let r = replay_trace();
        assert_eq!(r.len(), 11);
        let detected = r.iter().filter(|&&b| b).count();
        // The previous detector found 2 of these 11 real blinks.
        assert!(detected >= 10, "only {detected} of 11 detected: {r:?}");
    }

    #[test]
    fn recorded_open_eyes_without_blink_are_not_a_blink() {
        // Attempt 3 from 3.2 s on: eyes open, natural noise, no blink.
        let data = include_str!("../tests/data/blink-trace-ir.txt");
        let block = data.split("\nattempt ").find(|b| b.starts_with("3\n")).unwrap();
        let mut t = template();
        t.eyes = Some(EyeCalibration {
            open: eye(OPEN),
            closed: eye(CLOSED),
            margin: 0.25,
        });
        let cfg = Config::default();
        let mut v = Verifier::new(&cfg, &t, true);
        for line in block.lines().skip(1) {
            let n: Vec<f32> = line.split_whitespace().map(|x| x.parse().unwrap()).collect();
            if (3.2..=4.65).contains(&n[0]) {
                v.advance_blink(n[0], [(n[1], n[3]), (n[2], n[4])], 0.8);
            }
        }
        assert!(!v.blink.done());
    }
}
