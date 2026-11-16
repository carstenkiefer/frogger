//! Vision pipeline: detection → alignment → embedding / eye patches.

pub mod align;
pub mod depth;
pub mod detect;
pub mod embed;
pub mod eyes;
pub mod eyestate;
pub mod image;

pub use self::image::Image;
pub use detect::{Detector, Face};
pub use embed::{cosine, Embedder, Embedding};

use anyhow::Result;
use std::path::Path;

/// Bundles the models; loaded once by the daemon.
pub struct Pipeline {
    pub detector: Detector,
    pub embedder: Embedder,
    /// Optional open/closed classifier for the eyes.
    pub eye_state: Option<eyestate::EyeStateModel>,
}

/// Everything the auth/enroll logic needs from one frame.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub face: Face,
    /// Number of faces visible in the frame.
    pub face_count: usize,
    pub embedding: Embedding,
    pub eyes: eyes::EyePair,
    /// Probability that each eye is closed (eye state model), if installed.
    pub eye_closed: Option<[f32; 2]>,
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
            eye_state: None,
        })
    }

    /// Add the eye state model; a missing file only disables it.
    pub fn with_eye_state(mut self, path: &Path) -> Self {
        if !path.exists() {
            log::info!("eye state model {} not installed", path.display());
            return self;
        }
        match eyestate::EyeStateModel::load(path) {
            Ok(m) => {
                log::info!("eye state model loaded");
                self.eye_state = Some(m);
            }
            Err(e) => log::warn!("eye state model unusable: {e:#}"),
        }
        self
    }

    fn eye_closed(&self, img: &Image, transform: &align::Similarity) -> Option<[f32; 2]> {
        let m = self.eye_state.as_ref()?;
        m.closed(img, transform)
            .map_err(|e| log::warn!("eye state: {e:#}"))
            .ok()
    }

    /// Cheap per-frame measurement between full analyses (see [`EyeSample`]).
    pub fn eye_sample(&self, img: &Image, last: &Analysis) -> EyeSample {
        let f = &last.face;
        EyeSample {
            eyes: eyes::EyePair::extract(img, &last.transform),
            eye_closed: self.eye_closed(img, &last.transform),
            face_brightness: img.mean_in_rect(f.x, f.y, f.w, f.h),
        }
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
        let eye_closed = self.eye_closed(img, &transform);
        let face_brightness = img.mean_in_rect(face.x, face.y, face.w, face.h);
        Ok(Some(Analysis {
            face,
            face_count: faces.len(),
            embedding,
            eyes,
            eye_closed,
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
    pub eye_closed: Option<[f32; 2]>,
    pub face_brightness: f32,
}
