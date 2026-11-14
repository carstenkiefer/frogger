//! Eye-state measurement used for blink-based liveness.
//!
//! Instead of a generic eye-landmark model we use per-user calibration: at
//! enrollment we store normalised eye patches with eyes *open* and *closed*.
//! At authentication every frame's eye patches are correlated against both
//! references. A static photo can never produce the open → closed → open
//! sequence, which is what makes "photo only" attacks fail even on IR.

use super::align::{Similarity, TEMPLATE};
use super::image::Image;
use serde::{Deserialize, Serialize};

/// Patch size in template pixels (112x112 aligned face space) and sampling step.
const PATCH_W: f32 = 30.0;
const PATCH_H: f32 = 18.0;
const STEP: f32 = 0.5;
pub const PATCH_COLS: usize = (PATCH_W / STEP) as usize;
pub const PATCH_ROWS: usize = (PATCH_H / STEP) as usize;

/// Zero-mean, unit-norm grayscale patches of both eyes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EyePair {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl EyePair {
    /// Extract both eye patches; `fwd` maps image → template coordinates.
    pub fn extract(img: &Image, fwd: &Similarity) -> EyePair {
        let gray;
        let src = if img.is_gray() {
            img
        } else {
            gray = img.to_gray();
            &gray
        };
        EyePair {
            left: patch(src, fwd, TEMPLATE[0]),
            right: patch(src, fwd, TEMPLATE[1]),
        }
    }
}

fn patch(img: &Image, fwd: &Similarity, center: [f32; 2]) -> Vec<f32> {
    let inv = fwd.inverse();
    let ox = center[0] - PATCH_W / 2.0;
    let oy = center[1] - PATCH_H / 2.0;
    let mut v = Vec::with_capacity(PATCH_COLS * PATCH_ROWS);
    for r in 0..PATCH_ROWS {
        for c in 0..PATCH_COLS {
            let s = inv.apply([ox + c as f32 * STEP, oy + r as f32 * STEP]);
            v.push(img.sample(s[0], s[1], 0));
        }
    }
    standardize(&mut v);
    v
}

/// Zero mean, unit L2 norm (so a dot product is the normalised cross-correlation).
pub fn standardize(v: &mut [f32]) {
    if v.is_empty() {
        return;
    }
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    v.iter_mut().for_each(|x| *x -= mean);
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-6 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn ncc(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Average several standardized patches into one reference patch.
pub fn average(patches: &[&[f32]]) -> Vec<f32> {
    let Some(first) = patches.first() else {
        return Vec::new();
    };
    let mut acc = vec![0f32; first.len()];
    for p in patches {
        for (a, x) in acc.iter_mut().zip(p.iter()) {
            *a += x;
        }
    }
    standardize(&mut acc);
    acc
}

/// Per-user eye references recorded during enrollment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EyeCalibration {
    pub open: EyePair,
    pub closed: EyePair,
    /// Minimum score separation required to call an eye open/closed.
    pub margin: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EyeState {
    Open,
    Closed,
    Unknown,
}

impl EyeCalibration {
    /// Build from enrollment samples. Returns `None` when open and closed
    /// references are too similar to be told apart reliably (e.g. the user
    /// did not close their eyes, or heavy glasses glare).
    pub fn from_samples(open: &[EyePair], closed: &[EyePair]) -> Option<EyeCalibration> {
        if open.is_empty() || closed.is_empty() {
            return None;
        }
        let o = EyePair {
            left: average(&open.iter().map(|p| p.left.as_slice()).collect::<Vec<_>>()),
            right: average(&open.iter().map(|p| p.right.as_slice()).collect::<Vec<_>>()),
        };
        let c = EyePair {
            left: average(&closed.iter().map(|p| p.left.as_slice()).collect::<Vec<_>>()),
            right: average(&closed.iter().map(|p| p.right.as_slice()).collect::<Vec<_>>()),
        };
        let mut cal = EyeCalibration {
            open: o,
            closed: c,
            margin: 0.0,
        };
        // Separation: how well do the samples themselves separate?
        let open_scores: Vec<f32> = open.iter().map(|p| cal.score(p)).collect();
        let closed_scores: Vec<f32> = closed.iter().map(|p| cal.score(p)).collect();
        let min_open = open_scores.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_closed = closed_scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let gap = min_open - max_closed;
        if gap < 0.10 {
            return None;
        }
        // Require each frame to be clearly on one side: a third of the gap.
        cal.margin = (gap / 3.0).clamp(0.03, 0.25);
        Some(cal)
    }

    /// Positive → looks like the open reference, negative → closed. Uses the
    /// *weaker* eye so a single winking eye or one occluded eye cannot pass.
    pub fn score(&self, p: &EyePair) -> f32 {
        let l = ncc(&p.left, &self.open.left) - ncc(&p.left, &self.closed.left);
        let r = ncc(&p.right, &self.open.right) - ncc(&p.right, &self.closed.right);
        // For "open" both must look open → min; for "closed" both must look
        // closed → max. Return the value with smaller magnitude.
        if l.signum() == r.signum() {
            if l > 0.0 {
                l.min(r)
            } else {
                l.max(r)
            }
        } else {
            0.0
        }
    }

    pub fn state(&self, p: &EyePair) -> EyeState {
        let s = self.score(p);
        if s > self.margin {
            EyeState::Open
        } else if s < -self.margin {
            EyeState::Closed
        } else {
            EyeState::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(seed: f32) -> EyePair {
        let mut l: Vec<f32> = (0..PATCH_COLS * PATCH_ROWS)
            .map(|i| ((i as f32) * seed).sin())
            .collect();
        let mut r = l.clone();
        standardize(&mut l);
        standardize(&mut r);
        EyePair { left: l, right: r }
    }

    #[test]
    fn calibration_separates() {
        let open = vec![pair(0.10), pair(0.1001)];
        let closed = vec![pair(0.37), pair(0.3702)];
        let cal = EyeCalibration::from_samples(&open, &closed).expect("separable");
        assert_eq!(cal.state(&pair(0.1002)), EyeState::Open);
        assert_eq!(cal.state(&pair(0.3701)), EyeState::Closed);
    }

    #[test]
    fn calibration_rejects_identical() {
        let open = vec![pair(0.1)];
        let closed = vec![pair(0.1)];
        assert!(EyeCalibration::from_samples(&open, &closed).is_none());
    }
}
