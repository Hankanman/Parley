//! "Export folder" sync: after each summary, write a Markdown note with YAML
//! front-matter into a user-chosen folder — typically an Obsidian vault
//! subfolder, or any folder a notes tool imports from.
//!
//! One note per meeting, named `YYYY-MM-DD Title.md`. The note is found
//! again on re-summary by the `parley_meeting_id` in its front-matter, not
//! by filename, so a renamed meeting updates (and renames) its own note.
//! A file this app didn't write for this meeting is never overwritten: a
//! name collision with another meeting's note, or with a note the user
//! wrote by hand, gets a ` (2)`, ` (3)`… suffix instead.
//!
//! Re-summarising replaces the note's contents; edits made to it in the
//! notes app are not merged back.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::{info, warn};

use super::ExportedMeeting;
use crate::database::repositories::setting::SettingsRepository;
use crate::utils::format_timestamp;

/// `app_settings` key holding [`ExportFolderSettings`] as JSON.
pub const SETTINGS_KEY: &str = "export_folder_sync";

/// Front-matter key that ties a note to its meeting.
const ID_KEY: &str = "parley_meeting_id";
/// How much of each existing `.md` file is read looking for front-matter.
const FRONT_MATTER_SCAN_BYTES: u64 = 16 * 1024;
const MAX_TITLE_CHARS: usize = 100;
const MAX_COLLISION_SUFFIX: u32 = 999;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportFolderSettings {
    /// Write a note after each summary.
    pub enabled: bool,
    /// Absolute path of the folder notes are written into.
    pub folder: Option<String>,
}

impl ExportFolderSettings {
    /// The folder, when sync is on and a folder is set.
    pub fn active_folder(&self) -> Option<PathBuf> {
        if !self.enabled {
            return None;
        }
        self.folder
            .as_deref()
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .map(PathBuf::from)
    }
}

pub async fn load_settings(pool: &SqlitePool) -> ExportFolderSettings {
    match SettingsRepository::get_setting::<ExportFolderSettings>(pool, SETTINGS_KEY).await {
        Ok(settings) => settings.unwrap_or_default(),
        Err(e) => {
            warn!("export folder: failed to read settings: {e}");
            ExportFolderSettings::default()
        }
    }
}

pub async fn save_settings(
    pool: &SqlitePool,
    settings: &ExportFolderSettings,
) -> Result<(), String> {
    SettingsRepository::set_setting(pool, SETTINGS_KEY, settings)
        .await
        .map_err(|e| format!("Failed to save export folder settings: {e}"))
}

/// Hook for the summary pipeline: sync the meeting's note if folder sync is
/// on. Best-effort — logs and returns; a failure here must never affect the
/// summary, which is already saved.
pub async fn sync_after_summary(pool: &SqlitePool, meeting_id: &str) {
    match sync_meeting_note(pool, meeting_id).await {
        Ok(Some(path)) => info!(
            "export folder: wrote note for {meeting_id} to {}",
            path.display()
        ),
        Ok(None) => {}
        Err(e) => warn!("export folder: failed to write note for {meeting_id}: {e}"),
    }
}

/// Write (or update) the meeting's note in the export folder. `Ok(None)`
/// when folder sync is off or no folder is set.
pub async fn sync_meeting_note(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Option<PathBuf>, String> {
    let Some(folder) = load_settings(pool).await.active_folder() else {
        return Ok(None);
    };
    let data = super::collect(pool, meeting_id).await?;
    let note = render_note(&data);
    let filename = note_filename(&local_date(&data.created_at), &data.title);
    let id = data.id.clone();
    tokio::task::spawn_blocking(move || write_note(&folder, &id, &filename, &note))
        .await
        .map_err(|e| format!("Export folder task failed: {e}"))?
        .map(Some)
}

/// The full note: front-matter, title heading, then the same body as the
/// markdown export (summary, action items, notes, transcript).
fn render_note(m: &ExportedMeeting) -> String {
    let mut out = front_matter(m);
    let title = if m.title.trim().is_empty() {
        "Untitled meeting"
    } else {
        m.title.trim()
    };
    out.push_str(&format!("\n# {title}\n"));
    out.push_str(&super::render_markdown_body(m));
    out
}

fn front_matter(m: &ExportedMeeting) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_quote(m.title.trim())));
    out.push_str(&format!("date: {}\n", local_date(&m.created_at)));
    out.push_str(&format!("created: {}\n", yaml_quote(&m.created_at)));
    if let Some(d) = m.duration_seconds {
        out.push_str(&format!("duration: {}\n", yaml_quote(&format_timestamp(d))));
    }
    // Invited attendees when the meeting is linked to a calendar event;
    // otherwise the speakers actually heard.
    let mut attendees = super::attendee_names(m);
    if attendees.is_empty() {
        attendees = m.speakers.clone();
    }
    if attendees.is_empty() {
        out.push_str("attendees: []\n");
    } else {
        out.push_str("attendees:\n");
        for a in &attendees {
            out.push_str(&format!("  - {}\n", yaml_quote(a)));
        }
    }
    out.push_str("tags:\n  - parley\n  - meeting\n");
    out.push_str(&format!("{ID_KEY}: {}\n", yaml_quote(&m.id)));
    out.push_str("---\n");
    out
}

