// Whisper GPU benchmark: live-style 8 s segments (beam 5 final + greedy partial) and a 5-min batch.
// Build against whisper.cpp (Vulkan or CUDA); see docs/benchmarks/2026-11-gpu.md.
// Usage: whisper_bench <model.bin> <audio.f32 16 kHz mono> <flash_attn 0|1>
#include "whisper.h"
#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <vector>
using clk = std::chrono::steady_clock;
static double secs(clk::time_point a) { return std::chrono::duration<double>(clk::now() - a).count(); }
static void quiet(ggml_log_level, const char *, void *) {}
int main(int argc, char **argv) {
    const char *model = argv[1], *pcm = argv[2];
    bool fa = atoi(argv[3]);
    std::ifstream f(pcm, std::ios::binary);
    f.clear(); f.seekg(0, std::ios::end); size_t n = f.tellg() / sizeof(float); f.seekg(0);
    std::vector<float> audio(n); f.read((char *)audio.data(), n * sizeof(float));
    whisper_log_set(quiet, nullptr);
    auto cp = whisper_context_default_params();
    cp.use_gpu = true; cp.flash_attn = fa;
    auto t = clk::now();
    whisper_context *ctx = whisper_init_from_file_with_params(model, cp);
    if (!ctx) { fprintf(stderr, "load failed\n"); return 1; }
    double load = secs(t);
    auto run = [&](const float *d, int len, bool greedy) {
        auto p = whisper_full_default_params(greedy ? WHISPER_SAMPLING_GREEDY : WHISPER_SAMPLING_BEAM_SEARCH);
        p.n_threads = 4; p.language = "en"; p.print_progress = false; p.print_realtime = false; p.print_timestamps = false; p.print_special = false;
        if (!greedy) { p.beam_search.beam_size = 5; p.beam_search.patience = 1.0f; }
        auto t0 = clk::now(); whisper_full(ctx, p, d, len); return secs(t0);
    };
    const int SR = 16000, SEG = 8 * SR, NSEG = 20;
    run(audio.data(), SEG, true);  // warm-up
    std::vector<double> fin, part;
    for (int i = 0; i < NSEG; i++) { fin.push_back(run(audio.data() + i * SEG, SEG, false)); part.push_back(run(audio.data() + i * SEG, SEG, true)); }
    auto med = [](std::vector<double> v) { std::sort(v.begin(), v.end()); return v[v.size() / 2]; };
    auto p90 = [](std::vector<double> v) { std::sort(v.begin(), v.end()); return v[(v.size() * 9) / 10]; };
    int batch = std::min<size_t>(n, 300 * SR);
    double b = run(audio.data(), batch, false);
    printf("load %.2fs | 8s segment, beam5 final: median %.3fs p90 %.3fs | greedy partial: median %.3fs | 5-min batch beam5: %.1fs (%.0fx realtime)\n",
           load, med(fin), p90(fin), med(part), b, (batch / (double)SR) / b);
    whisper_free(ctx);
}
