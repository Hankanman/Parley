# Portable ggml CPU baseline for release builds (issue #155).
#
# build.sh points CMAKE_PROJECT_INCLUDE here, so CMake runs this after every
# project() call. whisper-rs-sys leaves GGML_NATIVE at its default (ON), which
# compiles whisper.cpp with -march=native for whatever CPU the CI runner had:
# 0.5.1 shipped AVX-512 code that crashed with "Illegal instruction" on
# AVX2-only CPUs. Turning it off alone would also drop AVX2/FMA/F16C, so the
# baseline is set explicitly: x86-64-v3 (Haswell, 2013, and later).
#
# Every value is a non-FORCE cache default: anything the build already passed
# with -D wins. llama-cpp-sys-2 sets its own GGML_* options this way, so this
# file only changes whisper.cpp's ggml.
set(GGML_NATIVE OFF CACHE BOOL "ggml: optimize the build for the current system")
foreach(feature SSE42 AVX AVX2 BMI2 FMA F16C)
    set(GGML_${feature} ON CACHE BOOL "ggml: enable ${feature}")
endforeach()
