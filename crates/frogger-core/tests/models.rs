//! Runs the real ONNX models (needs `packaging/fetch-models.sh`):
//! `FROGGER_MODEL_DIR=target/models cargo test -p frogger-core -- --ignored`
//! Proves that the inference engine supports all models on this platform.

use frogger_core::vision::{align, embed, eyestate::EyeStateModel, Image, Pipeline};
use std::path::PathBuf;

fn model_dir() -> PathBuf {
    PathBuf::from(std::env::var("FROGGER_MODEL_DIR").unwrap_or_else(|_| "../../target/models".into()))
}

fn pipeline() -> Pipeline {
    let dir = model_dir();
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

#[test]
#[ignore]
fn eye_state_model_runs() {
    let m = EyeStateModel::load(&model_dir().join("open_closed_eye.onnx")).expect("eye model loads");
    let id = align::Similarity::estimate(&align::TEMPLATE, &align::TEMPLATE);
    // Uniform and patterned crops: probabilities in [0, 1], deterministic,
    // never NaN (the graph's own softmax overflows; we use the logits).
    for img in [
        Image::from_gray(112, 112, vec![255; 112 * 112]),
        Image::from_gray(112, 112, (0..112 * 112).map(|i| ((i * 37) % 251) as u8).collect()),
    ] {
        let a = m.closed(&img, &id).unwrap();
        let b = m.closed(&img, &id).unwrap();
        assert_eq!(a, b);
        assert!(
            a.iter().all(|p| p.is_finite() && (0.0..=1.0).contains(p)),
            "{a:?}"
        );
    }
}
