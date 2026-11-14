//! Vision pipeline: detection → alignment → embedding / eye patches.

pub mod align;
pub mod depth;
pub mod detect;
pub mod embed;
pub mod eyes;
pub mod image;

pub use self::image::Image;
pub use detect::{Detector, Face};
pub use embed::{cosine, Embedder, Embedding};

use anyhow::Result;
use std::path::Path;

/// Bundles both models; loaded once by the daemon.
pub struct Pipeline {
    pub detector: Detector,
    pub embedder: Embedder,
}

/// Everything the auth/enroll logic needs from one frame.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub face: Face,
    /// Number of faces visible in the frame.
    pub face_count: usize,
    pub embedding: Embedding,
    pub eyes: eyes::EyePair,
    /// Mean intensity of the face region (0..255).
    pub face_brightness: f32,
    /// Image → aligned-template transform (reused for eye-only frames).
    pub transform: align::Similarity,
}

impl Pipeline {
    pub fn load(detector: &Path, recognizer: &Path) -> Result<Self> {
        Ok(Pipeline {
            detector: Detector::load(detector)?,
            embedder: Embedder::load(recognizer)?,
        })
    }

    /// Analyse the largest face in `img`, if any.
    pub fn analyse(&self, img: &Image) -> Result<Option<Analysis>> {
        let faces = self.detector.detect(img)?;
        let Some(face) = faces.first().cloned() else {
            return Ok(None);
        };
        let (aligned, transform) = align::align_face(img, &face.landmarks);
        let embedding = self.embedder.embed(&aligned)?;
        let eyes = eyes::EyePair::extract(img, &transform);
        let face_brightness = img.mean_in_rect(face.x, face.y, face.w, face.h);
        Ok(Some(Analysis {
            face,
            face_count: faces.len(),
            embedding,
            eyes,
            face_brightness,
            transform,
        }))
    }
}

/// Cheap per-frame measurement between full analyses: eye patches and face
/// brightness at the last known face position.
#[derive(Clone, Debug)]
pub struct EyeSample {
    pub eyes: eyes::EyePair,
    pub face_brightness: f32,
}

pub fn eye_sample(img: &Image, last: &Analysis) -> EyeSample {
    let f = &last.face;
    EyeSample {
        eyes: eyes::EyePair::extract(img, &last.transform),
        face_brightness: img.mean_in_rect(f.x, f.y, f.w, f.h),
    }
}
