//! Runs the real ONNX models (needs `packaging/fetch-models.sh`):
//! `FROGGER_MODEL_DIR=target/models cargo test -p frogger-core -- --ignored`
//! Proves that the inference engine supports both models on this platform.

use frogger_core::vision::{align, embed, Image, Pipeline};
use std::path::PathBuf;

fn pipeline() -> Pipeline {
    let dir =
        PathBuf::from(std::env::var("FROGGER_MODEL_DIR").unwrap_or_else(|_| "../../target/models".into()));
    Pipeline::load(
        &dir.join("face_detection_yunet_2023mar.onnx"),
        &dir.join("face_recognition_sface_2021dec.onnx"),
    )
    .expect("models load")
}

#[test]
#[ignore]
fn models_run_and_behave() {
    let p = pipeline();
    // A uniform frame contains no face.
    let blank = Image::from_gray(640, 480, vec![90; 640 * 480]);
    assert!(p.detector.detect(&blank).unwrap().is_empty());

    // Embeddings are unit vectors and deterministic.
    let mut crop = Image::new(112, 112, 1);
    for (i, v) in crop.data.iter_mut().enumerate() {
        *v = ((i * 37) % 251) as u8;
    }
    let a = p.embedder.embed(&crop).unwrap();
    let b = p.embedder.embed(&crop).unwrap();
    assert_eq!(a.len(), embed::DIM);
    assert!((a.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-3);
    assert!(embed::cosine(&a, &b) > 0.999);

    // Alignment of a frame through the canonical template is the identity.
    let (aligned, t) = align::align_face(&crop, &align::TEMPLATE);
    assert!((t.scale() - 1.0).abs() < 1e-3);
    assert_eq!(aligned.width, 112);
}
