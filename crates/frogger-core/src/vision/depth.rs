//! Experimental depth-based liveness (depth cameras such as Intel RealSense).
//!
//! A real face is three dimensional: the nose tip is 2–4 cm closer to the
//! camera than cheeks, forehead and chin. We compare the nose with points on
//! *both* axes:
//!
//! * horizontal: left and right cheek,
//! * vertical: forehead and chin.
//!
//! A flat photo yields ~0 on both axes, however it is tilted: the sample
//! points are symmetric around the nose, so a plane cancels out. Paper is a
//! developable surface – it can only be bent around one axis – so a curved
//! print can fake relief on one axis but not on both at the same time.
//!
//! The depth map must be registered to the IR image the landmarks come from
//! (true for RealSense: depth is computed from its left IR imager). Only
//! proportional scaling between the two resolutions is applied.

use super::detect::Face;
use serde::Serialize;

/// Depth image in millimetres; 0 = no measurement.
#[derive(Clone, Debug)]
pub struct DepthImage {
    pub width: usize,
    pub height: usize,
    pub mm: Vec<u16>,
}

/// Plausible face distance range for laptop/desktop use.
const MIN_MM: u16 = 150;
const MAX_MM: u16 = 1500;
/// Minimum share of valid pixels in each sampling window.
const MIN_COVERAGE: f32 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum DepthVerdict {
    /// Relief on both axes: a three-dimensional face.
    Live { horizontal_mm: f32, vertical_mm: f32 },
    /// At least one axis without relief: flat or singly curved surface.
    Flat { horizontal_mm: f32, vertical_mm: f32 },
    /// Not enough valid depth data (out of range, projector covered,
    /// reflective glasses …). Proves nothing either way.
    Invalid,
}

impl DepthImage {
    /// Median of valid depth values in a square window (image coordinates of
    /// the depth map).
    fn window_median(&self, cx: f32, cy: f32, r: f32) -> Option<f32> {
        let x0 = (cx - r).floor().max(0.0) as usize;
        let y0 = (cy - r).floor().max(0.0) as usize;
        let x1 = ((cx + r).ceil() as usize).min(self.width.saturating_sub(1));
        let y1 = ((cy + r).ceil() as usize).min(self.height.saturating_sub(1));
        if x1 < x0 || y1 < y0 {
            return None;
        }
        let mut v: Vec<u16> = Vec::new();
        let mut total = 0usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                total += 1;
                let d = self.mm[y * self.width + x];
                if (MIN_MM..=MAX_MM).contains(&d) {
                    v.push(d);
                }
            }
        }
        if total == 0 || (v.len() as f32) < MIN_COVERAGE * total as f32 {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2] as f32)
    }
}

