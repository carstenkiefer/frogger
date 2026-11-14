//! 5-point similarity alignment to the canonical ArcFace/SFace 112x112 layout.

use super::image::Image;

/// Canonical landmark positions in a 112x112 aligned face
/// (image-left eye, image-right eye, nose, mouth corners).
pub const TEMPLATE: [[f32; 2]; 5] = [
    [38.2946, 51.6963],
    [73.5318, 51.5014],
    [56.0252, 71.7366],
    [41.5493, 92.3655],
    [70.7299, 92.2041],
];

/// Similarity transform p' = [a -b; b a] p + t.
#[derive(Clone, Copy, Debug)]
pub struct Similarity {
    pub a: f32,
    pub b: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Similarity {
    /// Least-squares similarity mapping `src` onto `dst` (Umeyama, 2D closed form).
    pub fn estimate(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> Similarity {
        let ms = mean(src);
        let md = mean(dst);
        let (mut dot, mut cross, mut norm) = (0f32, 0f32, 0f32);
        for i in 0..src.len() {
            let sx = src[i][0] - ms[0];
            let sy = src[i][1] - ms[1];
            let dx = dst[i][0] - md[0];
            let dy = dst[i][1] - md[1];
            dot += sx * dx + sy * dy;
            cross += sx * dy - sy * dx;
            norm += sx * sx + sy * sy;
        }
        let norm = norm.max(1e-6);
        let a = dot / norm;
        let b = cross / norm;
        let tx = md[0] - (a * ms[0] - b * ms[1]);
        let ty = md[1] - (b * ms[0] + a * ms[1]);
        Similarity { a, b, tx, ty }
    }

    pub fn apply(&self, p: [f32; 2]) -> [f32; 2] {
        [
            self.a * p[0] - self.b * p[1] + self.tx,
            self.b * p[0] + self.a * p[1] + self.ty,
        ]
    }

    pub fn inverse(&self) -> Similarity {
        let d = (self.a * self.a + self.b * self.b).max(1e-12);
        let ia = self.a / d;
        let ib = -self.b / d;
        let tx = -(ia * self.tx - ib * self.ty);
        let ty = -(ib * self.tx + ia * self.ty);
        Similarity { a: ia, b: ib, tx, ty }
    }

    pub fn scale(&self) -> f32 {
        (self.a * self.a + self.b * self.b).sqrt()
    }
}

fn mean(p: &[[f32; 2]; 5]) -> [f32; 2] {
    let mut m = [0f32; 2];
    for q in p {
        m[0] += q[0];
        m[1] += q[1];
    }
    [m[0] / p.len() as f32, m[1] / p.len() as f32]
}

/// Warp `img` into a `size`x`size` RGB crop using the transform that maps
/// source → template coordinates (`fwd`). `template_scale` lets callers sample
/// the template space at higher density (e.g. 2.0 → 224 px for 112 template).
pub fn warp(img: &Image, fwd: &Similarity, out_w: usize, out_h: usize, origin: [f32; 2], step: f32) -> Image {
    let inv = fwd.inverse();
    let mut out = Image::new(out_w, out_h, img.channels);
    for v in 0..out_h {
        for u in 0..out_w {
            let tpl = [origin[0] + u as f32 * step, origin[1] + v as f32 * step];
            let s = inv.apply(tpl);
            for c in 0..img.channels {
                out.data[(v * out_w + u) * img.channels + c] = img.sample(s[0], s[1], c).round() as u8;
            }
        }
    }
    out
}

/// Aligned 112x112 face crop (same channel count as input).
pub fn align_face(img: &Image, landmarks: &[[f32; 2]; 5]) -> (Image, Similarity) {
    let t = Similarity::estimate(landmarks, &TEMPLATE);
    (warp(img, &t, 112, 112, [0.0, 0.0], 1.0), t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_estimate() {
        let t = Similarity::estimate(&TEMPLATE, &TEMPLATE);
        assert!((t.a - 1.0).abs() < 1e-4 && t.b.abs() < 1e-4);
        assert!(t.tx.abs() < 1e-3 && t.ty.abs() < 1e-3);
    }

    #[test]
    fn recovers_scaled_rotated_transform() {
        let truth = Similarity {
            a: 1.6,
            b: 0.3,
            tx: 12.0,
            ty: -7.0,
        };
        let mut src = [[0f32; 2]; 5];
        for i in 0..5 {
            src[i] = truth.inverse().apply(TEMPLATE[i]);
        }
        let est = Similarity::estimate(&src, &TEMPLATE);
        for i in 0..5 {
            let p = est.apply(src[i]);
            assert!((p[0] - TEMPLATE[i][0]).abs() < 1e-2);
            assert!((p[1] - TEMPLATE[i][1]).abs() < 1e-2);
        }
        let back = est.inverse().inverse();
        assert!((back.a - est.a).abs() < 1e-5);
    }
}
