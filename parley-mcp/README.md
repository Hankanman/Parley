# parley-mcp

An [MCP](https://modelcontextprotocol.io) server that exposes your Parley
meeting history — meetings, transcripts, AI-generated summaries, action items,
notes, chapters and bookmarks, the people Parley recognises, your calendar,
pre-meeting briefs and follow-up drafts — to an external AI agent like Claude
Desktop or Claude Code.

It reads the app's SQLite database directly, so **it works whether or not
Parley is running**, and it's safe to use while a meeting is being recorded.

Everything stays on your machine: this is a local process reading a local file.
Nothing is sent anywhere except to the MCP client that spawned it (and, for
`semantic_search` when Ollama is Parley's summary provider, the search text
to that Ollama server).

## Get it

The Parley AppImage includes it. The installer
(`curl -fsSL https://meetparley.app/install.sh | sh`) puts it at
`~/.local/bin/parley-mcp`; if you downloaded the AppImage yourself, use
**Install parley-mcp** in Parley's Settings → Integrations, which copies it
to the same place. (Clients can't use the copy inside the AppImage
directly: its path changes every launch.)

From source:

```bash
cargo build --release -p parley-mcp
# -> target/release/parley-mcp
```

`./build.sh` at the repo root builds it too, and bundles it into the
AppImage.

## Register it with a client

The client spawns the binary and talks to it over stdin/stdout — you never run
it by hand. Use an **absolute path**; MCP clients don't expand `~` or inherit
your shell's `PATH`.

### Claude Code

```bash
claude mcp add parley -- /home/you/.local/bin/parley-mcp
```

Or add it to `.mcp.json` in a project:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/home/you/.local/bin/parley-mcp"
    }
  }
}
```

### Claude Desktop

Edit `claude_desktop_config.json` — on Linux
`~/.config/Claude/claude_desktop_config.json`, on macOS
`~/Library/Application Support/Claude/claude_desktop_config.json` — then
restart Claude Desktop:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/home/you/.local/bin/parley-mcp"
    }
  }
}
```

