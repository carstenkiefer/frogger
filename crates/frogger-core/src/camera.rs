//! V4L2 camera access, IR-camera detection and IR emitter control.

use crate::config::{CameraConfig, DepthConfig, EmitterControl};
use crate::vision::depth::{decode_z16, DepthImage};
use crate::vision::Image;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC};

/// Pixel formats we can decode, best first for IR use.
/// Monochrome formats first (IR sensors), then YUV, then compressed/RGB.
const PREFERRED: [&[u8; 4]; 13] = [
    b"GREY", b"Y8I ", b"Y16 ", b"Y12 ", b"Y10 ", b"YUYV", b"UYVY", b"NV12", b"NV21", b"YU12", b"YV12",
    b"MJPG", b"RGB3",
];

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
    /// Depth sensor node (delivers Z16 depth maps).
    pub depth: bool,
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
    let depth = formats.iter().any(|f| f.trim() == "Z16");
    if depth {
        evidence.push("depth sensor (Z16)".into());
    }
    let kind = if depth && !mono_format && !colour_format {
        CameraKind::Unknown
    } else if mono_format && !colour_format {
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
        depth,
        evidence,
    })
}

/// `/dev/v4l/by-id/*` alias pointing at `path`.
///
/// `by-path` is preferred: it contains the USB interface number and is
/// therefore unique. `by-id` is only built from the serial number, so the RGB
/// and IR halves of a combined laptop camera compete for the same link and it
/// may point to either device after a reboot.
pub fn stable_alias(path: &Path) -> Option<PathBuf> {
    let target = std::fs::canonicalize(path).ok()?;
    for dir in ["/dev/v4l/by-path", "/dev/v4l/by-id"] {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let mut links: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        links.sort();
        // Skip legacy duplicates ("-usbv2-") when a plain link exists.
        links.sort_by_key(|l| l.to_string_lossy().contains("usbv2"));
        if let Some(l) = links
            .into_iter()
            .find(|l| std::fs::canonicalize(l).ok().as_deref() == Some(target.as_path()))
        {
            return Some(l);
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
        let p = PathBuf::from(&cfg.device);
        // Re-resolve when the configured node vanished (external camera on
        // another port) or now points at a colour camera (ambiguous by-id
        // link of combined RGB+IR laptop cameras, which share one USB id).
        let colour = p.exists() && probe(&p).map(|i| i.kind == CameraKind::Rgb).unwrap_or(false);
        if cfg.expected_usb_id.is_none() || (p.exists() && !colour) {
            return Ok(p);
        }
        log::warn!(
            "{} {}, searching for IR camera {:?}",
            cfg.device,
            if colour { "is a colour camera" } else { "not found" },
            cfg.expected_usb_id
        );
    }
    let cams = list_cameras();
    // With a pinned USB id only that camera qualifies; never fall back to an
    // arbitrary (e.g. RGB) device.
    let candidates: Vec<&CameraInfo> = cams
        .iter()
        .filter(|c| !c.depth)
        .filter(|c| cfg.expected_usb_id.is_none() || c.usb_id == cfg.expected_usb_id)
        .collect();
    let best = candidates
        .iter()
        .find(|c| c.kind == CameraKind::Infrared)
        .or_else(|| candidates.iter().find(|c| c.kind == CameraKind::Unknown))
        .or_else(|| {
            if cfg.expected_usb_id.is_none() {
                candidates.first()
            } else {
                None
            }
        })
        .context("no camera found")?;
    Ok(best.stable_path.clone().unwrap_or_else(|| best.path.clone()))
}

/// Depth node belonging to the IR camera: explicitly configured, or the Z16
/// node of the same USB device (Intel RealSense exposes IR and depth there).
pub fn find_depth_camera(cfg: &DepthConfig, ir: &CameraInfo) -> Option<PathBuf> {
    if cfg.device != "auto" {
        return Some(PathBuf::from(&cfg.device));
    }
    ir.usb_id.as_ref()?;
    list_cameras()
        .into_iter()
        .find(|c| c.depth && c.usb_id == ir.usb_id)
        .map(|c| c.stable_path.unwrap_or(c.path))
}

/// Background reader of a depth camera; keeps only the newest frame.
pub struct DepthStream {
    latest: Arc<Mutex<Option<(Instant, DepthImage)>>>,
    stop: Arc<AtomicBool>,
}

impl DepthStream {
    /// Start streaming; fails if no depth frame arrives within 3 s.
    pub fn start(path: &Path) -> Result<DepthStream> {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<()>>();
        let (shared, stop_flag, path) = (latest.clone(), stop.clone(), path.to_path_buf());
        std::thread::spawn(move || {
            let run = || -> Result<()> {
                let dev = Device::with_path(&path).with_context(|| format!("opening {}", path.display()))?;
                let mut fmt = dev.format()?;
                fmt.fourcc = FourCC::new(b"Z16 ");
                let fmt = dev.set_format(&fmt)?;
                if &fmt.fourcc.repr != b"Z16 " {
                    bail!("{} does not deliver Z16 depth maps", path.display());
                }
                let (w, h) = (fmt.width as usize, fmt.height as usize);
                let mut stream = Stream::with_buffers(&dev, Type::VideoCapture, 4)?;
                stream.set_timeout(Duration::from_millis(1500));
                let mut first = true;
                while !stop_flag.load(Ordering::Relaxed) {
                    let (buf, _) = stream.next()?;
                    if let Some(img) = decode_z16(buf, w, h) {
                        *shared.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), img));
                        if first {
                            first = false;
                            let _ = tx.send(Ok(()));
                        }
                    }
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = tx.send(Err(e));
            }
        });
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(DepthStream { latest, stop }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                bail!("depth camera delivered no frame")
            }
        }
    }

    /// Newest depth frame if it is not older than `max_age`.
    pub fn latest(&self, max_age: Duration) -> Option<DepthImage> {
        let g = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        g.as_ref()
            .filter(|(t, _)| t.elapsed() <= max_age)
            .map(|(_, img)| img.clone())
    }
}

