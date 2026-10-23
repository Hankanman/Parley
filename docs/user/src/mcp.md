# Using Parley with Claude (MCP)

`parley-mcp` is a small [Model Context Protocol](https://modelcontextprotocol.io) server that lets AI assistants such as Claude Desktop and Claude Code read your Parley meetings. You can then ask things like *"what did we decide about the Q3 budget?"* or *"which action items do I still have open?"*.

It reads Parley's database directly on your computer, works whether or not Parley is running (even during a recording), and sends your data nowhere except to the assistant that started it. Bear in mind that whatever the assistant reads is then sent to its own model provider.

## Getting parley-mcp

`parley-mcp` isn't included in the AppImage yet, so for now you build it from source (you need [Rust](https://rustup.rs)):

```bash
git clone https://github.com/Hankanman/Parley.git
cd Parley
cargo build --release -p parley-mcp
```

The program is then at `target/release/parley-mcp`. Copy it somewhere permanent, such as `~/.local/bin/`.

**Settings → Integrations** shows whether Parley can find it, and gives you ready-made setup commands with the right paths filled in, each with a copy button.

## Connecting an assistant

Assistants start `parley-mcp` themselves, so give them its full path (they don't understand `~`).

**Claude Code:**

```bash
claude mcp add parley -- /home/you/.local/bin/parley-mcp
```

**Claude Desktop:** add this to `~/.config/Claude/claude_desktop_config.json` and restart Claude Desktop:

```json
{
  "mcpServers": {
    "parley": {
      "command": "/home/you/.local/bin/parley-mcp"
    }
  }
}
```

Run Parley at least once before connecting, so that its database exists.

## What the assistant can do

It can list and search your meetings, read transcripts, summaries and talk-time statistics, and list action items. It can also add notes and action items, mark action items done and replace a meeting's summary. Anything it creates is marked as coming from the agent.

The full list of tools is in the [parley-mcp README](https://github.com/Hankanman/Parley/blob/main/parley-mcp/README.md).
