//! Eye state (open/closed) with Intel's `open-closed-eye-0001` (Open Model
//! Zoo, Apache-2.0), trained on infrared eye images (MRL Eye Dataset).
//!
//! Model: input 1x3x32x32 BGR, normalised as (x - 127) / 255; output the
//! two-class scores [open, closed]. The graph ends in a naive softmax
//! (Exp / ReduceSum / Div) that overflows to NaN for large logits, so the
//! logits (node "16") are read instead and a stable softmax is applied here.

use super::align::{Similarity, TEMPLATE};
use super::detect::f32s;
use super::image::Image;
use anyhow::{Context, Result};
use std::path::Path;
use tract_onnx::prelude::*;

const SIZE: usize = 32;
/// Side of the square eye crop in aligned-template units (the template's
/// eye distance is ~35): the eye with lids and a little skin around it, like
/// the training images.
const CROP: f32 = 30.0;

pub struct EyeStateModel {
    model: std::sync::Arc<TypedRunnableModel>,
}

impl EyeStateModel {
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("loading eye state model {}", path.display()))?
            .with_input_fact(0, f32::fact([1, 3, SIZE, SIZE]).into())?
            .with_outputs_by_name(["16"])?
            .into_optimized()?
            .into_runnable()?;
        Ok(EyeStateModel { model })
    }

    /// Probability that each eye (image-left, image-right) is closed.
    /// `fwd` maps image → aligned-template coordinates.
    pub fn closed(&self, img: &Image, fwd: &Similarity) -> Result<[f32; 2]> {
        Ok([
            self.closed_prob(&crop(img, fwd, TEMPLATE[0]))?,
            self.closed_prob(&crop(img, fwd, TEMPLATE[1]))?,
        ])
    }

    fn closed_prob(&self, gray: &[f32]) -> Result<f32> {
        let plane = SIZE * SIZE;
        let mut input = vec![0f32; 3 * plane];
        for c in 0..3 {
            input[c * plane..(c + 1) * plane].copy_from_slice(gray);
        }
        let tensor: Tensor = tract_ndarray::Array4::from_shape_vec((1, 3, SIZE, SIZE), input)?.into();
        let out = self.model.run(tvec!(tensor.into()))?;
        let logits = f32s(&out[0])?;
        anyhow::ensure!(logits.len() == 2, "unexpected eye state output {}", logits.len());
        Ok(softmax2(logits[0], logits[1]))
    }
}

/// Stable two-class softmax: probability of the second class.
fn softmax2(open: f32, closed: f32) -> f32 {
    let m = open.max(closed);
    let (o, c) = ((open - m).exp(), (closed - m).exp());
    c / (o + c)
}

/// Upright, normalised 32x32 grayscale crop around a template eye position.
fn crop(img: &Image, fwd: &Similarity, center: [f32; 2]) -> Vec<f32> {
    let inv = fwd.inverse();
    let step = CROP / SIZE as f32;
    let (ox, oy) = (center[0] - CROP / 2.0, center[1] - CROP / 2.0);
    let mut v = Vec::with_capacity(SIZE * SIZE);
    for r in 0..SIZE {
        for c in 0..SIZE {
            let p = inv.apply([ox + (c as f32 + 0.5) * step, oy + (r as f32 + 0.5) * step]);
            let g = if img.channels == 1 {
                img.sample(p[0], p[1], 0)
            } else {
                // BGR/RGB order does not matter for the gray average.
                (img.sample(p[0], p[1], 0) + img.sample(p[0], p[1], 1) + img.sample(p[0], p[1], 2)) / 3.0
            };
            v.push((g - 127.0) / 255.0);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn softmax_is_stable() {
        assert!((softmax2(0.0, 0.0) - 0.5).abs() < 1e-6);
        assert!(softmax2(1000.0, 0.0) < 1e-6);
        assert!((softmax2(0.0, 1000.0) - 1.0).abs() < 1e-6);
        assert!(softmax2(4.806984, 7.3240714) > 0.92);
    }

    #[test]
    fn crop_is_upright_and_normalised() {
        // Identity transform: template coordinates are image coordinates.
        let id = Similarity::estimate(&TEMPLATE, &TEMPLATE);
        let img = Image::from_gray(112, 112, vec![127; 112 * 112]);
        let c = crop(&img, &id, TEMPLATE[0]);
        assert_eq!(c.len(), SIZE * SIZE);
        assert!(c.iter().all(|v| v.abs() < 0.01));
    }
}
