//! Inference benchmark: `cargo run --release -p frogger-core --example bench -- <model dir> [image] [iterations]`
//! Prints the median time of a full frame analysis (detection + alignment +
//! embedding + eye patches). Without an image a synthetic frame is used.

use frogger_core::vision::{image, Image, Pipeline};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("target/models"));
    let p = Pipeline::load(
        &dir.join("face_detection_yunet_2023mar.onnx"),
        &dir.join("face_recognition_sface_2021dec.onnx"),
    )?;
    let img = match args.get(2) {
        Some(path) => image::load_file(path.as_ref())?.to_gray(),
        None => Image::from_gray(640, 480, (0..640 * 480).map(|i| (i * 31 % 251) as u8).collect()),
    };
    let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut times = Vec::with_capacity(n);
    let mut faces = 0;
    for _ in 0..n {
        let t = Instant::now();
        faces = p.analyse(&img)?.map(|a| a.face_count).unwrap_or(0);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{}x{} faces={} median={:.1} ms p10={:.1} p90={:.1} (n={n})",
        img.width,
        img.height,
        faces,
        times[n / 2],
        times[n / 10],
        times[n * 9 / 10]
    );
    Ok(())
}
