//! Face embeddings with SFace (OpenCV Zoo, Apache-2.0).
//!
//! Model: `face_recognition_sface_2021dec.onnx`, input 1x3x112x112 RGB float
//! in [0,255] (normalisation is part of the graph), output 128-d feature.

use super::detect::f32s;
use super::image::Image;
use anyhow::{Context, Result};
use std::path::Path;
use tract_onnx::prelude::*;

pub const DIM: usize = 128;

pub struct Embedder {
    model: std::sync::Arc<TypedRunnableModel>,
}

/// L2-normalised face descriptor.
pub type Embedding = Vec<f32>;

impl Embedder {
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("loading recognition model {}", path.display()))?
            .with_input_fact(0, f32::fact([1, 3, 112, 112]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Embedder { model })
    }

    /// `aligned` must be a 112x112 crop from [`super::align::align_face`].
    pub fn embed(&self, aligned: &Image) -> Result<Embedding> {
        assert_eq!((aligned.width, aligned.height), (112, 112));
        let plane = 112 * 112;
        let mut input = vec![0f32; 3 * plane];
        for i in 0..plane {
            for c in 0..3 {
                let v = if aligned.channels == 1 {
                    aligned.data[i]
                } else {
                    aligned.data[i * 3 + c]
                };
                input[c * plane + i] = v as f32;
            }
        }
        let tensor: Tensor = tract_ndarray::Array4::from_shape_vec((1, 3, 112, 112), input)?.into();
        let out = self.model.run(tvec!(tensor.into()))?;
        let mut v = f32s(&out[0])?;
        if v.len() != DIM {
            anyhow::bail!("unexpected embedding size {}", v.len());
        }
        normalize(&mut v);
        Ok(v)
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
}

/// Cosine similarity of two L2-normalised vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
