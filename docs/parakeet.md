# Parakeet TDT v3

Settings → Transcription → Speech recognition → **Parakeet TDT v3 · Multilingual**.
Download and select one of these models:

| Model | Download | Inference |
| --- | --- | --- |
| `parakeet-tdt-0.6b-v3` | ~2.4 GiB | NVIDIA CUDA, with CPU fallback |
| `parakeet-tdt-0.6b-v3-int8` | ~640 MiB | CPU INT8 |

CPU builds, including the standard CI AppImage, default to INT8. Linux CUDA
builds default to FP32, which uses CUDA automatically when the shared ONNX
Runtime can initialize it. The active engine status shows the actual device
after loading.

For a CUDA AppImage, set `PARLEY_ONNX_RUNTIME_DIR` to a directory with a
CUDA-enabled ONNX Runtime that matches the version used by the locked
sherpa-onnx release. Include `libonnxruntime.so`, its versioned library,
the CUDA and shared provider libraries, and their redistributable CUDA/cuDNN
dependencies. Run `PARLEY_ONNX_RUNTIME_DIR=/path/to/runtime ./build.sh cuda`.
The package places these libraries together in `usr/lib` so ONNX Runtime
can load its CUDA provider there. Standard AppImages retain their usual
library layout.

The external FP32 `encoder.weights` file is part of the model bundle. Each
download is verified against a pinned revision, size and SHA-256 before
installation. Starting a recording checks only file sizes. Both models
support progress, cancellation, retries and deletion. Weights stay outside the
AppImage under the app data directory at `models/parakeet/<model>/`.

`PARLEY_PARAKEET_DEVICE=cpu` forces CPU for diagnostics; `cuda` requires CUDA
for the FP32 model and returns an error instead of falling back. The default
is `auto`. An INT8 model always uses CPU. CUDA registration failures fall back
to CPU with a warning in auto mode. Allocation errors are reported without
changing the selected model. An exception-safe C++ boundary catches native
load/decode exceptions before they cross the Rust FFI boundary.

The model supports 25 European languages including Russian, English and
Ukrainian, with automatic language detection, punctuation and capitalization.
Whisper's language hints and glossary prompt do not apply to Parakeet.

Recording uses VAD speech segments and previews of the last six seconds.
Final transcription takes priority over previews. Imports and retranscription
retain speaker attribution and time offsets. Long segments split near quiet
boundaries with a 24-second maximum. Parakeet supplies no confidence score:
live events use the existing `0.85` fallback and decoding skips Whisper hallucination filters.

A weak registry shares one recognizer between tasks and rejects model switches
while it is in use. After the last task and its blocking inference finish, the
recognizer is destroyed and model allocations are released. Cancelling an
async caller cannot free a recognizer still decoding. Small runtime/driver
allocations may remain until process exit.

## Sources

- [NVIDIA model card and CC-BY-4.0 license](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3)
- [sherpa-onnx model documentation](https://k2-fsa.github.io/sherpa/onnx/pretrained_models/offline-transducer/nemo-transducer-models.html#sherpa-onnx-nemo-parakeet-tdt-0-6b-v3-int8-25-european-languages)
- [Pinned FP32 weights](https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3/tree/1a468a35cbba69418f126de829e75261dea4a4e4)
- [Pinned INT8 weights](https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tree/2bda32ec70b097a55adaa07d9a7173915b43cc78)

## Validation

The `parakeet_pipeline_check` example refuses to use normal app data or
recordings directories: all XDG directories must be under
`target/parakeet-e2e/`. Configure `config/user-dirs.dirs` to point to the
isolated `documents` directory. Copy normal `speaker_models` into the isolated
app data directory so the VAD/speaker pipeline is available.

```bash
cargo +1.99.0 test --release -p parley-core -p parley-gpui
cargo +1.99.0 build --release -p parley-gpui --example parakeet_pipeline_check
XDG_DATA_HOME="$PWD/target/parakeet-e2e/data" \
XDG_CONFIG_HOME="$PWD/target/parakeet-e2e/config" \
XDG_CACHE_HOME="$PWD/target/parakeet-e2e/cache" \
XDG_STATE_HOME="$PWD/target/parakeet-e2e/state" \
LD_LIBRARY_PATH="$PWD/target/release" \
  target/release/examples/parakeet_pipeline_check /path/to/public-speech.wav
```

To check CUDA with a compatible shared runtime, set `PARLEY_PARAKEET_DEVICE=cuda`
and pass `parakeet-tdt-0.6b-v3` as the final argument. It exercises download/cancel/retry, checksums,
previews, long audio, session sharing, busy guards, cancellation, live workers,
import, retranscription and deletion. Verified fixtures are retained under
`target/parakeet-e2e/verified-<model>/` after installed test weights are deleted.

Set `PARLEY_PARAKEET_PROFILE` to an absolute prefix inside the isolated test
directory to enable diagnostic ONNX Runtime profiles. CUDA node events in
these JSON files verify execution on GPU; the GPU check also prints process
VRAM allocations and rejects CPU fallback. Profiling is off by default.