/// A YAML double-quoted scalar. Escapes everything YAML 1.1/1.2 parsers
/// disagree on or reject (control/C1 characters, line separators, BOM), so
/// a title or attendee name from a calendar or a model can't break out of
/// its value.
pub(crate) fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FEFF}') => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `created_at` as a local `YYYY-MM-DD`; the raw date prefix if unparsable.
fn local_date(created_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|_| created_at.chars().take(10).collect())
}

/// `YYYY-MM-DD Title.md`, with the title made safe as a filename and as an
/// Obsidian link target: path separators, characters Windows/Android sync
/// targets reject, and Obsidian's link syntax (`#^[]|`) become spaces;
/// whitespace collapses; leading dots go (no hidden files); length is
/// capped.
pub(crate) fn note_filename(date: &str, title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '#' | '^' | '[' | ']' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut title = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    title = title
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_string();
    if title.chars().count() > MAX_TITLE_CHARS {
        title = title
            .chars()
            .take(MAX_TITLE_CHARS)
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    if title.is_empty() {
        title = "Meeting".to_string();
    }
    let date: String = date
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    if date.is_empty() {
        format!("{title}.md")
    } else {
        format!("{date} {title}.md")
    }
}

/// The meeting id in a note's front-matter, if it has one.
fn note_meeting_id(content: &str) -> Option<String> {
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let mut lines = content.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    for line in lines {
        let line = line.trim_end();
        if line == "---" || line == "..." {
            break;
        }
        if let Some(value) = line
            .strip_prefix(ID_KEY)
            .and_then(|r| r.trim_start().strip_prefix(':'))
        {
            let value = value.trim();
            let id = if value.starts_with('"') {
                serde_json::from_str::<String>(value).ok()?
            } else {
                value.trim_matches('\'').to_string()
            };
            return Some(id).filter(|id| !id.is_empty());
        }
    }
    None
}

fn read_meeting_id(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(FRONT_MATTER_SCAN_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    note_meeting_id(&String::from_utf8_lossy(&head))
}

/// The note already written for `meeting_id` in `folder`, if any.
fn find_note(folder: &Path, meeting_id: &str) -> Result<Option<PathBuf>, String> {
    let entries =
        fs::read_dir(folder).map_err(|e| format!("Can't read {}: {e}", folder.display()))?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md") && p.is_file())
        .filter(|p| read_meeting_id(p).as_deref() == Some(meeting_id))
        .collect();
    // Deterministic if a user duplicated the note: update the first by name.
    matches.sort();
    Ok(matches.into_iter().next())
}

/// Write `content` as the note for `meeting_id`, never touching a file that
/// belongs to anything else. Returns the note's path.
pub(crate) fn write_note(
    folder: &Path,
    meeting_id: &str,
    filename: &str,
    content: &str,
) -> Result<PathBuf, String> {
    if !folder.is_absolute() {
        return Err(format!(
            "Export folder must be an absolute path: {}",
            folder.display()
        ));
    }
    if !folder.is_dir() {
        // Not created on our behalf: a missing folder usually means an
        // unmounted drive or a moved vault, and a fresh empty folder in its
        // place would hide that.
        return Err(format!(
            "Export folder does not exist: {}",
            folder.display()
        ));
    }

    if let Some(existing) = find_note(folder, meeting_id)? {
        let mut target = existing.clone();
        // Follow a title change, but only into a free name.
        let desired = folder.join(filename);
        if desired != existing && !desired.exists() {
            match link_no_clobber(&existing, &desired) {
                Ok(()) => {
                    let _ = fs::remove_file(&existing);
                    target = desired;
                }
                Err(e) => warn!(
                    "export folder: keeping {} (rename failed: {e})",
                    existing.display()
                ),
            }
        }
        replace_atomically(folder, &target, content)?;
        return Ok(target);
    }

    let (stem, ext) = filename.rsplit_once('.').unwrap_or((filename, "md"));
    for n in 1..=MAX_COLLISION_SUFFIX {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem} ({n}).{ext}")
        };
        let path = folder.join(name);
        // `create_new` fails if anything is already there, so a note that
        // isn't ours is never opened for writing, even in a race.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(content.as_bytes())
                    .and_then(|_| file.sync_all())
                    .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create {}: {e}", path.display())),
        }
    }
    Err(format!(
        "Too many notes named like {filename} in {}",
        folder.display()
    ))
}