/// Measure the relief of `face` (detected in an image of `img_w`x`img_h`).
pub fn measure(
    depth: &DepthImage,
    face: &Face,
    img_w: usize,
    img_h: usize,
    min_relief_mm: f32,
) -> DepthVerdict {
    if depth.width == 0 || depth.height == 0 || img_w == 0 || img_h == 0 {
        return DepthVerdict::Invalid;
    }
    let sx = depth.width as f32 / img_w as f32;
    let sy = depth.height as f32 / img_h as f32;
    let p = |q: [f32; 2]| [q[0] * sx, q[1] * sy];
    let lm = face.landmarks;
    let (eye_l, eye_r, nose, mouth_l, mouth_r) = (p(lm[0]), p(lm[1]), p(lm[2]), p(lm[3]), p(lm[4]));
    let d = ((eye_l[0] - eye_r[0]).powi(2) + (eye_l[1] - eye_r[1]).powi(2)).sqrt();
    if d < 8.0 {
        return DepthVerdict::Invalid; // face too small in the depth map
    }
    // Unit vectors along the face's own axes (head roll tolerant).
    let ux = [(eye_r[0] - eye_l[0]) / d, (eye_r[1] - eye_l[1]) / d];
    let uy = [-ux[1], ux[0]];
    let at = |o: [f32; 2], a: f32, b: f32| [o[0] + a * ux[0] + b * uy[0], o[1] + a * ux[1] + b * uy[1]];
    let eyes_mid = [(eye_l[0] + eye_r[0]) / 2.0, (eye_l[1] + eye_r[1]) / 2.0];
    let mouth_mid = [(mouth_l[0] + mouth_r[0]) / 2.0, (mouth_l[1] + mouth_r[1]) / 2.0];

    let r_small = (0.10 * d).max(1.5);
    let r_large = (0.15 * d).max(2.0);
    let sample = |q: [f32; 2], r: f32| depth.window_median(q[0], q[1], r);

    let Some(z_nose) = sample(nose, r_small) else {
        return DepthVerdict::Invalid;
    };
    let (Some(z_left), Some(z_right)) = (
        sample(at(nose, -0.85 * d, 0.0), r_large),
        sample(at(nose, 0.85 * d, 0.0), r_large),
    ) else {
        return DepthVerdict::Invalid;
    };
    let (Some(z_top), Some(z_bottom)) = (
        sample(at(eyes_mid, 0.0, -0.55 * d), r_large),
        sample(at(mouth_mid, 0.0, 0.45 * d), r_large),
    ) else {
        return DepthVerdict::Invalid;
    };
    let horizontal = (z_left + z_right) / 2.0 - z_nose;
    let vertical = (z_top + z_bottom) / 2.0 - z_nose;
    if horizontal >= min_relief_mm && vertical >= min_relief_mm {
        DepthVerdict::Live {
            horizontal_mm: horizontal,
            vertical_mm: vertical,
        }
    } else {
        DepthVerdict::Flat {
            horizontal_mm: horizontal,
            vertical_mm: vertical,
        }
    }
}

