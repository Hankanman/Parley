# Native GigaAM v3

Parley runs GigaAM inside its Rust process through ONNX Runtime. There is no
Python service or helper process at application runtime. The four ASR models
are `v3_ctc`, `v3_rnnt`, `v3_e2e_ctc`, and `v3_e2e_rnnt` (the recommended first
choice for Russian text with punctuation).

## Prepare a Linux/NVIDIA build

Install `uv`, Python 3.12, ffmpeg, git, curl and patchelf, plus the normal Parley
build dependencies. The prepared GPU runtime uses CUDA 13 and requires a
compatible NVIDIA driver. On other hosts inference falls back to CPU.
Run:

```bash
./scripts/prepare-gigaam.sh
./build.sh cuda
```

Use the CUDA compiler/architecture overrides described in the root README
when required by your toolkit or GPU.

Preparation installs **build-only** Python environments under `target/`, pins
the official GigaAM revision, downloads the official checkpoints, exports
FP32 ONNX models and creates Python reference fixtures. It prepares a single
ONNX Runtime 1.24.4 GPU library shared by GigaAM and sherpa-onnx, along with
its CUDA 13/cuDNN libraries. Whisper keeps the backend selected by the
normal build (CUDA 12 on the tested Ubuntu machine). Host NVIDIA driver libraries (`libcuda`, `libnvidia-*`) must never be included in the bundle.
The native CUDA 13 archive is pinned by SHA-256. Its runtime version matches
the symbol-versioned `OrtGetApiBase` dependency in the locked sherpa-onnx
release; substituting another ONNX Runtime version is not sufficient even if
its C API supports older API tables. Use a fresh staging directory when
changing runtime versions.
The build script uses `target/gigaam-runtime` automatically when present;
`PARLEY_ONNX_RUNTIME_DIR` overrides that directory.

Preparation installs the bundles under the app data directory, at
`models/gigaam/<model>/`. The installer respects the legacy `com.meetily.ai`
directory so it cannot hide an existing database by bypassing migration.
`python3 scripts/install-gigaam-models.py --bundles target/gigaam-bundles`
can repeat this step independently.
The exporter writes `manifest.json` last and publishes each bundle atomically.
All ONNX/tokenizer files have size and SHA-256 metadata; inference checks both.
Model weights remain separate from the AppImage. They need no network access
once installed. Missing bundles appear as "Not installed" in Settings; this
initial integration does not download or export GigaAM models from the UI.
The preparation script installs all four FP32 bundles (about 3.4 GB total);
the CUDA runtime adds about 2 GB to the AppImage.

In Settings → Transcription choose GigaAM, then a model. Import and
retranscription also list installed GigaAM models; their default uses the saved
active model. GigaAM v3 recognizes Russian and does not use Whisper language
hints or the Whisper glossary prompt.

## Lifetime and scheduling

A weak registry shares a loaded model between tasks, never keeps it alive by
itself, and rejects requests to swap a model while it is in use. Active model
selection is locked during transcription. A task keeps a strong reference
through its blocking inference, including when its async caller is cancelled.
After the last operation and queued inference finish, the model's ONNX
sessions and buffers are destroyed. Small runtime/driver allocations may
remain until app exit; model weights and inference arenas are released.

Final live segments take priority over partial previews and batch work.
Previews re-decode the trailing six seconds and are skipped while the model
is busy. GigaAM input segments have a hard 24-second bound; long audio is
split near quiet boundaries while retaining channel and time offsets. VAD,
speaker attribution and meeting persistence use the existing Parley pipeline.
GigaAM does not invent a confidence score or apply Whisper hallucination rules.

CUDA registration failures fall back to CPU with the device logged/displayed.
Out-of-memory errors are reported, and do not change the selected model or
provider. `PARLEY_GIGAAM_DEVICE=cpu` forces CPU for diagnostics.

## Validation

```bash
cargo test -p gigaam-core
cargo build -p gigaam-core --examples --release
# Run each device in a separate process: ORT's runtime initialization is global.
target/release/examples/validate target/release/libonnxruntime.so target/gigaam-bundles cpu
target/release/examples/validate target/release/libonnxruntime.so target/gigaam-bundles cuda
```

`validate` checks native log-mel features against exported torch features,
compares all four texts exactly against official Python inference, validates
word-time ordering and measures real-time factor. No test downloads data.
Use a copied database and isolated XDG directories for end-to-end import,
retranscription, live pipeline, cancellation and AppImage launch checks.

Upstream GigaAM and its checkpoints are MIT licensed:
https://github.com/salute-developers/GigaAM
The native frontend/decoder implementation follows that project's exported
configuration and decoding algorithms. SentencePiece model decoding uses
`sentencepiece-rs` (Apache-2.0); ONNX Runtime is MIT licensed.