impl Drop for DepthStream {
    fn drop(&mut self) {
        // The reader thread exits after its next frame (≤ one frame period);
        // not joined so the authentication result is not delayed.
        self.stop.store(true, Ordering::Relaxed);
    }
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
            let data = buf[..2 * n].as_chunks::<2>().0.iter().map(|p| p[1]).collect();
            (Image::from_gray(w, h, data), 0.0)
        }
        b"Y10 " | b"Y12 " => {
            // 16 bit little endian, LSB aligned (10 or 12 significant bits).
            if buf.len() < 2 * n {
                bail!("short Y10/Y12 frame");
            }
            let shift = if fourcc == b"Y10 " { 2 } else { 4 };
            let data = buf[..2 * n]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| (u16::from_le_bytes(*p) >> shift).min(255) as u8)
                .collect();
            (Image::from_gray(w, h, data), 0.0)
        }
        b"Y8I " => {
            // Interleaved stereo IR (e.g. Intel RealSense): use the left sensor.
            if buf.len() < 2 * n {
                bail!("short Y8I frame");
            }
            let data = buf[..2 * n].as_chunks::<2>().0.iter().map(|p| p[0]).collect();
            (Image::from_gray(w, h, data), 0.0)
        }
        b"UYVY" => {
            if buf.len() < 2 * n {
                bail!("short UYVY frame");
            }
            let yuyv: Vec<u8> = buf[..2 * n]
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[1], p[0], p[3], p[2]])
                .collect();
            return decode(&yuyv, w, h, b"YUYV");
        }
        b"NV12" | b"NV21" | b"YU12" | b"YV12" => {
            if buf.len() < n + n / 2 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
                bail!("short or odd-sized 4:2:0 frame");
            }
            let (y, c) = buf[..n + n / 2].split_at(n);
            let q = n / 4;
            let cw = w / 2;
            // (u, v) of the chroma sample covering chroma position i.
            let uv = |i: usize| -> (u8, u8) {
                match fourcc {
                    b"NV12" => (c[2 * i], c[2 * i + 1]),
                    b"NV21" => (c[2 * i + 1], c[2 * i]),
                    b"YU12" => (c[i], c[q + i]),
                    _ => (c[q + i], c[i]), // YV12
                }
            };
            let mut chroma_sum = 0u64;
            for i in 0..q {
                let (u, v) = uv(i);
                chroma_sum += (u as i32 - 128).unsigned_abs() as u64 + (v as i32 - 128).unsigned_abs() as u64;
            }
            let chroma = chroma_sum as f32 / q.max(1) as f32;
            if chroma < 2.0 {
                (Image::from_gray(w, h, y.to_vec()), chroma)
            } else {
                let mut out = Vec::with_capacity(3 * n);
                for row in 0..h {
                    for col in 0..w {
                        let (u, v) = uv((row / 2) * cw + col / 2);
                        out.extend(yuv_to_rgb(y[row * w + col], u, v));
                    }
                }
                (Image::from_rgb(w, h, out), chroma)
            }
        }
        b"YUYV" => {
            if buf.len() < 2 * n {
                bail!("short YUYV frame");
            }
            let mut y = Vec::with_capacity(n);
            let mut chroma_sum = 0u64;
            for px in buf[..2 * n].as_chunks::<4>().0 {
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
    for px in img.data.as_chunks::<3>().0.iter().step_by(7) {
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

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let (y, u, v) = (y as f32, u as f32 - 128.0, v as f32 - 128.0);
    [
        (y + 1.402 * v).clamp(0.0, 255.0) as u8,
        (y - 0.344 * u - 0.714 * v).clamp(0.0, 255.0) as u8,
        (y + 1.772 * u).clamp(0.0, 255.0) as u8,
    ]
}

fn yuyv_to_rgb(buf: &[u8], w: usize, h: usize) -> Image {
    let mut out = Vec::with_capacity(w * h * 3);
    for px in buf.as_chunks::<4>().0 {
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
    /// Measured frame rate and the rate of frames lit by the emitter (the
    /// latter is what liveness can use; a blink lasts 100–400 ms).
    pub fps: f32,
    pub lit_fps: f32,
}

pub fn capture_stats(cam: &mut Camera, frames: usize) -> Result<CaptureStats> {
    let mut means = Vec::new();
    let mut chroma = 0.0;
    // The first frames may come straight from the driver's queue; time only
    // the rest.
    const SKIP: usize = 3;
    let mut timed_from = None;
    for i in 0..frames.max(SKIP + 2) {
        let f = cam.frame()?;
        if i == SKIP {
            timed_from = Some(std::time::Instant::now());
        }
        chroma += f.chroma;
        means.push(f.mean);
    }
    let fps = timed_from
        .map(|t| (means.len() - SKIP - 1) as f32 / t.elapsed().as_secs_f32().max(1e-3))
        .unwrap_or(0.0);
    let mut strobe = crate::auth::StrobeFilter::default();
    let lit = means.iter().filter(|&&m| !strobe.is_off(m)).count();
    let n = means.len().max(1) as f32;
    let mean = means.iter().sum::<f32>() / n;
    let alt = means.windows(2).map(|w| (w[0] - w[1]).abs()).sum::<f32>() / (n - 1.0).max(1.0);
    Ok(CaptureStats {
        frames: means.len(),
        mean_chroma: chroma / n,
        mean_brightness: mean,
        alternation: if mean > 1.0 { alt / mean } else { 0.0 },
        fps,
        lit_fps: fps * lit as f32 / means.len().max(1) as f32,
    })
}

/// An IR image that is almost black and does not strobe: the emitter is most
/// likely switched off (common default; enable it with linux-enable-ir-emitter).
pub fn emitter_looks_off(s: &CaptureStats) -> bool {
    s.mean_brightness < 15.0 && s.alternation < 0.25
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
    fn decode_more_mono_formats() {
        // Y10: 10-bit value 800 → 200 after scaling to 8 bit.
        let y10: Vec<u8> = (0..16).flat_map(|_| 800u16.to_le_bytes()).collect();
        let f = decode(&y10, 4, 4, b"Y10 ").unwrap();
        assert!(f.image.is_gray() && f.image.data.iter().all(|&v| v == 200));
        let y12: Vec<u8> = (0..16).flat_map(|_| 4000u16.to_le_bytes()).collect();
        assert!(decode(&y12, 4, 4, b"Y12 ")
            .unwrap()
            .image
            .data
            .iter()
            .all(|&v| v == 250));
        let y8i: Vec<u8> = (0..16).flat_map(|_| [77u8, 3]).collect();
        assert!(decode(&y8i, 4, 4, b"Y8I ")
            .unwrap()
            .image
            .data
            .iter()
            .all(|&v| v == 77));
    }

    #[test]
    fn decode_yuv_variants() {
        // UYVY with neutral chroma → gray with the Y values.
        let uyvy: Vec<u8> = (0..8).flat_map(|_| [128u8, 60, 128, 61]).collect();
        let f = decode(&uyvy, 4, 4, b"UYVY").unwrap();
        assert!(f.image.is_gray());
        assert_eq!(&f.image.data[..2], &[60, 61]);
        // NV12 grey and coloured.
        let mut nv12 = vec![90u8; 16];
        nv12.extend([128u8; 8]);
        let f = decode(&nv12, 4, 4, b"NV12").unwrap();
        assert!(f.image.is_gray() && f.mean == 90.0);
        let mut nv12c = vec![90u8; 16];
        nv12c.extend([60u8, 200, 60, 200, 60, 200, 60, 200]);
        assert!(!decode(&nv12c, 4, 4, b"NV12").unwrap().image.is_gray());
        let mut yu12 = vec![40u8; 16];
        yu12.extend([128u8; 8]);
        assert!(decode(&yu12, 4, 4, b"YU12").unwrap().image.is_gray());
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