/// Decode a V4L2 `Z16` buffer (16 bit little endian, 1 mm per unit on Intel
/// RealSense D4xx/SR3xx defaults).
pub fn decode_z16(buf: &[u8], w: usize, h: usize) -> Option<DepthImage> {
    if buf.len() < 2 * w * h {
        return None;
    }
    let mm = buf[..2 * w * h]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect();
    Some(DepthImage {
        width: w,
        height: h,
        mm,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const W: usize = 320;
    pub const H: usize = 240;

    /// Landmarks of a frontal face centred in a 320x240 image (interocular 60 px).
    pub fn face() -> Face {
        Face {
            x: 110.0,
            y: 50.0,
            w: 100.0,
            h: 130.0,
            score: 0.9,
            landmarks: [
                [130.0, 100.0],
                [190.0, 100.0],
                [160.0, 130.0],
                [137.0, 155.0],
                [183.0, 155.0],
            ],
        }
    }

    /// Synthetic depth map: `f(x, y)` in mm, with optional holes.
    pub fn map(f: impl Fn(f32, f32) -> f32) -> DepthImage {
        let mut mm = Vec::with_capacity(W * H);
        for y in 0..H {
            for x in 0..W {
                mm.push(f(x as f32, y as f32).round().clamp(0.0, 65535.0) as u16);
            }
        }
        DepthImage {
            width: W,
            height: H,
            mm,
        }
    }

    /// Ellipsoid head with a nose bump, 500 mm away, plus sensor noise.
    pub fn real_face(x: f32, y: f32) -> f32 {
        let (cx, cy) = (160.0, 120.0);
        let (ax, ay) = ((x - cx) / 75.0, (y - cy) / 95.0);
        let head = 500.0 + 60.0 * (ax * ax + ay * ay).min(1.5);
        let nose = (-((x - 160.0).powi(2) + (y - 128.0).powi(2)) / (2.0 * 9.0f32.powi(2))).exp() * 18.0;
        let noise = ((x * 12.9898 + y * 78.233).sin() * 43758.545).fract() * 3.0 - 1.5;
        head - nose + noise
    }

    #[test]
    fn real_face_is_live() {
        let v = measure(&map(real_face), &face(), W, H, 8.0);
        assert!(matches!(v, DepthVerdict::Live { .. }), "{v:?}");
    }

    #[test]
    fn flat_photo_is_flat_even_when_tilted() {
        for (ax, ay) in [(0.0, 0.0), (0.8, 0.0), (0.0, 0.8), (0.6, -0.5)] {
            let v = measure(
                &map(|x, y| 500.0 + ax * (x - 160.0) + ay * (y - 120.0)),
                &face(),
                W,
                H,
                8.0,
            );
            assert!(matches!(v, DepthVerdict::Flat { .. }), "tilt {ax}/{ay}: {v:?}");
        }
    }

    #[test]
    fn photo_bent_around_one_axis_is_flat() {
        // Convex cylinder around the vertical axis: relief only horizontally.
        let v = measure(
            &map(|x, _| 500.0 + ((x - 160.0) / 3.0).powi(2) * 0.4),
            &face(),
            W,
            H,
            8.0,
        );
        match v {
            DepthVerdict::Flat {
                horizontal_mm,
                vertical_mm,
            } => {
                assert!(horizontal_mm > 8.0 && vertical_mm.abs() < 3.0, "{v:?}")
            }
            other => panic!("expected flat, got {other:?}"),
        }
        // …and around the horizontal axis.
        let v = measure(
            &map(|_, y| 500.0 + ((y - 120.0) / 3.0).powi(2) * 0.4),
            &face(),
            W,
            H,
            8.0,
        );
        assert!(matches!(v, DepthVerdict::Flat { .. }), "{v:?}");
    }

    #[test]
    fn missing_data_is_invalid_not_flat() {
        // Projector covered / out of range: no valid pixels.
        assert_eq!(
            measure(&map(|_, _| 0.0), &face(), W, H, 8.0),
            DepthVerdict::Invalid
        );
        assert_eq!(
            measure(&map(|_, _| 4000.0), &face(), W, H, 8.0),
            DepthVerdict::Invalid
        );
        // Hole exactly at the nose (e.g. glare).
        let holes = map(|x, y| {
            if (x - 160.0).abs() < 12.0 && (y - 130.0).abs() < 12.0 {
                0.0
            } else {
                real_face(x, y)
            }
        });
        assert_eq!(measure(&holes, &face(), W, H, 8.0), DepthVerdict::Invalid);
    }

    #[test]
    fn scaled_depth_resolution_and_head_roll() {
        // IR image at 640x480, depth map at 320x240 (half resolution):
        // landmarks are in IR coordinates and must be scaled down.
        let mut big = face();
        for p in big.landmarks.iter_mut() {
            *p = [p[0] * 2.0, p[1] * 2.0];
        }
        assert!(matches!(
            measure(&map(real_face), &big, W * 2, H * 2, 8.0),
            DepthVerdict::Live { .. }
        ));
        // Rolled head (15°): rotate landmarks around the nose.
        let (s, c) = (15f32.to_radians().sin(), 15f32.to_radians().cos());
        let mut f = face();
        for p in f.landmarks.iter_mut() {
            let (dx, dy) = (p[0] - 160.0, p[1] - 130.0);
            *p = [160.0 + c * dx - s * dy, 130.0 + s * dx + c * dy];
        }
        assert!(matches!(
            measure(&map(real_face), &f, W, H, 8.0),
            DepthVerdict::Live { .. }
        ));
    }

    #[test]
    fn z16_decoding() {
        let buf: Vec<u8> = (0..4).flat_map(|i| (500u16 + i).to_le_bytes()).collect();
        let d = decode_z16(&buf, 2, 2).unwrap();
        assert_eq!(d.mm, vec![500, 501, 502, 503]);
        assert!(decode_z16(&buf, 4, 4).is_none());
    }
}
