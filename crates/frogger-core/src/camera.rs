//! V4L2 camera access, IR-camera detection and IR emitter control.

use crate::config::{CameraConfig, EmitterControl};
use crate::vision::Image;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC};

/// Pixel formats we can decode, best first for IR use.
const PREFERRED: [&[u8; 4]; 5] = [b"GREY", b"Y16 ", b"YUYV", b"MJPG", b"RGB3"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraKind {
    /// Monochrome near-infrared sensor (Windows-Hello-class camera).
    Infrared,
    /// Ordinary colour webcam.
    Rgb,
    /// Not determined (e.g. no frames could be captured).
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct CameraInfo {
    pub path: PathBuf,
    /// Stable alias under /dev/v4l/by-id, if any.
    pub stable_path: Option<PathBuf>,
    pub name: String,
    pub driver: String,
    pub bus: String,
    pub usb_id: Option<String>,
    pub formats: Vec<String>,
    pub kind: CameraKind,
    /// Human readable reasons for the classification.
    pub evidence: Vec<String>,
}

/// List V4L2 video capture nodes (skips metadata/output nodes).
pub fn list_cameras() -> Vec<CameraInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/video4linux") else {
        return out;
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("video"))
        .collect();
    names.sort_by_key(|n| n[5..].parse::<u32>().unwrap_or(u32::MAX));
    for n in names {
        let path = PathBuf::from("/dev").join(&n);
        if let Ok(info) = probe(&path) {
            out.push(info);
        }
    }
    out
}

/// Inspect one device without streaming.
pub fn probe(path: &Path) -> Result<CameraInfo> {
    let dev = Device::with_path(path).with_context(|| format!("opening {}", path.display()))?;
    let caps = dev.query_caps()?;
    if !caps.capabilities.contains(v4l::capability::Flags::VIDEO_CAPTURE) {
        bail!("{} is not a video capture device", path.display());
    }
    let formats: Vec<String> = dev
        .enum_formats()
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            d.fourcc
                .str()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| format!("{:?}", d.fourcc))
        })
        .collect();
    if formats.is_empty() {
        bail!("{} offers no capture formats", path.display());
    }
    let mut evidence = Vec::new();
    let mono_format = formats
        .iter()
        .any(|f| matches!(f.trim(), "GREY" | "Y8" | "Y10" | "Y12" | "Y16" | "Y8I"));
    let colour_format = formats
        .iter()
        .any(|f| matches!(f.trim(), "YUYV" | "MJPG" | "NV12" | "RGB3" | "H264"));
    let name_ir = {
        let l = caps.card.to_lowercase();
        l.contains(" ir")
            || l.contains("infrared")
            || l.ends_with("ir")
            || l.contains("_ir")
            || l.contains("ir camera")
    };
    if mono_format {
        evidence.push("offers a monochrome pixel format".into());
    }
    if name_ir {
        evidence.push(format!("device name '{}' indicates IR", caps.card));
    }
    let kind = if mono_format && !colour_format {
        CameraKind::Infrared
    } else if mono_format || name_ir {
        // Needs a frame-based check (see `classify_frames`).
        CameraKind::Unknown
    } else {
        evidence.push("colour-only pixel formats".into());
        CameraKind::Rgb
    };
    Ok(CameraInfo {
        stable_path: stable_alias(path),
        usb_id: usb_id(path),
        path: path.to_path_buf(),
        name: caps.card,
        driver: caps.driver,
        bus: caps.bus,
        formats,
        kind,
        evidence,
    })
}

/// `/dev/v4l/by-id/*` alias pointing at `path`.
pub fn stable_alias(path: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(path).ok()?;
    for e in std::fs::read_dir("/dev/v4l/by-id").ok()?.flatten() {
        if std::fs::canonicalize(e.path()).ok().as_deref() == Some(target.as_path()) {
            return Some(e.path());
        }
    }
    None
}

