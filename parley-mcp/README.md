# parley-mcp

An [MCP](https://modelcontextprotocol.io) server that exposes your Parley
meeting history — meetings, transcripts, AI-generated summaries, action items,
and notes — to an external AI agent like Claude Desktop or Claude Code.

It reads the app's SQLite database directly, so **it works whether or not
Parley is running**, and it's safe to use while a meeting is being recorded.

Everything stays on your machine: this is a local process reading a local file.
Nothing is sent anywhere except to the MCP client that spawned it.

## Build

```bash
cargo build --release -p parley-mcp
# -> target/release/parley-mcp
```

`./build.sh` at the repo root builds it too, alongside the app.

## Register it with a client

The client spawns the binary and talks to it over stdin/stdout — you never run
it by hand. Use an **absolute path**; MCP clients don't expand `~` or inherit
your shell's `PATH`.

### Claude Code

```bash
claude mcp add parley -- /absolute/path/to/parley/target/release/parley-mcp
```

Or add it to `.mcp.json` in a project:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/absolute/path/to/parley/target/release/parley-mcp"
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
      "command": "/absolute/path/to/parley/target/release/parley-mcp"
    }
  }
}
```

To point at a database somewhere other than the default:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/absolute/path/to/parley/target/release/parley-mcp",
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
| `list_meetings` | `limit?` (default 50, max 500), `since?`/`until?` (`YYYY-MM-DD`, UTC), `query?` (title substring) | id, title, created_at, has_summary, open_action_item_count |
| `get_meeting` | `meeting_id` | metadata, summary markdown, summary status, transcript/action-item/note counts |
| `get_transcript` | `meeting_id`, `include_speakers?` (default true) | rendered text plus, when attributed, per-segment speaker + timestamps |
| `search_transcripts` | `query`, `limit?` (default 50, max 500) | matching segments across all meetings with meeting title, snippet, speaker |
| `get_action_items` | `meeting_id?`, `status?` (`open`/`done`) | action items, open first |
| `get_recent_summaries` | `limit?` (default 5, max 500) | recent meetings' summary markdown |

### Write

Anything created is tagged `source = "agent"`, so you can tell the agent's
entries apart from your own in the app.

| Tool | Arguments | Effect |
|---|---|---|
| `add_meeting_note` | `meeting_id`, `body` | inserts a note |
| `create_action_item` | `meeting_id`, `text`, `assignee?`, `due_hint?` | inserts an open action item |
| `set_action_item_status` | `action_item_id`, `status` (`open`/`done`) | updates status; maintains the completion timestamp |
| `update_summary` | `meeting_id`, `markdown` | replaces the summary text |

`update_summary` **overwrites** the whole summary — it doesn't append. It
leaves the generation status untouched, and if the app happens to be generating
a summary for that meeting right now, the response says so, because the app
will overwrite what was just written when it finishes.

### A note on search

`search_transcripts` is a literal, case-insensitive **substring** match — not
fuzzy, not semantic. `"budget"` won't match `"budgets"`, and a multi-word query
matches only an exact contiguous phrase. Short single keywords work best. `%`
and `_` are matched literally, not as wildcards.

## Resources

Each meeting is also readable as a resource at `parley://meeting/{id}`,
returning summary + full attributed transcript as one markdown document —
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
