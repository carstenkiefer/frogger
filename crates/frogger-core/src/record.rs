//! Measurement traces (`frogger doctor --record`): per-frame numbers from
//! which liveness checks can be developed and regression-tested without
//! images.
//!
//! A trace contains **no images, no eye patches and no face embeddings**:
//! only brightness values, the face box, five landmark positions,
//! similarity scores against the user's own template and simple statistics
//! of the eye regions (for the corneal glint check). No face can be
//! reconstructed from it, so testers can attach it to an issue (see the
//! test data policy in docs/ROADMAP.md).

use crate::vision::detect::Face;
use crate::vision::Image;
use serde::{Deserialize, Serialize};

/// Format version of [`Trace`]; bump on incompatible changes.
pub const TRACE_FORMAT: u32 = 1;
/// Longest recording the daemon accepts.
pub const MAX_RECORD_SECS: u32 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub format: u32,
    /// FROGGER version that recorded the trace.
    pub frogger: String,
    /// Free-text scenario chosen by the tester, e.g. `real`, `glasses`,
    /// `photo`, `ir-print`, `screen`.
    pub label: String,
    pub camera: CameraMeta,
    /// Liveness mode in effect and whether the template has an eye
    /// calibration (eye scores are only recorded with one).
    pub liveness: String,
    pub eye_calibration: bool,
    pub frames: Vec<FrameRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CameraMeta {
    pub name: String,
    pub usb_id: Option<String>,
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub fourcc: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameRecord {
    /// Seconds since the start of the recording.
    pub t: f32,
    /// Mean brightness of the whole frame (0..255).
    pub mean: f32,
    /// Mean absolute chroma deviation (0 for monochrome frames).
    pub chroma: f32,
    /// `false` for the dark phase of a strobing emitter.
    pub lit: bool,
    /// Detected face (lit frames only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub face: Option<FaceRecord>,
    /// Eye-region statistics, `[image-left eye, image-right eye]`, at the
    /// current face or – in frames without one, e.g. unlit frames – at the
    /// last known face position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glints: Option<[GlintRecord; 2]>,
    /// Verdict of the real verifier after this frame (`Continue`,
    /// `NeedBlink`, `Success`).
    pub verdict: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaceRecord {
    /// Detector confidence.
    pub score: f32,
    /// Number of faces in the frame.
    pub faces: usize,
    /// x, y, width, height in pixels.
    pub bbox: [f32; 4],
    /// Image-left eye, image-right eye, nose tip, mouth corners (pixels).
    pub landmarks: [[f32; 2]; 5],
    /// Mean brightness of the face box.
    pub brightness: f32,
    /// Horizontal nose offset relative to the eye distance (head turn).
    pub yaw: f32,
    /// Best cosine similarity to the user's enrolled samples.
    pub identity: f32,
    /// Per eye: similarity to the user's open and closed eye reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eyes: Option<[[f32; 2]; 2]>,
    /// Per eye: closed probability of the eye state model, if enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eye_closed: Option<[f32; 2]>,
}

/// Brightness statistics of a square window around one eye. A corneal
/// glint of the IR emitter shows up as a small peak (`max` well above
/// `p90`) close to the eye centre that disappears when the emitter is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GlintRecord {
    pub max: f32,
    pub p90: f32,
    pub mean: f32,
    /// Position of the brightest pixel relative to the eye landmark, in
    /// units of the eye distance.
    pub dx: f32,
    pub dy: f32,
}

/// Half side of the eye window, relative to the eye distance.
const GLINT_WINDOW: f32 = 0.2;

/// Statistics of the windows around both eyes of `face` in `img`.
pub fn glints(img: &Image, face: &Face) -> [GlintRecord; 2] {
    let iod = face.interocular().max(1.0);
    [0, 1].map(|i| glint(img, face.landmarks[i], iod))
}

fn glint(img: &Image, eye: [f32; 2], iod: f32) -> GlintRecord {
    let r = (GLINT_WINDOW * iod).max(2.0);
    let (x0, y0) = ((eye[0] - r).max(0.0) as usize, (eye[1] - r).max(0.0) as usize);
    let x1 = ((eye[0] + r).max(0.0) as usize).min(img.width);
    let y1 = ((eye[1] + r).max(0.0) as usize).min(img.height);
    if x1 <= x0 || y1 <= y0 {
        return GlintRecord::default();
    }
    let mut values = Vec::with_capacity((x1 - x0) * (y1 - y0));
    let (mut best, mut at) = (-1.0f32, (x0, y0));
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * img.width + x) * img.channels;
            let v = if img.channels == 1 {
                img.data[i] as f32
            } else {
                (img.data[i] as f32 + img.data[i + 1] as f32 + img.data[i + 2] as f32) / 3.0
            };
            if v > best {
                best = v;
                at = (x, y);
            }
            values.push(v);
        }
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    values.sort_by(f32::total_cmp);
    let p90 = values[(values.len() * 9 / 10).min(values.len() - 1)];
    GlintRecord {
        max: best,
        p90,
        mean: r2(mean),
        dx: r3((at.0 as f32 + 0.5 - eye[0]) / iod),
        dy: r3((at.1 as f32 + 0.5 - eye[1]) / iod),
    }
}

/// Round to 2 / 3 decimals: keeps traces small and readable.
pub fn r2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}
pub fn r3(v: f32) -> f32 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face_at(left: [f32; 2], right: [f32; 2]) -> Face {
        Face {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
            score: 0.9,
            landmarks: [left, right, [50.0, 60.0], [40.0, 80.0], [60.0, 80.0]],
        }
    }

    #[test]
    fn glint_finds_the_highlight_next_to_the_eye() {
        let mut data = vec![60u8; 100 * 100];
        // Highlight 2 px right of and 1 px above the left eye at (30, 40).
        data[39 * 100 + 32] = 250;
        let img = Image::from_gray(100, 100, data);
        let g = glints(&img, &face_at([30.0, 40.0], [70.0, 40.0]));
        assert_eq!(g[0].max, 250.0);
        assert_eq!(g[0].p90, 60.0);
        assert!((g[0].dx - 2.5 / 40.0).abs() < 0.002, "{}", g[0].dx);
        assert!((g[0].dy + 0.5 / 40.0).abs() < 0.002, "{}", g[0].dy);
        // The other eye has no highlight.
        assert_eq!(g[1].max, 60.0);
    }

    #[test]
    fn glint_window_at_the_image_border_is_safe() {
        let img = Image::from_gray(20, 20, vec![10; 400]);
        // Partly outside: only the visible part is measured.
        let g = glints(&img, &face_at([-5.0, -5.0], [25.0, 25.0]));
        assert!(g.iter().all(|w| w.max == 10.0 && w.mean == 10.0), "{g:?}");
        // Completely outside: empty statistics instead of a panic.
        let g = glints(&img, &face_at([-50.0, -50.0], [-30.0, -50.0]));
        assert_eq!(g, [GlintRecord::default(); 2]);
    }

    #[test]
    fn trace_contains_no_embedding_or_image_fields() {
        let json = serde_json::to_string(&FaceRecord {
            score: 0.9,
            faces: 1,
            bbox: [0.0; 4],
            landmarks: [[0.0; 2]; 5],
            brightness: 100.0,
            yaw: 0.0,
            identity: 0.8,
            eyes: Some([[0.9, 0.1]; 2]),
            eye_closed: None,
        })
        .unwrap();
        for forbidden in ["embedding", "image", "patch", "pixels"] {
            assert!(!json.contains(forbidden), "{forbidden} in {json}");
        }
    }
}
