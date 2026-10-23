# Calendar and briefs

Connect your calendar and Parley will name meetings after their events, know who attended, remind you before meetings start and prepare a brief from past meetings with the same people.

## Adding a calendar

Parley reads calendars as **iCal (ICS) feeds**, so it works with Google Calendar, Outlook, Fastmail, Nextcloud and most others. To get the feed address:

- **Google Calendar**: *Settings → (your calendar) → Integrate calendar → Secret address in iCal format*.
- **Outlook / Microsoft 365**: *Settings → Calendar → Shared calendars → Publish a calendar*, then copy the ICS link.

Then in Parley, open **Settings → Calendar**, paste the URL (`https://` and `webcal://` both work), optionally give it a **Label**, and click **Add**.

Each feed shows when it was last synced. Feeds are fetched when you add them; click the refresh icon to fetch the latest events. Parley keeps events from a week ago to three months ahead.

> [!NOTE]
> A feed's secret address gives read access to your calendar, so treat it like a password. Parley stores it locally and only uses it to download the feed.

## What the calendar does

- **Upcoming meetings.** The next few events in the coming 12 hours are listed in the sidebar under *Upcoming*.
- **Meeting titles and attendees.** When you start a recording during an event (from 5 minutes before it starts to 15 minutes after it ends), the meeting is named after the event and linked to it. All-day, cancelled, declined, free-time and solo events are ignored.
- **Reminders.** A notification 15 minutes (and 5 minutes) before each event, with an **Open brief** button. Turn these off with **Settings → Notifications → Events → Meeting reminders**.
- **Speaker names.** Attendees help Parley [name speakers](speakers.md), and can be added to the [custom vocabulary](models.md#custom-vocabulary).

## Briefs

Click an event under *Upcoming*, or **Open brief** on a reminder, to see a brief for that meeting. Parley looks through past meetings in the same recurring series or with the same people, and summarises what was discussed, what was decided and what is still open, with links back to the meetings it used.

Briefs are written by your [language model](models.md#language-model-for-summaries-and-ask). Click **Regenerate** to write it again.
