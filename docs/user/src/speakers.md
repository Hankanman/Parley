# Speakers

Parley works out who is speaking ("diarization") on your computer and labels the transcript *Speaker 1*, *Speaker 2* and so on. Name a voice once and Parley recognises that person in later meetings.

![The Speakers page](/images/speakers.png)

Speaker detection needs a small speaker model, which Parley downloads automatically. After a recording stops, Parley takes a second look at the whole meeting to tidy up the labels.

## Naming a speaker

Click a speaker's name in a transcript.

1. Choose whether to rename **all** of that speaker's lines or **only this line** (useful when one label really covers two people).
2. Type their **name**, and optionally their email. If the meeting is linked to a [calendar event](calendar.md), its attendees are suggested.
3. Save.

To give the lines to someone Parley already knows, use the **target** button to pick their profile instead of *Create new speaker…*. This merges the two, and relabels every line in the meeting.

To undo a wrong name, **unassign** the lines: they go back to an unnamed speaker.

> [!NOTE]
> Parley learns a new voice when you name a speaker **in a meeting you've just recorded** (or imported or re-transcribed in the same session). Naming speakers in older meetings renames their lines but doesn't save a voice. Naming a single line never changes a saved voice.

## Your own voice

On the **Speakers** page, under **Your voice**, click **Record my voice** and read the text aloud for about 20 seconds. Parley then labels you by name in every meeting, including on system audio.

## Managing speakers

The **Speakers** page lists every saved voice. For each one you can:

- **Rename** it.
- **Relearn** their voice from all the lines attributed to them, if recognition has been unreliable.
- **Merge into…** another speaker, if the same person was saved twice. Parley flags profiles with the same name.
- **Delete** it. Past transcripts keep the name, but future meetings stop recognising the voice.

## How recognition works

Voices are matched against saved profiles automatically. When a meeting is linked to a calendar event, people on the invite are matched more readily than people who aren't, and the number of attendees helps Parley decide how many speakers there are. Voice profiles are stored only in Parley's local database.
