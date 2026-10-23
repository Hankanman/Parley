# Your data and privacy

Everything Parley records and produces stays on your computer. There is no account, no telemetry and no crash reporting. The full details are in the [privacy policy](https://github.com/Hankanman/Parley/blob/main/PRIVACY_POLICY.md).

## When Parley uses the network

- **Downloading models**, from Hugging Face or GitHub. No meeting data is sent.
- **Fetching your calendar feeds**, if you've [added any](calendar.md).
- **Checking for updates**, only when you click *Check for updates* in the tray menu.
- **Listing a provider's models**, when Settings → Summary shows a cloud provider or Ollama.
- **Cloud AI providers**, only if you chose one in [Settings → Summary](models.md#cloud-providers). The transcript text needed for each request is sent to that provider. With the built-in model or a local Ollama, nothing leaves your computer.

## Where everything is stored

| What | Where |
|---|---|
| Audio recordings | `~/Documents/parley-recordings/`, one folder per meeting |
| Meetings, transcripts, summaries, speakers, action items and settings (including API keys) | `~/.local/share/io.github.hankanman.Parley/meeting_minutes.sqlite` |
| Whisper and summary models | `~/.local/share/io.github.hankanman.Parley/models/` |
| Custom summary templates | `~/.local/share/io.github.hankanman.Parley/templates/` |
| Log files | `~/.local/state/parley/logs/` |

If you used Meetily before, Parley moved its data to these locations automatically on first launch.

## Backing up

Quit Parley, then copy `~/.local/share/io.github.hankanman.Parley/` (you can skip `models/`, which can be downloaded again) and `~/Documents/parley-recordings/`. To restore, put them back in the same places before starting Parley.

## Deleting your data

Deleting a meeting in Parley removes its transcript, summary and related data from the database. Its audio stays in the recordings folder until you delete it there.

To remove everything, [uninstall Parley](install.md#uninstalling) and delete the folders in the table above.
