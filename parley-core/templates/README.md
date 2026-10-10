# Meeting Summary Templates

This directory contains template definitions for meeting summary generation.

## Available Templates

### 1. `daily_standup.json`
Time-boxed daily updates template designed for engineering/product teams.

**Sections:**
- Date
- Attendees
- Yesterday (completed work)
- Today (planned work)
- Blockers
- Notes

### 2. `standard_meeting.json`
General-purpose meeting notes template focusing on key outcomes and actions.

**Sections:**
- Summary
- Key Decisions
- Action Items
- Discussion Highlights

## Template Structure

Each template JSON file follows this schema:

```json
{
  "name": "Template Name",
  "description": "Brief description of the template's purpose",
  "sections": [
    {
      "title": "Section Title",
      "instruction": "Instructions for the LLM on what to extract/include",
      "format": "paragraph|list|string",
      "item_format": "Optional: Markdown table format for list items"
    }
  ]
}
```

## Custom Templates

Users can add custom templates to the application data directory:

- **macOS**: `~/Library/Application Support/Parley/templates/`
- **Windows**: `%APPDATA%\Parley\templates\`
- **Linux**: `~/.config/Parley/templates/`

Custom templates override built-in templates with the same filename.

## Template Fields

### Root Level
- `name` (required): Display name for the template
- `description` (required): Brief explanation of the template's use case
- `sections` (required): Array of section definitions

### Section Object
- `title` (required): Section heading text
- `instruction` (required): LLM guidance for this section
- `format` (required): One of `"paragraph"`, `"list"`, or `"string"`
- `item_format` (optional): Markdown formatting hint for list items (e.g., table structure)
- `example_item_format` (optional): Alternative formatting hint

## Usage in Code

Templates are loaded using the `templates` module:

```rust
use crate::summary::templates;

// Get a specific template
let template = templates::get_template("daily_standup")?;

// List available templates
let available = templates::list_templates();

// Validate custom template JSON
let custom_json = std::fs::read_to_string("custom.json")?;
let validated = templates::validate_template(&custom_json)?;
```

## Structured (JSON) Output

Summaries are generated as structured JSON first and rendered back to the
Markdown layout below (`# Title`, then one `**Section**` block per template
section). Each template gets its **own JSON schema, derived automatically
from its sections** — nothing extra to write, custom templates included:

- `title` and `overview` are always present. A paragraph/string section
  whose title contains "summary" or "overview" is filled from `overview`;
  otherwise the overview renders as an untitled lead paragraph.
- Every other section becomes a property keyed by a slug of its title
  (`"Top Risks & Mitigations"` → `top_risks_mitigations`): a string for
  `paragraph`/`string`, an array of strings for `list`. A table-style
  `item_format` becomes per-item guidance ("Each item should cover: Owner,
  Blocker, Impact"); list items render as bullets.
- List sections about decisions, action items / next steps, or open
  questions are filled from the core `decisions`, `action_items`
  (`{text, owner, due}`) and `open_questions` fields. Those core fields are
  always requested; when the template has no section for them they're
  appended as **Key Decisions** / **Action Items** / **Open Questions**, only
  if non-empty.
- When the meeting has notes, a **Your Notes** section lists each note
  verbatim with what the transcript says about it.

If a provider can't produce valid JSON (after one retry), the summary falls
back to the free-form Markdown prompt built from the same template. See
`parley-core/src/summary/structured.rs`.
