//! Face detection with YuNet (OpenCV Zoo, MIT licensed).
//!
//! Model: `face_detection_yunet_2023mar.onnx`, fixed input 1x3x640x640,
//! BGR, float in [0,255]. Outputs per stride (8/16/32): cls, obj, bbox, kps.

use super::image::Image;
use anyhow::{Context, Result};
use std::path::Path;
use tract_onnx::prelude::*;

pub const INPUT: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];

/// One detected face in source image coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Face {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub score: f32,
    /// Right eye, left eye (subject's perspective; i.e. image-left eye first),
    /// nose tip, right mouth corner, left mouth corner.
    pub landmarks: [[f32; 2]; 5],
}

impl Face {
    pub fn area(&self) -> f32 {
        self.w * self.h
    }
    pub fn center(&self) -> [f32; 2] {
        [self.x + self.w / 2.0, self.y + self.h / 2.0]
    }
    pub fn interocular(&self) -> f32 {
        let [a, b] = [self.landmarks[0], self.landmarks[1]];
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
    }
    pub fn iou(&self, o: &Face) -> f32 {
        let x1 = self.x.max(o.x);
        let y1 = self.y.max(o.y);
        let x2 = (self.x + self.w).min(o.x + o.w);
        let y2 = (self.y + self.h).min(o.y + o.h);
        let inter = (x2 - x1).max(0.0) * (y2 - y1).max(0.0);
        let union = self.area() + o.area() - inter;
        if union <= 0.0 {
            0.0
        } else {
            inter / union
        }
    }
}

pub struct Detector {
    model: std::sync::Arc<TypedRunnableModel>,
    pub score_threshold: f32,
    pub nms_threshold: f32,
}

impl Detector {
    pub fn load(path: &Path) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("loading detector model {}", path.display()))?
            .with_input_fact(0, f32::fact([1, 3, INPUT, INPUT]).into())?
            .into_optimized()?
            .into_runnable()?;
        Ok(Detector {
            model,
            score_threshold: 0.8,
            nms_threshold: 0.3,
        })
    }

    /// Detect faces, sorted by area (largest first).
    pub fn detect(&self, img: &Image) -> Result<Vec<Face>> {
        let scale = (INPUT as f32 / img.width as f32).min(INPUT as f32 / img.height as f32);
        let tw = ((img.width as f32 * scale).round() as usize).min(INPUT);
        let th = ((img.height as f32 * scale).round() as usize).min(INPUT);

        // Letterbox into the top-left corner, BGR planar, zero padded.
        let plane = INPUT * INPUT;
        let mut input = vec![0f32; 3 * plane];
        let inv = 1.0 / scale;
        for y in 0..th {
            let sy = (y as f32 + 0.5) * inv - 0.5;
            for x in 0..tw {
                let sx = (x as f32 + 0.5) * inv - 0.5;
                let o = y * INPUT + x;
                // planes: B, G, R
                input[o] = img.sample_rgb(sx, sy, 2);
                input[plane + o] = img.sample_rgb(sx, sy, 1);
                input[2 * plane + o] = img.sample_rgb(sx, sy, 0);
            }
        }
        let tensor: Tensor = tract_ndarray::Array4::from_shape_vec((1, 3, INPUT, INPUT), input)?.into();
        let out = self.model.run(tvec!(tensor.into()))?;
        if out.len() != 12 {
            anyhow::bail!("unexpected detector output count {}", out.len());
        }

        let mut faces = Vec::new();
        for (i, &stride) in STRIDES.iter().enumerate() {
            let cls = &f32s(&out[i])?;
            let obj = &f32s(&out[3 + i])?;
            let bbox = &f32s(&out[6 + i])?;
            let kps = &f32s(&out[9 + i])?;
            let cols = INPUT / stride;
            let rows = INPUT / stride;
            let s = stride as f32;
            for r in 0..rows {
                for c in 0..cols {
                    let idx = r * cols + c;
                    let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                    if score < self.score_threshold {
                        continue;
                    }
                    let b = &bbox[idx * 4..idx * 4 + 4];
                    let cx = (c as f32 + b[0]) * s;
                    let cy = (r as f32 + b[1]) * s;
                    let w = b[2].exp() * s;
                    let h = b[3].exp() * s;
                    let k = &kps[idx * 10..idx * 10 + 10];
                    let mut lm = [[0f32; 2]; 5];
                    for n in 0..5 {
                        lm[n] = [
                            (k[2 * n] + c as f32) * s * inv,
                            (k[2 * n + 1] + r as f32) * s * inv,
                        ];
                    }
                    faces.push(Face {
                        x: (cx - w / 2.0) * inv,
                        y: (cy - h / 2.0) * inv,
                        w: w * inv,
                        h: h * inv,
                        score,
                        landmarks: lm,
                    });
                }
            }
        }
        let mut faces = nms(faces, self.nms_threshold);
        faces.sort_by(|a, b| b.area().total_cmp(&a.area()));
        Ok(faces)
    }
}

fn nms(mut faces: Vec<Face>, thr: f32) -> Vec<Face> {
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| k.iou(&f) <= thr) {
            keep.push(f);
        }
    }
    keep
}

pub(crate) fn f32s(t: &TValue) -> Result<Vec<f32>> {
    Ok(t.try_as_plain_ram()?.as_slice::<f32>()?.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(x: f32, score: f32) -> Face {
        Face {
            x,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            score,
            landmarks: [[0.0; 2]; 5],
        }
    }

    #[test]
    fn nms_suppresses_overlaps() {
        let out = nms(vec![face(0.0, 0.9), face(1.0, 0.95), face(50.0, 0.8)], 0.3);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].score, 0.95);
    }
}
