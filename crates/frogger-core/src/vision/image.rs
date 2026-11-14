//! Minimal image containers and the few pixel operations the pipeline needs.
//! Kept dependency free on purpose: camera frames arrive as raw buffers and
//! all heavy lifting happens inside the ONNX models.

/// 8-bit image with 1 (gray) or 3 (RGB) interleaved channels.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub data: Vec<u8>,
}

impl Image {
    pub fn new(width: usize, height: usize, channels: usize) -> Self {
        Image {
            width,
            height,
            channels,
            data: vec![0; width * height * channels],
        }
    }

    pub fn from_gray(width: usize, height: usize, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width * height);
        Image {
            width,
            height,
            channels: 1,
            data,
        }
    }

    pub fn from_rgb(width: usize, height: usize, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), width * height * 3);
        Image {
            width,
            height,
            channels: 3,
            data,
        }
    }

    pub fn is_gray(&self) -> bool {
        self.channels == 1
    }

    /// Luma view of the image (copy for RGB, clone for gray).
    pub fn to_gray(&self) -> Image {
        if self.channels == 1 {
            return self.clone();
        }
        let mut out = Vec::with_capacity(self.width * self.height);
        for px in self.data.as_chunks::<3>().0 {
            let y = (77 * px[0] as u32 + 150 * px[1] as u32 + 29 * px[2] as u32) >> 8;
            out.push(y as u8);
        }
        Image::from_gray(self.width, self.height, out)
    }

    /// Bilinear sample of channel `c` at sub-pixel position; outside → 0.
    #[inline]
    pub fn sample(&self, x: f32, y: f32, c: usize) -> f32 {
        if !(x >= 0.0 && y >= 0.0) {
            return 0.0;
        }
        let x0 = x.floor() as usize;
        let y0 = y.floor() as usize;
        if x0 + 1 >= self.width || y0 + 1 >= self.height {
            if x0 < self.width && y0 < self.height {
                return self.data[(y0 * self.width + x0) * self.channels + c] as f32;
            }
            return 0.0;
        }
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let ch = self.channels;
        let i00 = (y0 * self.width + x0) * ch + c;
        let i01 = i00 + ch;
        let i10 = i00 + self.width * ch;
        let i11 = i10 + ch;
        let d = &self.data;
        let top = d[i00] as f32 * (1.0 - fx) + d[i01] as f32 * fx;
        let bot = d[i10] as f32 * (1.0 - fx) + d[i11] as f32 * fx;
        top * (1.0 - fy) + bot * fy
    }

    /// Value of channel `c` (gray images return the single channel for every c).
    #[inline]
    pub fn sample_rgb(&self, x: f32, y: f32, c: usize) -> f32 {
        if self.channels == 1 {
            self.sample(x, y, 0)
        } else {
            self.sample(x, y, c)
        }
    }

    /// Mean intensity inside a rectangle (clamped to the image).
    pub fn mean_in_rect(&self, x: f32, y: f32, w: f32, h: f32) -> f32 {
        let g = if self.channels == 1 {
            None
        } else {
            Some(self.to_gray())
        };
        let img = g.as_ref().unwrap_or(self);
        let x0 = x.max(0.0) as usize;
        let y0 = y.max(0.0) as usize;
        let x1 = ((x + w).max(0.0) as usize).min(img.width);
        let y1 = ((y + h).max(0.0) as usize).min(img.height);
        if x1 <= x0 || y1 <= y0 {
            return 0.0;
        }
        let mut sum = 0u64;
        for yy in y0..y1 {
            let row = &img.data[yy * img.width + x0..yy * img.width + x1];
            sum += row.iter().map(|&v| v as u64).sum::<u64>();
        }
        sum as f32 / ((x1 - x0) * (y1 - y0)) as f32
    }

    pub fn mean(&self) -> f32 {
        if self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().map(|&v| v as u64).sum::<u64>() as f32 / self.data.len() as f32
    }
}

/// Load an image file (JPEG/PNG) – used by tests and `frogger test --image`.
pub fn load_file(path: &std::path::Path) -> anyhow::Result<Image> {
    let img = ::image::open(path)?.to_rgb8();
    let (w, h) = img.dimensions();
    Ok(Image::from_rgb(w as usize, h as usize, img.into_raw()))
}
