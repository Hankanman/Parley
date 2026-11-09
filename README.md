<div align="center" style="border-bottom: none">
    <h1>
        Parley — Private AI Meeting Notes for Linux
    </h1>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue" alt="License"></a>
    <a href="https://github.com/Hankanman/Parley/releases"><img src="https://img.shields.io/badge/Supported_OS-Linux-white" alt="Supported OS"></a>
    <a href="https://github.com/Hankanman/Parley/releases"><img alt="GitHub Tag" src="https://img.shields.io/github/v/tag/Hankanman/Parley?include_prereleases&color=yellow"></a>
    <br>
    <h3>Open Source • Private by Design • Native Linux</h3>

Parley records your meetings, transcribes them live and writes the summary, the decisions and the action items, all on your own machine. No account, no telemetry, and no cloud unless you choose to add one.

**[meetparley.app](https://meetparley.app)** · [Download](https://github.com/Hankanman/Parley/releases/latest) · [User guide](https://meetparley.app/docs/) · [40-second tour](https://meetparley.app/#video)

<img src="docs/images/hero.png" alt="Parley showing a meeting with its transcript, chapters and AI summary" width="900">

</div>

---

## Features

- **Records both sides of any call.** Microphone and system audio are captured straight from PipeWire, so it works with Meet, Zoom, Teams or anything else that makes sound, and no bot joins your meeting. The mic gets echo cancellation, noise suppression and automatic gain; the two sources are kept on separate channels.
- **Live transcription on your device.** [Whisper](https://github.com/ggml-org/whisper.cpp) runs in-process, GPU-accelerated on NVIDIA (CUDA) or AMD/Intel (Vulkan), with a CPU fallback. [GigaAM v3](docs/gigaam.md) adds native Russian ASR with four CTC/RNNT variants, CUDA or CPU inference, and automatic model unloading. Its models and runtime are prepared separately at build time; no Python service is needed. [Parakeet TDT v3](docs/parakeet.md) adds 25-language recognition including Russian, using local CUDA FP32 or CPU INT8 inference with models downloadable in Settings.
- **Knows who said what.** On-device speaker diarization, plus voice profiles: name a voice once and it is recognised in later meetings.
- **Summaries from templates.** Stand-ups, client calls, retros, project syncs and more, with key decisions, chapters and bookmarks.
- **Ask across every meeting.** Plain-language questions over all your transcripts, with answers that cite the meeting and timestamp they came from.
- **Action items.** Picked out of each meeting with owners and due dates, and collected in one list. Draft a follow-up email from any meeting.
- **Calendar aware.** Subscribe to iCal feeds; meetings pick up their title, attendees and location.
- **Import and re-transcribe.** Turn an existing audio file into a meeting, or run a past meeting again with a different model or language.
- **Export.** Markdown, PDF, Word, JSON or SRT subtitles.
- **Crash safe.** Audio is checkpointed while recording; an interrupted meeting is recovered on the next launch.
- **MCP server.** [`parley-mcp`](https://meetparley.app/docs/mcp.html) exposes your meetings, transcripts, summaries and action items to Claude and other MCP clients, reading the local database directly. It ships in the AppImage and the installer puts it at `~/.local/bin/parley-mcp`.

### Your choice of AI

Summaries, Ask and action items use a language model of your choosing:

- **Built in**: a local model run by the bundled llama.cpp helper. Nothing to set up.
- **Ollama**, on this machine or another one you run.

Ask's semantic search (finding passages by meaning, not just keywords) runs on-device with a built-in embedding model, or with Ollama if that's your provider.
- **Claude, Groq, OpenRouter, OpenAI** or any **OpenAI-compatible** endpoint, used only if you add a key.

## Screenshots

All screenshots use the fictional demo data in [`parley-core/examples/demo/seed.sql`](parley-core/examples/demo/seed.sql); regenerate them with `scripts/showcase-screenshots.sh`.

| Ask across meetings | Action items |
|---|---|
| ![Ask Parley](docs/images/ask.png) | ![Action items](docs/images/actions.png) |
| **Speakers** | **Record** |
| ![Speakers](docs/images/speakers.png) | ![Record](docs/images/record.png) |

## Privacy

Audio, transcripts, summaries, voice profiles and settings are stored on your disk and never uploaded by Parley. There is no telemetry or crash reporting. Parley only uses the network when you ask it to: downloading models, syncing calendar feeds you added, or calling a remote LLM provider you configured. See [Your data and privacy](https://meetparley.app/docs/data.html) for where everything is stored, and the [privacy policy](PRIVACY_POLICY.md) for the full list.

## Installation

Parley is Linux-only (x86_64) and ships as an AppImage for any distribution with glibc 2.39+ and PipeWire (Ubuntu 24.04, Debian 13, Fedora 40, Arch, or newer). Install or update it with:

```bash
curl -fsSL https://meetparley.app/install.sh | sh
```

This picks the Vulkan (GPU) build if you have an NVIDIA, AMD or Intel GPU with a Vulkan driver, or the CPU build otherwise, verifies its checksum, installs it as `~/.local/bin/parley` (plus the `parley-mcp` MCP server) and adds Parley to your application menu, with no root needed. `curl -fsSL https://meetparley.app/install.sh | sh -s -- --uninstall` removes it again (your meetings and settings stay). The script is [`site/install.sh`](site/install.sh) if you'd like to read it first.

Or download the `.AppImage` from the [latest release](https://github.com/Hankanman/Parley/releases/latest) yourself, make it executable (`chmod +x`) and run it. AppImages need FUSE's `fusermount`, which most desktops already have (otherwise install your distribution's `fuse3` package).

The first run walks you through downloading a Whisper model and the built-in summary model. The [user guide](https://meetparley.app/docs/) covers everything after that: [installing](https://meetparley.app/docs/install.html), [recording](https://meetparley.app/docs/recording.html), [models and AI providers](https://meetparley.app/docs/models.html) and [troubleshooting](https://meetparley.app/docs/troubleshooting.html).

## Build from source

Needs Rust, cmake and clang; see [`docs/building_in_linux.md`](docs/building_in_linux.md) for the full list of packages.

```bash
git clone https://github.com/Hankanman/Parley.git
cd Parley
./build.sh           # auto: CUDA if an NVIDIA GPU is present, CPU otherwise
./build.sh cuda      # NVIDIA
./build.sh vulkan    # AMD / Intel
./build.sh cpu       # CPU only
./build.sh --help
```

This produces `Parley-<version>-x86_64.AppImage`. For development, `./dev.sh` (same `cuda` / `vulkan` / `cpu` options) runs a debug build directly.

`build.sh` also handles the Fedora / CUDA 13 build environment (a `g++-15` host compiler for nvcc, `CUDAARCHS` for Turing and newer, `NO_STRIP=1` for linuxdeploy); each can be overridden with an environment variable. `./clean.sh` removes `target/` and leaves your data and models alone.

More detail: [`docs/BUILDING.md`](docs/BUILDING.md), [`docs/GPU_ACCELERATION.md`](docs/GPU_ACCELERATION.md).

## Architecture

Parley is a single native process; there is no server and no webview.

- **`parley-core`**: audio capture and processing, voice activity detection, Whisper/GigaAM transcription, speaker diarization, SQLite storage, and the summary / LLM providers. No UI dependency.
- **`parley-gpui`**: the desktop app, built with [GPUI](https://www.gpui.rs/) (the UI framework behind the Zed editor). It calls `parley-core` directly.
- **`llama-helper`**: the sidecar that runs the built-in summary model with llama.cpp.
- **`parley-mcp`**: the standalone MCP server.

See [docs/architecture.md](docs/architecture.md) for more.

## Contributing

PRs welcome. Releases follow semantic versioning; see [RELEASING.md](RELEASING.md). Open an issue first if you're planning a non-trivial change so we can agree on direction. If you're contributing a fix, make sure `./build.sh` produces a working AppImage on your distro before opening the PR.

## License

[MIT](LICENSE). Use it, fork it, ship it.

## Origins and acknowledgments

Parley started as a fork of [Zackriya-Solutions/meetily](https://github.com/Zackriya-Solutions/meetily), the MIT-licensed Meetily Community Edition, and has since been developed independently: it shares no infrastructure, releases or roadmap with that project. Since the fork it has moved to Linux only with native PipeWire capture, replaced the Tauri + React shell with a native GPUI app, and added speaker recognition, Ask, action items, calendar support and the MCP server.

Thanks to:

- **[Zackriya-Solutions/meetily](https://github.com/Zackriya-Solutions/meetily)**, the project Parley grew out of, and its lead author [Sujith S](https://github.com/sujithatzackriya) and community.
- **[whisper.cpp](https://github.com/ggml-org/whisper.cpp)** and **[whisper-rs](https://codeberg.org/tazz4843/whisper-rs)**, for transcription.
- **[llama.cpp](https://github.com/ggml-org/llama.cpp)**, for the built-in summary model.
- **[sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)**, for voice activity detection and speaker diarization.
- **[GPUI](https://www.gpui.rs/)** and **[gpui-component](https://github.com/longbridge/gpui-component)**, for the UI.
- **[Screenpipe](https://github.com/mediar-ai/screenpipe)**, whose audio capture code early versions built on.

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=Hankanman/Parley&type=Date)](https://star-history.com/#Hankanman/Parley&Date)
