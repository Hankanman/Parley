# Troubleshooting

## Parley won't start

**From a terminal**, run `parley` (or the AppImage) to see any error message.

- *"fusermount: command not found"* or similar FUSE errors: install your distribution's `fuse3` package, or run the AppImage with `--appimage-extract-and-run`.
- *"GLIBC_2.39 not found"*: your distribution is too old. Parley needs Ubuntu 24.04, Debian 13, Fedora 40 or newer.

## No sound is being recorded

- Open **Audio** on the Record page and check the **Mic** and **System** level bars move when there is sound. If not, pick a different device from the lists.
- Parley needs **PipeWire**. Check with `pactl info | grep "Server Name"`, which should mention PipeWire.
- If the other people on a call aren't being transcribed, make sure **System audio** is set to the output you're actually listening on (for example your headset rather than your speakers).

## Transcription is slow or falls behind

- Use a smaller or quantised model (the ones ending in `q5_0` or `q5_1`) under [Settings → Transcription](models.md#transcription-whisper).
- If you have a GPU, make sure you installed the **Vulkan** build: reinstall with `PARLEY_VARIANT=vulkan`. The log says `Vulkan GPU support: enabled` when it is in use.

## The transcript is inaccurate

- Set the **Transcription language** under Settings → Summary instead of relying on detection.
- Add names and jargon to your [custom vocabulary](models.md#custom-vocabulary).
- [Re-transcribe](import.md#re-transcribing-a-meeting) with a larger model, or download `large-v3-q5_0` so meetings are refined automatically after recording.

## Summaries fail or are poor

- Check that a model is selected in **Settings → Summary**. With *Built-in AI*, make sure its model has finished downloading.
- For better summaries, try the **Gemma 3 4B** built-in model, a larger model in Ollama, or a cloud provider.
- With Ollama, check that it's running (`ollama list`) and that the **Endpoint** is right.

## Ask says "keyword search only"

Semantic search needs Ollama as the summary provider and the `nomic-embed-text` model; see [Ask Parley](ask.md#keyword-and-semantic-search).

## No notifications

Click **Send test notification** under Settings → Notifications. If it appears, check the **Events** toggles and that **Manual Do Not Disturb** is off. If it doesn't, check your desktop's own notification settings.

## Logs and crash reports

Parley writes a log to `~/.local/state/parley/logs/parley.log` (the previous run is kept as `parley.log.1`). For more detail, start it with:

```bash
RUST_LOG=debug parley
```

If Parley crashes, the log records it, and `coredumpctl info parley` shows the crash details on most distributions.

## Reporting a problem

[Open an issue](https://github.com/Hankanman/Parley/issues/new) with your distribution, the Parley version (Settings → About), what happened, and the relevant part of the log. Check the log for anything private, such as meeting titles, before sharing it.
