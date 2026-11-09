#!/usr/bin/env bash
# Build-time setup only. Parley runs GigaAM natively, without this Python env.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UPSTREAM="$ROOT/target/gigaam-upstream"
REVISION=7447938d791c4f3e643386ee22c33777004293a5
if [[ ! -d "$UPSTREAM/.git" ]]; then
    git clone https://github.com/salute-developers/GigaAM.git "$UPSTREAM"
    git -C "$UPSTREAM" checkout --detach "$REVISION"
fi
if [[ "$(git -C "$UPSTREAM" rev-parse HEAD)" != "$REVISION" ]]; then
    echo "error: GigaAM source must be at $REVISION; use a separate clean checkout" >&2
    exit 1
fi
uv venv --python 3.12 --allow-existing "$ROOT/target/gigaam-export-env"
EXPORT_PY="$ROOT/target/gigaam-export-env/bin/python"
uv pip install --python "$EXPORT_PY" torch==2.10.0 torchaudio==2.10.0 --index-url https://download.pytorch.org/whl/cpu
uv pip install --python "$EXPORT_PY" -r "$ROOT/scripts/gigaam-export-requirements.txt"
uv pip install --python "$EXPORT_PY" --no-deps "$UPSTREAM"
"$EXPORT_PY" "$ROOT/scripts/export-gigaam.py" --output "$ROOT/target/gigaam-bundles"
"$EXPORT_PY" "$ROOT/scripts/install-gigaam-models.py" --bundles "$ROOT/target/gigaam-bundles"
# Match the symbol-versioned C API required by the locked sherpa-onnx build.
ORT_VERSION=1.24.4
ORT_ARCHIVE="$ROOT/target/onnxruntime-linux-x64-gpu_cuda13-$ORT_VERSION.tgz"
ORT_SHA256=fdc6eb18317b4eaeda8b3b86595e5da7e853f72bac67ccac9b04ffc20c9f7fe0
if [[ ! -f "$ORT_ARCHIVE" ]]; then
    curl -fL --retry 3 -o "$ORT_ARCHIVE.part" \
        "https://github.com/microsoft/onnxruntime/releases/download/v$ORT_VERSION/onnxruntime-linux-x64-gpu_cuda13-$ORT_VERSION.tgz"
    mv "$ORT_ARCHIVE.part" "$ORT_ARCHIVE"
fi
printf '%s  %s\n' "$ORT_SHA256" "$ORT_ARCHIVE" | sha256sum -c -
tar -xzf "$ORT_ARCHIVE" -C "$ROOT/target"
uv venv --python 3.12 --allow-existing "$ROOT/target/ort-gpu-env"
uv pip install --python "$ROOT/target/ort-gpu-env/bin/python" -r "$ROOT/scripts/gigaam-runtime-requirements.txt"
"$ROOT/target/ort-gpu-env/bin/python" "$ROOT/scripts/stage-gigaam-runtime.py" \
    "$ROOT/target/ort-gpu-env/lib/python3.12/site-packages" "$ROOT/target/gigaam-runtime" \
    --onnxruntime "$ROOT/target/onnxruntime-linux-x64-gpu-$ORT_VERSION"
echo "Prepared four model bundles and the native CUDA runtime."
