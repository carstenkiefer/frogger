# Third-party components

## Models (shipped in the packages)

| Model | Purpose | Source | License |
|---|---|---|---|
| `face_detection_yunet_2023mar.onnx` | Face detection + 5 landmarks (YuNet) | [OpenCV Zoo](https://huggingface.co/opencv/face_detection_yunet) | MIT, © 2020 Shiqi Yu |
| `face_recognition_sface_2021dec.onnx` | 128-d face embeddings (SFace) | [OpenCV Zoo](https://huggingface.co/opencv/face_recognition_sface) | Apache-2.0 |

Both licenses allow commercial use and redistribution. `packaging/fetch-models.sh`
pins the files by SHA-256.

Models that were evaluated and **not** used:

* **InsightFace** (ArcFace, SCRFD, buffalo_*) – excellent accuracy, but the
  pretrained weights are licensed for non-commercial research only.
* **dlib** `shape_predictor_68_face_landmarks` (used by Howdy) – trained on
  iBUG 300-W, whose license excludes commercial use. dlib's ResNet face
  recognition model would be usable, but requires a C++ toolchain/library at
  build and run time.
* **MediaPipe Face Mesh** (Apache-2.0) – usable license, but only distributed
  as TFLite; a converted ONNX would have to be maintained by us.

## Rust crates

Inference runs on [tract](https://github.com/sonos/tract) (MIT/Apache-2.0), a
pure-Rust ONNX runtime – no C++ runtime, no GPU drivers, identical builds on
amd64 and arm64. Camera access uses [v4l](https://github.com/raymanfx/libv4l-rs)
(MIT). All dependencies are MIT, Apache-2.0, BSD or similarly permissive; see
`Cargo.lock` (e.g. `cargo license` / `cargo deny check licenses`).
