# Parley Privacy Policy

*Last updated: 23 October 2026*

Parley is a desktop app that runs entirely on your own machine. There is no Parley server, no account, and no company behind it collecting anything. This document explains exactly what leaves your computer, and when.

## No telemetry

Parley collects **no** usage analytics, crash reports, or tracking data of any kind. It has no analytics SDK and no "phone home" call. Logs are written only to your own disk (`~/.local/state/parley/logs/`).

## What stays on your machine

Everything Parley produces is stored locally and never uploaded by Parley:

- **Audio recordings**: `~/Documents/parley-recordings/` (or the folder you choose)
- **Transcripts, summaries, speakers, action items and settings**: a SQLite database in `~/.local/share/io.github.hankanman.Parley/`
- **Voice profiles** used for speaker recognition: the same database
- **Downloaded models**: `~/.local/share/io.github.hankanman.Parley/models/`

Transcription (Whisper, GigaAM and Parakeet), voice activity detection, speaker diarization and the built-in summary models all run on your device.

## When Parley uses the network

Parley only makes network connections for the following, and only when you trigger them:

| What | Where it connects | What is sent |
|---|---|---|
| Downloading a transcription, summary, semantic search, diarization or VAD model | Hugging Face (`huggingface.co`) or GitHub release downloads (`github.com`) | An ordinary file download request. No meeting data. |
| Generating a summary or chapters, answering an "Ask" question, extracting action items (including live action items while recording), drafting a follow-up or preparing a meeting brief **with a remote LLM provider you configured** | The provider you chose: a remote Ollama server, Anthropic, Groq, OpenRouter, OpenAI, or a custom OpenAI-compatible endpoint | The transcript text and prompt needed for that request, plus your API key. That provider's own privacy policy then applies. |
| Listing the models a provider offers, when Settings → Summary shows that provider | The provider you chose (OpenRouter's list needs no key; the others get your API key) | A request for the model list. No meeting data. |
| Calendar sync | The calendar feed URLs you add | A request for the feed. No meeting data. |
| "Check for updates" in the tray menu | GitHub's API (`api.github.com`) | A request for the latest release. No meeting data. |

If you use the built-in summary model or a local Ollama server, transcript text never leaves your machine.

The optional `parley-mcp` server gives an AI client you connect (such as Claude Desktop or Claude Code) access to your meetings. Parley itself sends nothing, but whatever that client reads is then handled under its own privacy policy. Its `semantic_search` tool runs on your computer; only if Ollama is your summary provider does it send the search text to that Ollama server, as Ask Parley does.

## Your control

- **Access and export**: all your data is in ordinary files and a SQLite database you can open, copy or export from the app.
- **Delete**: deleting a meeting in Parley removes its transcript, summary and related data from the database; its audio stays in the recordings folder until you delete it there. To remove everything, delete the folders listed above.
- **Audit**: Parley is open source under the MIT license, so every network call can be checked in the source code.

## Changes to this policy

Changes are made in this file in the [Parley repository](https://github.com/Hankanman/Parley) and noted in release notes.

## Contact

For privacy questions or concerns, open an issue at [github.com/Hankanman/Parley/issues](https://github.com/Hankanman/Parley/issues).
