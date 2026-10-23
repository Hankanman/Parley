# Importing audio

You can turn any existing recording, such as a voice memo or a call recorded elsewhere, into a Parley meeting with a transcript, speakers and (optionally) a summary.

## Importing a file

Click **Import audio** in the sidebar, or drag an audio file onto the Parley window.

Supported formats: MP4, M4A, WAV, MP3, FLAC, OGG, AAC, MKV, WebM and WMA.

After you pick a file, Parley shows its length, size and format. Set a **Meeting Title** (it defaults to the file name), then click **Import**.

Under **Advanced options** you can set:

- **Language**: *Auto Detect*, or one of English, Chinese, German, Spanish, Russian, Korean, French, Japanese, Portuguese, Italian, Dutch, Arabic or Hindi. Choosing the language helps with short or noisy recordings.
- **Number of speakers**: *Auto-detect*, or 1 to 8 if you know how many people are talking.
- **Model**: any Whisper model you have downloaded.

A progress bar shows how far along the import is, and you can cancel at any time. When it's done, the new meeting opens.

Imports are labelled by speaker too; see [Speakers](speakers.md).

## Re-transcribing a meeting

If a transcript isn't good enough, you can run the meeting's audio through Whisper again, for example with a larger model or with the language set explicitly.

1. Open the meeting.
2. Click the **Retranscribe audio** button (the circular arrow) in the header.
3. Optionally type a **Language** code (such as `en` or `de`; leave it as `auto` to detect it), and click **Model** to cycle through your downloaded models.
4. Click **Start**.

This only works for meetings whose audio was saved; see [Where recordings are saved](recording.md#where-recordings-are-saved).
