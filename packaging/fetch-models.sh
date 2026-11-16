#!/usr/bin/env bash
# Download the ONNX models (OpenCV Zoo via its Hugging Face mirror, Intel
# Open Model Zoo) and verify their SHA-256 checksums.
# Output: target/models (or $1).
set -euo pipefail
OUT=${1:-$(cd "$(dirname "$0")/.." && pwd)/target/models}
BASE=https://huggingface.co/opencv
mkdir -p "$OUT"

fetch_url() { # name url sha256
  local f="$OUT/$1"
  if [ ! -f "$f" ] || ! echo "$3  $f" | sha256sum -c --status; then
    curl -fsSL --retry 4 -o "$f.part" "$2"
    mv "$f.part" "$f"
  fi
  echo "$3  $f" | sha256sum -c
}

fetch() { # name repo sha256
  fetch_url "$1" "$BASE/$2/resolve/main/$1" "$3"
}

fetch face_detection_yunet_2023mar.onnx face_detection_yunet \
  8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4
fetch face_recognition_sface_2021dec.onnx face_recognition_sface \
  0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79
# Eye state (open/closed), trained on infrared eye images (MRL Eye Dataset).
fetch_url open_closed_eye.onnx \
  https://storage.openvinotoolkit.org/repositories/open_model_zoo/public/2022.1/open-closed-eye-0001/open_closed_eye.onnx \
  4daa100034482525a26c9afb9297c16580a531189e66e3d2b2ac7d32becfd593
