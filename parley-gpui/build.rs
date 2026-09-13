// sherpa-onnx is linked with its `shared` feature (see parley-core's
// Cargo.toml for why), so libsherpa-onnx-c-api.so / libonnxruntime.so are
// copied next to the binary in target/<profile>/. Embed an `$ORIGIN` rpath so
// the binary finds them there without LD_LIBRARY_PATH (dev.sh sets that for
// the Tauri app) — and so a packaged build can ship them alongside it.
fn main() {
    println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
}
