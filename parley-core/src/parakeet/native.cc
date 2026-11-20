// sherpa's C API does not catch ONNX Runtime's C++ exceptions. Keep failures
// (including CUDA initialization and allocation errors) on the C++ side of
// the ABI, so Rust can report them without unwinding across an extern C call.
#include <cstdint>
#include <cstdio>
#include <exception>

extern "C" {
struct SherpaOnnxOfflineRecognizerConfig;
struct SherpaOnnxOfflineRecognizer;
struct SherpaOnnxOfflineStream;
const SherpaOnnxOfflineRecognizer *SherpaOnnxCreateOfflineRecognizer(
    const SherpaOnnxOfflineRecognizerConfig *);
void SherpaOnnxDestroyOfflineRecognizer(const SherpaOnnxOfflineRecognizer *);
const SherpaOnnxOfflineStream *SherpaOnnxCreateOfflineStream(
    const SherpaOnnxOfflineRecognizer *);
void SherpaOnnxDestroyOfflineStream(const SherpaOnnxOfflineStream *);
void SherpaOnnxAcceptWaveformOffline(const SherpaOnnxOfflineStream *, int32_t,
                                   const float *, int32_t);
void SherpaOnnxDecodeOfflineStream(const SherpaOnnxOfflineRecognizer *,
                                  const SherpaOnnxOfflineStream *);
const char *SherpaOnnxGetOfflineStreamResultAsJson(const SherpaOnnxOfflineStream *);
}

extern "C" const SherpaOnnxOfflineRecognizer *parley_parakeet_create(
    const SherpaOnnxOfflineRecognizerConfig *config, char *error,
    size_t capacity) noexcept {
  try {
    auto result = SherpaOnnxCreateOfflineRecognizer(config);
    if (!result) std::snprintf(error, capacity, "Invalid Parakeet configuration");
    return result;
  } catch (const std::exception &e) {
    std::snprintf(error, capacity, "%s", e.what());
  } catch (...) {
    std::snprintf(error, capacity, "Unknown native Parakeet loading failure");
  }
  return nullptr;
}

extern "C" const char *parley_parakeet_decode(
    const SherpaOnnxOfflineRecognizer *recognizer, const float *samples,
    int32_t count, char *error, size_t capacity) noexcept {
  struct Stream {
    const SherpaOnnxOfflineStream *ptr = nullptr;
    ~Stream() { if (ptr) SherpaOnnxDestroyOfflineStream(ptr); }
  } stream;
  try {
    stream.ptr = SherpaOnnxCreateOfflineStream(recognizer);
    if (!stream.ptr) {
      std::snprintf(error, capacity, "Cannot create Parakeet audio stream");
      return nullptr;
    }
    SherpaOnnxAcceptWaveformOffline(stream.ptr, 16000, samples, count);
    SherpaOnnxDecodeOfflineStream(recognizer, stream.ptr);
    auto result = SherpaOnnxGetOfflineStreamResultAsJson(stream.ptr);
    if (!result) std::snprintf(error, capacity, "Parakeet returned no result");
    return result;
  } catch (const std::exception &e) {
    std::snprintf(error, capacity, "%s", e.what());
  } catch (...) {
    std::snprintf(error, capacity, "Unknown native Parakeet decoding failure");
  }
  return nullptr;
}
