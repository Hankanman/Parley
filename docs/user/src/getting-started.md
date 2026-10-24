# First run

The first time you open Parley, a short setup walks you through three steps.

1. **Welcome.** Click **Get started**.
2. **Setting things up.** Parley creates its local database. Click **Continue** once it says *Ready*.
3. **Getting things ready.** Parley downloads two models:
   - the **transcription engine**, Whisper large-v3-turbo (about 547 MB), and
   - the **summary engine**, Gemma 3 1B, the built-in summary model (about 1 GB).

   You can click **Continue** as soon as the transcription engine has finished. The summary model keeps downloading in the background, and summaries become available when it's done.

   Shortly afterwards Parley also fetches two small models in the background: one that recognises speakers (about 28 MB) and one for [semantic search](ask.md#keyword-and-semantic-search) (about 146 MB).

That's it: Parley opens on the **Record** page, ready for your first meeting. Nothing you record leaves your computer unless you later choose a cloud AI provider.

> [!TIP]
> The models come from Hugging Face. If a download fails, check your connection and try again; you can also manage models later under [Settings → Transcription and Summary](models.md).

## Finding your way around

The sidebar on the left holds everything:

- **Search meetings…** filters your meetings by title as you type, and after a moment also searches inside transcripts (shown under *In transcripts*; click one to open the meeting at that line).
- **Meetings**
  - **Record**: start and control a recording. See [Recording a meeting](recording.md).
  - **Ask Parley**: ask questions across all your meetings. See [Ask Parley](ask.md).
  - **Import audio**: turn an existing audio file into a meeting. See [Importing audio](import.md).
  - **Settings**
- **Tools**
  - **Action items**: every action item from every meeting. See [Action items](action-items.md).
  - **Speakers**: the people Parley has learned to recognise. See [Speakers](speakers.md).
- **Upcoming**: your next calendar events, once you've [added a calendar](calendar.md).
- Your meetings, grouped into *Today*, *Yesterday*, *This week* and *Older*. Click one to open it; see [Meeting notes](meetings.md).

Parley also puts an icon in your system tray. See [The tray icon](recording.md#the-tray-icon).
