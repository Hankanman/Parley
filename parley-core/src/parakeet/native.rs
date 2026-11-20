//! Small exception-safe boundary around sherpa-onnx's offline C API.
use sherpa_onnx_sys as sys;
use std::{
    ffi::{CStr, CString, c_char},
    path::Path,
    ptr::NonNull,
};

unsafe extern "C" {
    fn parley_parakeet_create(
        config: *const sys::OfflineRecognizerConfig,
        error: *mut c_char,
        capacity: usize,
    ) -> *const sys::OfflineRecognizer;
    fn parley_parakeet_decode(
        recognizer: *const sys::OfflineRecognizer,
        samples: *const f32,
        count: i32,
        error: *mut c_char,
        capacity: usize,
    ) -> *const c_char;
}
pub struct Recognizer(NonNull<sys::OfflineRecognizer>);
// SAFETY: Like sherpa-onnx's upstream wrapper, one object may move between
// threads. Engine's mutex serializes every decode and destruction.
unsafe impl Send for Recognizer {}
impl Recognizer {
    pub fn load(
        dir: &Path,
        int8: bool,
        provider: &str,
        profile: Option<&Path>,
    ) -> Result<Self, String> {
        let suffix = if int8 { ".int8.onnx" } else { ".onnx" };
        let filename = |base: &str| {
            CString::new(
                dir.join(format!("{base}{suffix}"))
                    .to_string_lossy()
                    .as_bytes(),
            )
            .map_err(|e| e.to_string())
        };
        let encoder = filename("encoder")?;
        let decoder = filename("decoder")?;
        let joiner = filename("joiner")?;
        let tokens = CString::new(dir.join("tokens.txt").to_string_lossy().as_bytes())
            .map_err(|e| e.to_string())?;
        let model_type = CString::new("nemo_transducer").unwrap();
        let decoding = CString::new("greedy_search").unwrap();
        // An explicit diagnostic profile is used only by the isolated GPU
        // check. Session node events identify the actual execution provider.
        let provider = if let Some(prefix) = profile {
            let config = prefix.with_extension("config");
            std::fs::write(
                &config,
                format!("ProfilingFilePrefix={}\n", prefix.display()),
            )
            .map_err(|e| e.to_string())?;
            format!("{provider}:{}", config.display())
        } else {
            provider.into()
        };
        let provider = CString::new(provider).map_err(|e| e.to_string())?;
        // SAFETY: This repr(C) config contains only scalars, raw pointers and
        // nested C configs; zero is valid for every field. The native API
        // applies defaults to unused model families. Strings live through create.
        let mut config: sys::OfflineRecognizerConfig = unsafe { std::mem::zeroed() };
        config.feat_config.sample_rate = 16_000;
        config.feat_config.feature_dim = 128;
        config.model_config.transducer = sys::OfflineTransducerModelConfig {
            encoder: encoder.as_ptr(),
            decoder: decoder.as_ptr(),
            joiner: joiner.as_ptr(),
        };
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.model_type = model_type.as_ptr();
        config.model_config.provider = provider.as_ptr();
        config.model_config.num_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(2) as i32;
        config.decoding_method = decoding.as_ptr();
        config.max_active_paths = 4;
        config.lm_config.scale = 1.;
        let mut error = [0 as c_char; 4096];
        // SAFETY: The config and its string pointers remain valid for this
        // synchronous call. The C++ boundary catches all native exceptions.
        let ptr = unsafe { parley_parakeet_create(&config, error.as_mut_ptr(), error.len()) };
        NonNull::new(ptr.cast_mut())
            .map(Self)
            .ok_or_else(|| native_error(&error))
    }
    pub fn transcribe(&self, samples: &[f32]) -> Result<String, String> {
        let count = i32::try_from(samples.len())
            .map_err(|_| "Parakeet audio exceeds native input limit")?;
        let mut error = [0 as c_char; 4096];
        // SAFETY: Engine serializes access, samples stay borrowed for the call,
        // and the C++ wrapper destroys the stream on success and on exception.
        let json = unsafe {
            parley_parakeet_decode(
                self.0.as_ptr(),
                samples.as_ptr(),
                count,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if json.is_null() {
            return Err(native_error(&error));
        }
        // SAFETY: The API returns an independently allocated NUL-terminated
        // JSON string, released exactly once after copying/parsing it.
        let result = unsafe {
            let result = serde_json::from_slice::<sherpa_onnx::OfflineRecognizerResult>(
                CStr::from_ptr(json).to_bytes(),
            );
            sys::SherpaOnnxDestroyOfflineStreamResultJson(json);
            result
        };
        result.map(|r| r.text).map_err(|e| e.to_string())
    }
}
fn native_error(error: &[c_char]) -> String {
    // SAFETY: The zero-initialized buffer is always NUL-terminated, including
    // snprintf truncation and paths where the native function writes nothing.
    unsafe {
        CStr::from_ptr(error.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}
impl Drop for Recognizer {
    fn drop(&mut self) {
        // SAFETY: We uniquely own the native allocation; no stream outlives a
        // decode call, and Engine's mutex prevents concurrent destruction.
        unsafe {
            sys::SherpaOnnxDestroyOfflineRecognizer(self.0.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_onnx_returns_error_without_unwinding_into_rust() {
        let dir = tempfile::tempdir().unwrap();
        for file in ["encoder.onnx", "decoder.onnx", "joiner.onnx"] {
            std::fs::write(dir.path().join(file), b"invalid ONNX protobuf").unwrap();
        }
        std::fs::write(dir.path().join("tokens.txt"), "<blk> 0\nhello 1\n").unwrap();
        let error = Recognizer::load(dir.path(), false, "cpu", None)
            .err()
            .expect("Invalid ONNX was accepted");
        assert!(!error.is_empty());
    }
}
