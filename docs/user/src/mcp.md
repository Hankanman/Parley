# Using Parley with Claude (MCP)

`parley-mcp` is a small [Model Context Protocol](https://modelcontextprotocol.io) server that lets AI assistants such as Claude Desktop and Claude Code read your Parley meetings. You can then ask things like *"what did we decide about the Q3 budget?"* or *"which action items do I still have open?"*.

It reads Parley's database directly on your computer, works whether or not Parley is running (even during a recording), and sends your data nowhere except to the assistant that started it. Bear in mind that whatever the assistant reads is then sent to its own model provider.

## Getting parley-mcp

`parley-mcp` comes with Parley (version 0.5.1 and later):

- If you used the [install command](install.md#install-with-one-command), it's already installed at `~/.local/bin/parley-mcp`.
- If you downloaded the AppImage yourself, open **Settings → Integrations** and click **Install parley-mcp**, which copies it to `~/.local/bin/parley-mcp`. After updating Parley, the same button reads **Update parley-mcp**.

Assistants can't use the copy inside the AppImage directly, because its location changes every time Parley starts.

**Settings → Integrations** also gives you ready-made setup commands with the right paths filled in, each with a copy button.

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