/// Rename without ever replacing an existing file: hard-link then unlink
/// (the link fails if the target exists). Falls back to a checked rename on
/// filesystems without hard links.
fn link_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // (After a rename the caller's removal of `from` is a no-op.)
        Err(_) if !to.exists() => fs::rename(from, to),
        Err(e) => Err(e),
    }
}

/// Replace our own note via write-to-temp + rename, so a crash or a sync
/// tool reading mid-write never sees a half-written note.
fn replace_atomically(folder: &Path, target: &Path, content: &str) -> Result<(), String> {
    let tmp = folder.join(format!(".parley-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("Failed to update {}: {e}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::tests::sample;

    #[test]
    fn front_matter_has_every_field_and_parses_back() {
        let m = sample();
        let fm = front_matter(&m);
        assert!(fm.starts_with("---\ntitle: \"Weekly Sync\"\n"));
        assert!(fm.contains("\ndate: "));
        assert!(fm.contains("\nduration: \"00:02:05\"\n"));
        assert!(fm.contains("attendees:\n  - \"Me\"\n  - \"Speaker 1\"\n"));
        assert!(fm.contains("tags:\n  - parley\n  - meeting\n"));
        assert!(fm.ends_with("parley_meeting_id: \"meeting-1\"\n---\n"));
        assert_eq!(
            note_meeting_id(&render_note(&m)).as_deref(),
            Some("meeting-1")
        );
    }

    #[test]
    fn yaml_quote_escapes_breakouts() {
        assert_eq!(yaml_quote("plain"), "\"plain\"");
        assert_eq!(yaml_quote("a \"q\" \\ b"), "\"a \\\"q\\\" \\\\ b\"");
        assert_eq!(yaml_quote("line\nnext: x"), "\"line\\nnext: x\"");
        assert_eq!(
            yaml_quote("bell\u{7}\u{85}\u{2028}"),
            "\"bell\\u0007\\u0085\\u2028\""
        );
        assert_eq!(yaml_quote("café: 会议 #tag"), "\"café: 会议 #tag\"");
    }

    #[test]
    fn note_meeting_id_reads_quoted_and_bare_values_only_in_front_matter() {
        assert_eq!(
            note_meeting_id("---\nparley_meeting_id: \"m-1\"\n---\n").as_deref(),
            Some("m-1")
        );
        assert_eq!(
            note_meeting_id("---\r\nparley_meeting_id: m-2\r\n---\r\n").as_deref(),
            Some("m-2")
        );
        assert_eq!(note_meeting_id("# Note\nparley_meeting_id: m-3\n"), None);
        assert_eq!(
            note_meeting_id("---\ntitle: x\n---\nparley_meeting_id: m-4\n"),
            None
        );
    }

    #[test]
    fn filename_is_sanitised() {
        assert_eq!(
            note_filename("2026-07-15", "Weekly Sync"),
            "2026-07-15 Weekly Sync.md"
        );
        assert_eq!(
            note_filename("2026-07-15", "Q3/Q4: budget? [draft] #1 | \"final\""),
            "2026-07-15 Q3 Q4 budget draft 1 final.md"
        );
        assert_eq!(
            note_filename("2026-07-15", "../../etc/passwd"),
            "2026-07-15 etc passwd.md"
        );
        assert_eq!(
            note_filename("2026-07-15", "...hidden"),
            "2026-07-15 hidden.md"
        );
        assert_eq!(
            note_filename("2026-07-15", "  \t "),
            "2026-07-15 Meeting.md"
        );
        assert_eq!(
            note_filename("2026-07-15", "tab\tnew\nline"),
            "2026-07-15 tab new line.md"
        );
        let long = note_filename("2026-07-15", &"x".repeat(500));
        assert_eq!(
            long.chars().count(),
            "2026-07-15 ".len() + MAX_TITLE_CHARS + ".md".len()
        );
        assert_eq!(
            note_filename("2026-07-15", "会议记录"),
            "2026-07-15 会议记录.md"
        );
    }

    fn note_for(id: &str, body: &str) -> String {
        format!("---\nparley_meeting_id: {}\n---\n{body}\n", yaml_quote(id))
    }

    #[test]
    fn never_overwrites_another_meetings_note_or_a_hand_written_one() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let name = "2026-07-15 Weekly Sync.md";
        fs::write(folder.join(name), note_for("other-meeting", "theirs")).unwrap();
        fs::write(
            folder.join("2026-07-15 Weekly Sync (2).md"),
            "# my own note, no front-matter\n",
        )
        .unwrap();

        let path = write_note(folder, "meeting-1", name, &note_for("meeting-1", "v1")).unwrap();
        assert_eq!(path, folder.join("2026-07-15 Weekly Sync (3).md"));
        assert!(fs::read_to_string(folder.join(name))
            .unwrap()
            .contains("theirs"));
        assert_eq!(
            fs::read_to_string(folder.join("2026-07-15 Weekly Sync (2).md")).unwrap(),
            "# my own note, no front-matter\n"
        );

        // Re-summary: the same file is updated in place, no new file.
        let again = write_note(folder, "meeting-1", name, &note_for("meeting-1", "v2")).unwrap();
        assert_eq!(again, path);
        assert!(fs::read_to_string(&path).unwrap().contains("v2"));
        assert_eq!(
            fs::read_dir(folder).unwrap().count(),
            3,
            "no stray temp files"
        );
    }

    #[test]
    fn retitled_meeting_renames_its_own_note_into_a_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        let first = write_note(
            folder,
            "m-1",
            "2026-07-15 Meeting.md",
            &note_for("m-1", "v1"),
        )
        .unwrap();
        let second = write_note(
            folder,
            "m-1",
            "2026-07-15 Budget Review.md",
            &note_for("m-1", "v2"),
        )
        .unwrap();
        assert_eq!(second, folder.join("2026-07-15 Budget Review.md"));
        assert!(!first.exists());
        assert!(fs::read_to_string(&second).unwrap().contains("v2"));

        // A retitle into a name another note holds keeps the current file.
        fs::write(folder.join("2026-07-15 Taken.md"), "someone else's").unwrap();
        let third =
            write_note(folder, "m-1", "2026-07-15 Taken.md", &note_for("m-1", "v3")).unwrap();
        assert_eq!(third, second);
        assert_eq!(
            fs::read_to_string(folder.join("2026-07-15 Taken.md")).unwrap(),
            "someone else's"
        );
    }

    #[test]
    fn missing_or_relative_folder_is_an_error_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("vault");
        assert!(write_note(&missing, "m", "a.md", "x").is_err());
        assert!(!missing.exists());
        assert!(write_note(Path::new("relative"), "m", "a.md", "x").is_err());
    }

    #[test]
    fn settings_only_active_when_enabled_with_a_folder() {
        let mut s = ExportFolderSettings {
            enabled: true,
            folder: Some("  ".into()),
        };
        assert_eq!(s.active_folder(), None);
        s.folder = Some("/vault/Meetings".into());
        assert_eq!(s.active_folder(), Some(PathBuf::from("/vault/Meetings")));
        s.enabled = false;
        assert_eq!(s.active_folder(), None);
    }
}
