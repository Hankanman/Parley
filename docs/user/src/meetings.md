# Meeting notes

Click a meeting in the sidebar to open it. The **transcript** is on the left; on the right are three tabs: **Summary**, **Ask** and **Insights**.

![A meeting with its transcript, chapters and summary](/images/meeting.png)

The buttons in the header let you:

- **Rename** the meeting (the pencil).
- **Retranscribe audio**; see [Re-transcribing a meeting](import.md#re-transcribing-a-meeting).
- **Copy meeting as Markdown** or **as JSON**, to paste elsewhere.
- **Export meeting…**; see [Exporting](#exporting).
- **Delete meeting**. This removes the transcript, summary, notes and action items from Parley. The audio file stays in your recordings folder until you delete it there.

## Transcript

Each line shows who spoke, when, and what they said.

- Click ▶ to **play** that line's audio.
- Click a **speaker name** to say who it really is; see [Speakers](speakers.md).
- **Copy transcript** copies the whole transcript with timestamps.

To find something inside transcripts, use **Search meetings…** at the top of the sidebar, which searches every meeting.

The transcript text itself can't be edited. If it's poor, try [re-transcribing](import.md#re-transcribing-a-meeting) with a larger model or a fixed language, and add tricky words to your [custom vocabulary](models.md#custom-vocabulary).

## Chapters and bookmarks

The **Chapters** strip above the transcript lets you jump around the meeting by topic. Click **Generate chapters** to have the AI split the meeting into chapters (this happens automatically if *Auto-generate chapters after recording* is on). Chapter headings also appear inside the transcript.

[Bookmarks](recording.md#bookmarks) you made while recording are listed here too. Click one to jump to it, give it a label, or delete it.

Clicking a chapter, a bookmark, a citation in Ask or the longest monologue in Insights also plays the recording from that point, for up to a minute. Click ■ on the playing line to stop it.

## Summary

Choose a template with the template button (click it to cycle through them), optionally add **Custom instructions** such as *"focus on the budget discussion"*, and click **Generate summary**. The summary streams in as it is written, and you can **Regenerate** it at any time.

Summaries always include a title and overview, plus key decisions, action items and open questions when there are any, and your notes. Parley also uses the summary's title to rename the meeting, unless you named it yourself or it came from your calendar.

**Edit summary** opens an editor so you can correct or add to it; click **Save** when done.

Summaries are written by the AI model chosen in [Settings → Summary](models.md#language-model-for-summaries-and-ask), and are saved as `summary.md` in the meeting's recordings folder too.

### Templates

Parley comes with two templates:

- **Standard Meeting Notes**: summary, key decisions and discussion highlights.
- **Daily Standup**: attendees, yesterday, today, blockers and notes.

You can add your own. Create a JSON file in `~/.local/share/io.github.hankanman.Parley/templates/`, such as `retro.json`:

```json
{
  "name": "Retrospective",
  "description": "What went well, what didn't, and what we'll change.",
  "sections": [
    { "title": "What went well", "instruction": "Things the team was happy with.", "format": "list" },
    { "title": "What didn't", "instruction": "Problems and frustrations raised.", "format": "list" },
    { "title": "Changes", "instruction": "Concrete changes the team agreed to try.", "format": "list" }
  ]
}
```

Each section's `format` is `paragraph`, `list` or `string`. Restart Parley to pick up new templates. The repository has [more examples](https://github.com/Hankanman/Parley/tree/main/parley-core/templates), including project sync, retrospective and client call templates.

### Notes

Under the summary, add your own **Notes** to a meeting. They're included the next time you generate the summary.

## Follow-up email

**Draft follow-up email** (in the Summary tab) writes a follow-up to send to the attendees, based on the summary and action items. Choose a **Tone** (*Concise*, *Formal* or *Friendly*), edit the **Subject** and **Body**, and copy them into your email app. Parley never sends email itself.

## Ask

The **Ask** tab answers questions about this meeting, such as *"What did Priya say about the launch date?"*. Answers cite the transcript with timestamps you can click to jump there and hear it. To ask across all your meetings, use [Ask Parley](ask.md).

## Insights

The **Insights** tab shows talk-time statistics worked out from the transcript (no AI involved): the meeting's length and silence, how much you spoke compared with everyone else, questions asked, interruptions, and the longest monologue. Below that, each speaker's share of talk time, number of turns and speaking pace.

## Calendar event

The header shows the [calendar event](calendar.md) a meeting is linked to, with its time, location and attendees. Click **Link calendar event** to pick one from the week around the recording, or **Change** or unlink it.

## Exporting

**Export meeting…** saves the meeting as a file:

- **Markdown**, **JSON**, **Word (DOCX)** or **PDF**: summary, action items, notes, attendees and the full transcript.
- **SRT** or **WebVTT** subtitles: the transcript only, timed to the audio.

### Export to a notes folder (Obsidian)

To keep a copy of every meeting in a notes app such as Obsidian, open **Settings → Integrations → Export folder** and **Choose…** a folder, such as your vault. After every summary (and every time you save an edited summary) Parley writes a Markdown note named `YYYY-MM-DD Title.md`, with the title, date, duration, attendees and tags in its front matter. Parley only ever updates its own notes, but changes you make to them in another app are overwritten the next time that meeting's summary changes.