/// USB "vvvv:pppp" of the device behind a /dev/videoN node, from sysfs.
pub fn usb_id(path: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    let node = real.file_name()?.to_str()?.to_owned();
    let mut dir = std::fs::canonicalize(format!("/sys/class/video4linux/{node}/device")).ok()?;
    for _ in 0..4 {
        let v = std::fs::read_to_string(dir.join("idVendor"));
        let p = std::fs::read_to_string(dir.join("idProduct"));
        if let (Ok(v), Ok(p)) = (v, p) {
            return Some(format!("{}:{}", v.trim(), p.trim()));
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Laptop lid state from ACPI; `None` when there is no lid.
pub fn lid_closed() -> Option<bool> {
    for e in std::fs::read_dir("/proc/acpi/button/lid").ok()?.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("state")) {
            return Some(s.contains("closed"));
        }
    }
    None
}

/// An open, streaming camera.
pub struct Camera {
    // `stream` borrows from `dev`; it is declared first so it drops first.
    stream: Stream<'static>,
    _dev: Box<Device>,
    pub width: u32,
    pub height: u32,
    pub fourcc: [u8; 4],
    pub info: CameraInfo,
}

/// One decoded frame plus colour statistics used for IR verification.
pub struct Frame {
    pub image: Image,
    /// Mean absolute chroma deviation (0 for true monochrome sources).
    pub chroma: f32,
    pub mean: f32,
}

impl Camera {
    /// Resolve `auto` and open the configured device.
    pub fn open(cfg: &CameraConfig) -> Result<Camera> {
        let path = resolve_device(cfg)?;
        let info = probe(&path)?;
        if let Some(expected) = &cfg.expected_usb_id {
            if info.usb_id.as_deref() != Some(expected.as_str()) {
                bail!(
                    "camera identity mismatch: expected USB {expected}, found {:?} (camera pinning)",
                    info.usb_id
                );
            }
        }
        let dev = Box::new(Device::with_path(&path)?);
        if let Some(em) = &cfg.emitter {
            if let Err(e) = set_xu_control(dev.handle().fd(), em) {
                log::warn!("could not enable IR emitter: {e}");
            }
        }
        let available: Vec<[u8; 4]> = dev.enum_formats()?.into_iter().map(|d| d.fourcc.repr).collect();
        let fourcc = PREFERRED
            .iter()
            .map(|f| **f)
            .find(|f| available.contains(f))
            .context("camera offers no supported pixel format")?;
        let mut fmt = dev.format()?;
        fmt.width = cfg.width;
        fmt.height = cfg.height;
        fmt.fourcc = FourCC::new(&fourcc);
        let fmt = dev.set_format(&fmt)?;
        // SAFETY: the stream only borrows the boxed device, which lives at a
        // stable address inside `Camera` and is dropped after the stream.
        let dev_ref: &'static Device = unsafe { &*(dev.as_ref() as *const Device) };
        let mut stream = Stream::with_buffers(dev_ref, Type::VideoCapture, 4)?;
        stream.set_timeout(Duration::from_millis(1500));
        let mut cam = Camera {
            stream,
            _dev: dev,
            width: fmt.width,
            height: fmt.height,
            fourcc: fmt.fourcc.repr,
            info,
        };
        for _ in 0..cfg.warmup_frames {
            let _ = cam.stream.next();
        }
        Ok(cam)
    }

    /// Capture and decode one frame.
    pub fn frame(&mut self) -> Result<Frame> {
        let (buf, meta) = self.stream.next().context("capturing frame")?;
        let used = (meta.bytesused as usize).min(buf.len());
        decode(
            &buf[..if used == 0 { buf.len() } else { used }],
            self.width as usize,
            self.height as usize,
            &self.fourcc,
        )
    }
}

fn resolve_device(cfg: &CameraConfig) -> Result<PathBuf> {
    if cfg.device != "auto" {
        return Ok(PathBuf::from(&cfg.device));
    }
    let cams = list_cameras();
    cams.iter()
        .find(|c| c.kind == CameraKind::Infrared)
        .or_else(|| cams.iter().find(|c| c.kind == CameraKind::Unknown))
        .or_else(|| cams.first())
        .map(|c| c.stable_path.clone().unwrap_or_else(|| c.path.clone()))
        .context("no camera found")
}

/// Decode a raw V4L2 buffer.
pub fn decode(buf: &[u8], w: usize, h: usize, fourcc: &[u8; 4]) -> Result<Frame> {
    let n = w * h;
    let (image, chroma) = match fourcc {
        b"GREY" => {
            if buf.len() < n {
                bail!("short GREY frame");
            }
            (Image::from_gray(w, h, buf[..n].to_vec()), 0.0)
        }
        b"Y16 " => {
            if buf.len() < 2 * n {
                bail!("short Y16 frame");
            }
            // Little endian, MSB aligned → take the high byte.
            let data = buf[..2 * n].chunks_exact(2).map(|p| p[1]).collect();
            (Image::from_gray(w, h, data), 0.0)
        }
        b"YUYV" => {
            if buf.len() < 2 * n {
                bail!("short YUYV frame");
            }
            let mut y = Vec::with_capacity(n);
            let mut chroma_sum = 0u64;
            for px in buf[..2 * n].chunks_exact(4) {
                y.push(px[0]);
                y.push(px[2]);
                chroma_sum +=
                    (px[1] as i32 - 128).unsigned_abs() as u64 + (px[3] as i32 - 128).unsigned_abs() as u64;
            }
            let chroma = chroma_sum as f32 / n as f32;
            if chroma < 2.0 {
                (Image::from_gray(w, h, y), chroma)
            } else {
                (yuyv_to_rgb(&buf[..2 * n], w, h), chroma)
            }
        }
        b"MJPG" => {
            let img = image::load_from_memory_with_format(buf, image::ImageFormat::Jpeg)?.to_rgb8();
            let (iw, ih) = img.dimensions();
            let rgb = Image::from_rgb(iw as usize, ih as usize, img.into_raw());
            let chroma = rgb_chroma(&rgb);
            if chroma < 2.0 {
                (rgb.to_gray(), chroma)
            } else {
                (rgb, chroma)
            }
        }
        b"RGB3" => {
            if buf.len() < 3 * n {
                bail!("short RGB3 frame");
            }
            let rgb = Image::from_rgb(w, h, buf[..3 * n].to_vec());
            let chroma = rgb_chroma(&rgb);
            (rgb, chroma)
        }
        other => bail!("unsupported pixel format {:?}", String::from_utf8_lossy(other)),
    };
    let mean = image.mean();
    Ok(Frame { image, chroma, mean })
}

fn rgb_chroma(img: &Image) -> f32 {
    let mut s = 0u64;
    let mut cnt = 0u64;
    for px in img.data.chunks_exact(3).step_by(7) {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        s += ((r - g).abs() + (g - b).abs()) as u64;
        cnt += 1;
    }
    if cnt == 0 {
        0.0
    } else {
        s as f32 / cnt as f32
    }
}

fn yuyv_to_rgb(buf: &[u8], w: usize, h: usize) -> Image {
    let mut out = Vec::with_capacity(w * h * 3);
    for px in buf.chunks_exact(4) {
        let (u, v) = (px[1] as f32 - 128.0, px[3] as f32 - 128.0);
        for y in [px[0] as f32, px[2] as f32] {
            out.push((y + 1.402 * v).clamp(0.0, 255.0) as u8);
            out.push((y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8);
            out.push((y + 1.772 * u).clamp(0.0, 255.0) as u8);
        }
    }
    Image::from_rgb(w, h, out)
}

/// Statistics over a short capture, used by `frogger doctor`/`setup` to
/// classify a camera and to detect an emitter that strobes.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureStats {
    pub frames: usize,
    pub mean_chroma: f32,
    pub mean_brightness: f32,
    /// Mean absolute brightness difference between consecutive frames
    /// relative to the mean; high values indicate an alternating emitter.
    pub alternation: f32,
}

pub fn capture_stats(cam: &mut Camera, frames: usize) -> Result<CaptureStats> {
    let mut means = Vec::new();
    let mut chroma = 0.0;
    for _ in 0..frames {
        let f = cam.frame()?;
        chroma += f.chroma;
        means.push(f.mean);
    }
    let n = means.len().max(1) as f32;
    let mean = means.iter().sum::<f32>() / n;
    let alt = means.windows(2).map(|w| (w[0] - w[1]).abs()).sum::<f32>() / (n - 1.0).max(1.0);
    Ok(CaptureStats {
        frames: means.len(),
        mean_chroma: chroma / n,
        mean_brightness: mean,
        alternation: if mean > 1.0 { alt / mean } else { 0.0 },
    })
}

/// Final classification from static probe + captured frames.
pub fn classify(info: &CameraInfo, stats: Option<&CaptureStats>) -> (CameraKind, Vec<String>) {
    let mut ev = info.evidence.clone();
    let mut kind = info.kind;
    if let Some(s) = stats {
        if s.mean_chroma < 2.0 && kind != CameraKind::Rgb {
            ev.push(format!(
                "captured frames are monochrome (chroma {:.2})",
                s.mean_chroma
            ));
            kind = CameraKind::Infrared;
        } else if s.mean_chroma >= 2.0 {
            ev.push(format!(
                "captured frames contain colour (chroma {:.2})",
                s.mean_chroma
            ));
            kind = CameraKind::Rgb;
        }
        if s.alternation > 0.25 {
            ev.push("brightness alternates between frames (strobing IR emitter)".into());
        }
    }
    (kind, ev)
}

// ---------------------------------------------------------------------------
// UVC extension unit (IR emitter) control

#[repr(C)]
struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

/// `_IOWR('u', 0x21, struct uvc_xu_control_query)`
const UVCIOC_CTRL_QUERY: libc::c_ulong = 0xC000_0000
    | ((std::mem::size_of::<UvcXuControlQuery>() as libc::c_ulong) << 16)
    | ((b'u' as libc::c_ulong) << 8)
    | 0x21;
const UVC_SET_CUR: u8 = 0x01;

/// Send SET_CUR to a UVC extension unit (how IR emitters are switched on;
/// values come from linux-enable-ir-emitter or vendor documentation).
pub fn set_xu_control(fd: libc::c_int, c: &EmitterControl) -> Result<()> {
    let mut data = c.control.clone();
    let mut q = UvcXuControlQuery {
        unit: c.unit,
        selector: c.selector,
        query: UVC_SET_CUR,
        size: data.len() as u16,
        data: data.as_mut_ptr(),
    };
    // SAFETY: q points to valid memory for the duration of the call.
    let r = unsafe { libc::ioctl(fd, UVCIOC_CTRL_QUERY as _, &mut q as *mut UvcXuControlQuery) };
    if r < 0 {
        return Err(std::io::Error::last_os_error()).context("UVCIOC_CTRL_QUERY");
    }
    Ok(())
}

/// Extract emitter controls from a linux-enable-ir-emitter TOML config: any
/// table carrying `unit`, `selector` and `control` keys is accepted, so both
/// v6 and v7 layouts work.
pub fn import_emitter_config(src: &str) -> Result<Vec<EmitterControl>> {
    let value: toml::Value = toml::from_str(src)?;
    let mut out = Vec::new();
    fn walk(v: &toml::Value, out: &mut Vec<EmitterControl>) {
        match v {
            toml::Value::Table(t) => {
                let get = |k: &str| t.get(k).and_then(|x| x.as_integer());
                if let (Some(u), Some(s), Some(toml::Value::Array(c))) =
                    (get("unit"), get("selector"), t.get("control"))
                {
                    let control: Option<Vec<u8>> = c
                        .iter()
                        .map(|x| x.as_integer().and_then(|i| u8::try_from(i).ok()))
                        .collect();
                    if let (Ok(unit), Ok(selector), Some(control)) =
                        (u8::try_from(u), u8::try_from(s), control)
                    {
                        out.push(EmitterControl {
                            unit,
                            selector,
                            control,
                        });
                        return;
                    }
                }
                t.values().for_each(|x| walk(x, out));
            }
            toml::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(&value, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_number_matches_kernel() {
        // 16-byte struct on LP64 → 0xC0107521, as in <linux/uvcvideo.h>.
        assert_eq!(std::mem::size_of::<UvcXuControlQuery>(), 16);
        assert_eq!(UVCIOC_CTRL_QUERY, 0xC010_7521);
    }

    #[test]
    fn decode_grey_and_yuyv() {
        let f = decode(&[10u8; 16], 4, 4, b"GREY").unwrap();
        assert!(f.image.is_gray());
        assert_eq!(f.mean, 10.0);
        // Neutral chroma YUYV → treated as monochrome.
        let buf: Vec<u8> = (0..16).flat_map(|_| [50u8, 128]).collect();
        let f = decode(&buf, 4, 4, b"YUYV").unwrap();
        assert!(f.image.is_gray());
        // Coloured YUYV → RGB.
        let buf: Vec<u8> = (0..16).flat_map(|_| [120u8, 40]).collect();
        let f = decode(&buf, 4, 4, b"YUYV").unwrap();
        assert!(!f.image.is_gray());
    }

    #[test]
    fn imports_emitter_configs() {
        let v7 = r#"
            [[devices]]
            vendor = "04f2"
            product = "b6dd"
            [devices.instruction]
            unit = 14
            selector = 6
            control = [1, 3, 3, 0, 0, 0, 0, 0, 0]
        "#;
        let c = import_emitter_config(v7).unwrap();
        assert_eq!(
            c,
            vec![EmitterControl {
                unit: 14,
                selector: 6,
                control: vec![1, 3, 3, 0, 0, 0, 0, 0, 0]
            }]
        );
    }
}