To point at a database somewhere other than the default:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/home/you/.local/bin/parley-mcp",
      "args": ["--db", "/path/to/meeting_minutes.sqlite"]
    }
  }
}
```

`{"env": {"PARLEY_DB_PATH": "/path/to/meeting_minutes.sqlite"}}` works too.

Then ask Claude things like *"what did we decide about the Q3 budget?"*,
*"summarize my meetings from last week"*, or *"what action items do I still
have open?"*.

## Database

Resolved in this order — first match wins:

1. `--db <path>`
2. `$PARLEY_DB_PATH`
3. `<data-dir>/io.github.hankanman.Parley/meeting_minutes.sqlite`
   (Linux: `~/.local/share/io.github.hankanman.Parley/meeting_minutes.sqlite`)

The database **must already exist** — the server never creates or migrates it,
because the Parley app owns the schema. If it's missing, or predates the
tables this server needs, startup fails with a message telling you which. Run
Parley once first.

### Running alongside Parley

The app may have the same file open, so the server opens it in **WAL mode**
(readers and the writer don't block each other) with a **5-second busy
timeout**, and retries writes that hit lock contention. Reads can't stall a
recording in progress, and writes wait their turn rather than failing.

## Tools

### Read

| Tool | Arguments | Returns |
|---|---|---|
| `list_meetings` | `limit?` (default 50, max 500), `since?`/`until?` (`YYYY-MM-DD`, UTC), `query?` (title substring), `person?` (speaker id, or a name/email matched against speakers and calendar attendees), `series_id?` (a recurring calendar meeting) | id, title, created_at, has_summary, open_action_item_count |
| `get_meeting` | `meeting_id` | metadata, summary markdown and status, the linked calendar event (time, location, organizer, attendees, `series_id`), chapters, bookmarks, notes, counts, whether a follow-up draft exists |
| `get_transcript` | `meeting_id`, `include_speakers?` (default true) | rendered text plus, when attributed, per-segment speaker + timestamps |
| `get_meeting_stats` | `meeting_id` | talk-time analytics computed from the transcript (no AI): per-speaker talk share, turns, words per minute, longest monologue, questions, interruptions; duration, silence ratio, you-vs-others split |
| `search_transcripts` | `query`, `limit?` (default 50, max 500) | matching segments across all meetings with meeting title, snippet, speaker |
| `semantic_search` | `query`, `limit?` (default 10, max 50), `person?`, `since?`/`until?` | passages that match the meaning of the query, with meeting, similarity, start time and excerpt, plus how many meetings are indexed |
| `get_action_items` | `meeting_id?`, `status?` (`open`/`done`) | action items, open first |
| `get_recent_summaries` | `limit?` (default 5, max 500) | recent meetings' summary markdown |
| `list_speakers` | — | the people Parley recognises by voice: id, name, email, whether it's you, meeting count |
| `list_calendar_events` | `from?`/`to?` (`YYYY-MM-DD`; default the next 7 days), `query?`, `limit?` | events with attendees, recorded meeting ids, `series_id`, whether a brief exists |
| `get_brief` | `event_id` | the pre-meeting brief Parley prepared, with the past meetings it cites |
| `get_followup_draft` | `meeting_id` | the follow-up email drafted in Parley (subject, body, tone) |

### Write

Anything created is tagged `source = "agent"`, so you can tell the agent's
entries apart from your own in the app.

| Tool | Arguments | Effect |
|---|---|---|
| `add_meeting_note` | `meeting_id`, `body` | inserts a note |
| `create_action_item` | `meeting_id`, `text`, `assignee?`, `due_hint?` | inserts an open action item |
| `set_action_item_status` | `action_item_id`, `status` (`open`/`done`) | updates status; maintains the completion timestamp |
| `update_action_item` | `action_item_id`, `text?`, `assignee?`, `due_hint?` | edits the item; omitted fields are kept, an empty string clears owner or due hint |
| `delete_action_item` | `action_item_id` | deletes the item |
| `update_summary` | `meeting_id`, `markdown` | replaces the summary text |

`update_summary` **overwrites** the whole summary — it doesn't append. It
leaves the generation status untouched, and if the app happens to be generating
a summary for that meeting right now, the response says so, because the app
will overwrite what was just written when it finishes.

### A note on search

`search_transcripts` is ranked full-text keyword search, backed by the app's
SQLite FTS5 index. Every word in the query must appear in a segment (in any
order); matching is case- and accent-insensitive, and the last word also
matches as a prefix (`"budg"` finds `"budget"` and `"budgets"`). Hits come back
best-first (bm25), each with a snippet that has the matched words in
`**bold**`. Quotes and FTS5 operators (`AND`, `NEAR`, `*`, `:`…) in the query
are searched as plain text. It is keyword search, not semantic: synonyms don't
match.

`semantic_search` matches meaning instead, using the transcript embeddings
Parley builds for Ask Parley, and covers the meetings the app has indexed
(it does so in the background while Ask Parley is open). The query is
embedded with the same model the app used, by the app's rules:

- **Built in** (the default): nomic-embed-text v1.5, which the app
  downloads to `models/embeddings/` in its data directory, run by the app's
  bundled `llama-helper`. `parley-mcp` starts the helper itself, found in this
  order: `$PARLEY_LLAMA_HELPER`; `llama-helper` next to `parley-mcp` (a source
  build); the `parley` AppImage next to `parley-mcp` (the installer's layout),
  run as `parley --llama-helper`; the AppImage Parley last ran from (it
  records the path in its data directory).
- **Ollama**, when it is Parley's summary provider: that server, with the
  `ask_embedding_model` setting (default `nomic-embed-text`).

Against a database from an older Parley build that hasn't created the FTS5
index yet, it falls back to an unranked case-insensitive substring match.

## Resources

Each meeting is also readable as a resource at `parley://meeting/{id}`,
returning calendar details, summary, chapters, bookmarks, notes and the full
attributed transcript as one markdown document —
convenient for "ingest this meeting". `resources/list` returns the 100 most
recent meetings; use `list_meetings` to reach older ones.

## Logging

Logs go to **stderr** (stdout is the protocol channel). Set `RUST_LOG` to
adjust, e.g. `RUST_LOG=parley_mcp=debug`. Most MCP clients surface a server's
stderr in their own logs — that's the place to look when something's wrong.

## Tests

```bash
cargo test -p parley-mcp
```
