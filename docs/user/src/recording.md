# Recording a meeting

Parley records two things at once: your **microphone** and your computer's **system audio** (whatever comes out of your speakers or headphones). That captures both sides of any call, in Zoom, Teams, Meet, Slack, Discord or anything else, without a bot joining the meeting.

![The Record page](/images/record.png)

## Starting a recording

1. Open **Record** in the sidebar.
2. Optionally give the meeting a title. Otherwise it is called *Meeting* plus the date and time.
3. Click **Start recording**.

> [!IMPORTANT]
> Recording people without their knowledge may be illegal where you live. Let everyone in the meeting know you are recording.

While recording you can **Pause**, **Resume** and **Stop**. The timer doesn't count paused time.

If your [calendar](calendar.md) has an event happening now (from 5 minutes before it starts until 15 minutes after it ends), Parley names the meeting after it and links the two. A message tells you when this happens.

## Choosing devices and checking levels

Click **Audio** to open the device panel. Choose a **Microphone** and a **System audio** source, and watch the **Mic** and **System** level bars to check that sound is coming through. Your choice is remembered for next time. You can only change devices while not recording.

The panel is closed by default because the level meters keep your microphone open, which lights up your desktop's "microphone in use" indicator.

Default devices can also be set under **Settings → Recording → Devices**.

## The live transcript

Speech appears in the transcript a few seconds after it is spoken. Lines from your microphone are labelled **Me** and lines from system audio **Speaker**, until Parley works out who is speaking. Text that is still being transcribed is shown in italics underneath.

The transcription runs on your computer with Whisper. Your microphone gets echo cancellation, noise suppression and automatic gain, so you don't need headphones. The microphone and system audio are kept on separate channels in the recording.

## Bookmarks

Mark an important moment with the **Bookmark** button, the tray menu's **Bookmark this moment**, or <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>B</kbd> while a Parley window is focused. Bookmarks show as time chips while recording, and afterwards in the meeting's [Chapters strip](meetings.md#chapters-and-bookmarks). The summary gives bookmarked moments extra weight.

## Live action items (beta)

Turn on **Settings → Recording → Live action items** to have Parley pick out action items while the meeting is still going. They appear in an *Action items* card on the Record page.

## When you stop

Parley opens the new meeting straight away, and finishes up in the background:

1. It works out **who spoke when** (see [Speakers](speakers.md)).
2. If you have downloaded a larger Whisper model (large-v3), it **re-transcribes** the meeting with it for better accuracy. While this runs, the Record page says *Refining the last meeting…*.
3. If **Settings → Summary → Auto-summarize after recording** is on, it writes the **summary** and **chapters**. Otherwise you generate a summary yourself from the meeting page.

## Automatic call detection

Parley can notice when a call starts. It recognises Microsoft Teams, Zoom, Slack, Discord, Webex and Jitsi Meet, and web calls in Chrome, Chromium, Firefox, Brave and Edge, by watching which applications are using your microphone.

Choose what happens under **Settings → Recording → Meeting detection → When a call starts**:

- **Do nothing.**
- **Notify me** (the default). A notification asks whether to record, with **Record** and **Open Parley** buttons. The tray menu also offers **Record … call**.
- **Record automatically.** Recording starts by itself and a notification reminds you to tell the others.

Turn on **Stop recording when the call ends** to stop automatically about 8 seconds after the call's microphone use ends.

> [!NOTE]
> A browser using your microphone for anything counts as a web call.

## The tray icon

Parley's tray icon turns into a red dot while recording. Its menu shows the current status and lets you:

- **Start**, **Pause**, **Resume** or **Stop** recording, and **Bookmark this moment**. Starting from the tray uses your default devices.
- **Open Parley**, open **Settings** or **Check for updates**.
- **Quit**.

Closing the Parley window doesn't stop a recording: the window hides and Parley keeps running in the tray. Use **Open Parley** to bring it back. If you quit while recording, Parley stops and saves the recording first.

## If Parley closes unexpectedly

Audio is saved in 30-second pieces while you record. If Parley crashes or your computer loses power mid-meeting, the next launch shows *interrupted meetings found*. For each one you can **Preview** the transcript captured so far, **Recover** it (the audio is stitched back together), or **Delete** it.

## Where recordings are saved

Each meeting gets its own folder in `~/Documents/parley-recordings/`, containing `audio.mp4` (AAC audio, microphone and system audio on separate channels) plus the transcript as JSON. **Settings → Recording → Open folder** opens it.

If you turn off **Auto-save recordings**, Parley keeps the transcript but not the audio, so you won't be able to re-transcribe those meetings later.
