//! A small, format-neutral document model shared by the DOCX and PDF
//! exporters, plus the pulldown-cmark → model conversion for the summary.
//!
//! Both rich formats render the same [`Block`] list, built once by
//! [`build_document`], so they can't drift apart in content. The model only
//! covers what meeting summaries actually use: headings, paragraphs,
//! (nested, ordered, task) lists, tables, quotes, code, rules, and
//! bold/italic/strike/code inline styling.
//!
//! The summary markdown is LLM output and is treated as untrusted text:
//! links keep their label only (the URL is dropped, so nothing in an
//! exported file points anywhere), images are dropped, and raw HTML is
//! reduced to its text.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::ExportedMeeting;
use crate::utils::format_timestamp;

/// A run of text sharing one inline style. `text` may contain `\n` for a
/// hard line break inside the block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// De-emphasised (timestamps, metadata labels).
    pub muted: bool,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            ..Default::default()
        }
    }
    pub fn bold(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            bold: true,
            ..Default::default()
        }
    }
    pub fn muted(text: impl Into<String>) -> Self {
        Span {
            text: text.into(),
            muted: true,
            ..Default::default()
        }
    }
    fn same_style(&self, other: &Span) -> bool {
        self.bold == other.bold
            && self.italic == other.italic
            && self.strike == other.strike
            && self.code == other.code
            && self.muted == other.muted
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ListMarker {
    Bullet,
    Ordered(u64),
    Task(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// The document title (one per document).
    Title(String),
    /// `level` is 1..=4 after clamping.
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    /// `depth` is 0 for a top-level item.
    ListItem {
        depth: u8,
        marker: ListMarker,
        spans: Vec<Span>,
    },
    Quote(Vec<Span>),
    Code(String),
    Rule,
    Table {
        header: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
}

/// Plain text of a span list.
#[cfg(test)]
pub fn spans_text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn push_span(spans: &mut Vec<Span>, span: Span) {
    if span.text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.same_style(&span) {
            last.text.push_str(&span.text);
            return;
        }
    }
    spans.push(span);
}

/// Build the full export document: title, metadata, summary, action items,
/// notes, transcript. Sections with no content are omitted, matching the
/// markdown export.
pub(super) fn build_document(m: &ExportedMeeting) -> Vec<Block> {
    let mut blocks = vec![Block::Title(if m.title.trim().is_empty() {
        "Untitled meeting".to_string()
    } else {
        m.title.trim().to_string()
    })];

    let mut meta = |label: &str, value: String| {
        blocks.push(Block::Paragraph(vec![
            Span::bold(format!("{label}: ")),
            Span::plain(value),
        ]));
    };
    meta("Date", super::human_date(&m.created_at));
    if let Some(d) = m.duration_seconds {
        meta("Duration", format_timestamp(d));
    }
    let attendees = super::attendee_names(m);
    if !attendees.is_empty() {
        meta("Attendees", attendees.join(", "));
    }
    if !m.speakers.is_empty() {
        meta("Speakers", m.speakers.join(", "));
    }

    if let Some(summary) = &m.summary_markdown {
        blocks.push(heading(1, "Summary"));
        blocks.extend(markdown_to_blocks(summary, 2));
    }

    if !m.action_items.is_empty() {
        blocks.push(heading(1, "Action Items"));
        for item in &m.action_items {
            let mut spans = vec![Span::plain(item.text.trim())];
            if let Some(a) = item.assignee.as_ref().filter(|a| !a.trim().is_empty()) {
                spans.push(Span::plain(" — "));
                spans.push(Span::bold(a.trim()));
            }
            if let Some(d) = item.due_hint.as_ref().filter(|d| !d.trim().is_empty()) {
                spans.push(Span::muted(format!(" ({})", d.trim())));
            }
            blocks.push(Block::ListItem {
                depth: 0,
                marker: ListMarker::Task(item.status == "done"),
                spans,
            });
        }
    }

    if !m.notes.is_empty() {
        blocks.push(heading(1, "Notes"));
        for note in &m.notes {
            blocks.push(Block::ListItem {
                depth: 0,
                marker: ListMarker::Bullet,
                spans: vec![Span::plain(note.body.trim())],
            });
        }
    }

    if !m.transcript.is_empty() {
        blocks.push(heading(1, "Transcript"));
        for seg in &m.transcript {
            let text = seg.text.trim();
            if text.is_empty() {
                continue;
            }
            let mut spans = Vec::new();
            if let Some(label) = &seg.start_label {
                spans.push(Span::muted(format!("[{label}] ")));
            }
            if let Some(speaker) = seg.speaker.as_ref().filter(|s| !s.trim().is_empty()) {
                spans.push(Span::bold(format!("{}: ", speaker.trim())));
            }
            spans.push(Span::plain(text));
            blocks.push(Block::Paragraph(spans));
        }
    }

    blocks
}

fn heading(level: u8, text: &str) -> Block {
    Block::Heading {
        level,
        spans: vec![Span::plain(text)],
    }
}

fn heading_level_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Which block the inline content currently being collected belongs to.
#[derive(Debug, Clone, Copy)]
enum InlineKind {
    Paragraph,
    Heading(u8),
    Item { depth: u8, marker: ListMarker },
}

/// Convert summary markdown into blocks. The shallowest heading in the input
/// is mapped to `top_level` and deeper ones follow it, clamped to 1..=4, so a
/// summary that uses `##` and `###` nests under the export's own "Summary"
/// heading whatever level the model started at.
pub fn markdown_to_blocks(markdown: &str, top_level: u8) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;

    let min_level = Parser::new_ext(markdown, options)
        .filter_map(|e| match e {
            Event::Start(Tag::Heading { level, .. }) => Some(heading_level_num(level)),
            _ => None,
        })
        .min()
        .unwrap_or(1);

    let mut blocks = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut kind: Option<InlineKind> = None;

    let mut bold = 0u32;
    let mut italic = 0u32;
    let mut strike = 0u32;
    let mut image = 0u32;
    let mut quote = 0u32;

    // One entry per open list: the next ordinal for ordered lists.
    let mut lists: Vec<Option<u64>> = Vec::new();

    let mut code_block: Option<String> = None;

    // Table state.
    let mut in_table_head = false;
    let mut table_header: Vec<Vec<Span>> = Vec::new();
    let mut table_rows: Vec<Vec<Vec<Span>>> = Vec::new();
    let mut table_row: Vec<Vec<Span>> = Vec::new();
    let mut in_cell = false;

    fn flush(
        blocks: &mut Vec<Block>,
        spans: &mut Vec<Span>,
        kind: &mut Option<InlineKind>,
        quote: u32,
    ) {
        // Trim the edges so markdown's structural whitespace doesn't leak.
        while spans.first().is_some_and(|s| s.text.trim().is_empty()) {
            spans.remove(0);
        }
        while spans.last().is_some_and(|s| s.text.trim().is_empty()) {
            spans.pop();
        }
        if let Some(first) = spans.first_mut() {
            first.text = first.text.trim_start().to_string();
        }
        if let Some(last) = spans.last_mut() {
            last.text = last.text.trim_end().to_string();
        }
        let content = std::mem::take(spans);
        let Some(k) = kind.take() else {
            return;
        };
        match k {
            InlineKind::Item { depth, marker } => {
                // An item with no text of its own (e.g. only a nested list)
                // still shows its marker only when it's a task checkbox.
                if !content.is_empty() || matches!(marker, ListMarker::Task(_)) {
                    blocks.push(Block::ListItem {
                        depth,
                        marker,
                        spans: content,
                    });
                }
            }
            _ if content.is_empty() => {}
            InlineKind::Heading(level) => blocks.push(Block::Heading {
                level,
                spans: content,
            }),
            InlineKind::Paragraph if quote > 0 => blocks.push(Block::Quote(content)),
            InlineKind::Paragraph => blocks.push(Block::Paragraph(content)),
        }
    }

    for event in Parser::new_ext(markdown, options) {
        let style = |text: String, code: bool| Span {
            text,
            bold: bold > 0,
            italic: italic > 0,
            strike: strike > 0,
            code,
            muted: false,
        };
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if in_cell {
                        continue;
                    }
                    match kind {
                        // First paragraph of a loose list item: keep
                        // collecting into the item.
                        Some(InlineKind::Item { .. }) if spans.is_empty() => {}
                        // A later paragraph in the same item continues it on
                        // a new line.
                        Some(InlineKind::Item { .. }) => push_span(&mut spans, Span::plain("\n")),
                        _ => {
                            flush(&mut blocks, &mut spans, &mut kind, quote);
                            kind = Some(InlineKind::Paragraph);
                        }
                    }
                }
                Tag::Heading { level, .. } => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    let n = heading_level_num(level);
                    let mapped = (n.saturating_sub(min_level) + top_level).clamp(1, 4);
                    kind = Some(InlineKind::Heading(mapped));
                }
                Tag::BlockQuote(_) => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    quote += 1;
                }
                Tag::CodeBlock(_) => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    code_block = Some(String::new());
                }
                Tag::HtmlBlock => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    kind = Some(InlineKind::Paragraph);
                }
                Tag::List(start) => {
                    // A nested list ends the parent item's own text.
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    lists.push(start);
                }
                Tag::Item => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    let depth = lists.len().saturating_sub(1).min(8) as u8;
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let m = ListMarker::Ordered(*n);
                            *n += 1;
                            m
                        }
                        _ => ListMarker::Bullet,
                    };
                    kind = Some(InlineKind::Item { depth, marker });
                }
                Tag::Table(_) => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    table_header.clear();
                    table_rows.clear();
                }
                Tag::TableHead => {
                    in_table_head = true;
                    table_row.clear();
                }
                Tag::TableRow => table_row.clear(),
                Tag::TableCell => {
                    in_cell = true;
                    spans.clear();
                }
                Tag::Emphasis => italic += 1,
                Tag::Strong => bold += 1,
                Tag::Strikethrough => strike += 1,
                Tag::Image { .. } => image += 1,
                // Links keep their visible label; the destination is dropped.
                Tag::Link { .. } => {}
                Tag::FootnoteDefinition(_)
                | Tag::DefinitionList
                | Tag::DefinitionListTitle
                | Tag::DefinitionListDefinition
                | Tag::Superscript
                | Tag::Subscript
                | Tag::MetadataBlock(_) => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if in_cell || matches!(kind, Some(InlineKind::Item { .. })) {
                        continue;
                    }
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                }
                TagEnd::Heading(_) | TagEnd::HtmlBlock => {
                    flush(&mut blocks, &mut spans, &mut kind, quote)
                }
                TagEnd::BlockQuote(_) => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    quote = quote.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = code_block.take() {
                        let code = code.trim_end_matches('\n').to_string();
                        if !code.trim().is_empty() {
                            blocks.push(Block::Code(code));
                        }
                    }
                }
                TagEnd::List(_) => {
                    flush(&mut blocks, &mut spans, &mut kind, quote);
                    lists.pop();
                }
                TagEnd::Item => flush(&mut blocks, &mut spans, &mut kind, quote),
                TagEnd::TableHead => {
                    in_table_head = false;
                    table_header = std::mem::take(&mut table_row);
                }
                TagEnd::TableRow => {
                    if !in_table_head {
                        table_rows.push(std::mem::take(&mut table_row));
                    }
                }
                TagEnd::TableCell => {
                    in_cell = false;
                    let mut cell = std::mem::take(&mut spans);
                    if let Some(first) = cell.first_mut() {
                        first.text = first.text.trim_start().to_string();
                    }
                    if let Some(last) = cell.last_mut() {
                        last.text = last.text.trim_end().to_string();
                    }
                    cell.retain(|s| !s.text.is_empty());
                    table_row.push(cell);
                }
                TagEnd::Table => {
                    let header = std::mem::take(&mut table_header);
                    let rows = std::mem::take(&mut table_rows);
                    if !header.is_empty() || !rows.is_empty() {
                        blocks.push(Block::Table { header, rows });
                    }
                }
                TagEnd::Emphasis => italic = italic.saturating_sub(1),
                TagEnd::Strong => bold = bold.saturating_sub(1),
                TagEnd::Strikethrough => strike = strike.saturating_sub(1),
                TagEnd::Image => image = image.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => {
                if let Some(code) = code_block.as_mut() {
                    code.push_str(&text);
                } else if image == 0 {
                    if kind.is_none() && !in_cell {
                        kind = Some(InlineKind::Paragraph);
                    }
                    push_span(&mut spans, style(text.to_string(), false));
                }
            }
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                if image == 0 {
                    if kind.is_none() && !in_cell {
                        kind = Some(InlineKind::Paragraph);
                    }
                    push_span(&mut spans, style(text.to_string(), true));
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let lower = html.trim().to_ascii_lowercase();
                if lower.starts_with("<br") {
                    push_span(&mut spans, Span::plain("\n"));
                } else {
                    let text = strip_tags(&html);
                    if !text.trim().is_empty() {
                        if kind.is_none() && !in_cell {
                            kind = Some(InlineKind::Paragraph);
                        }
                        push_span(&mut spans, style(text, false));
                    }
                }
            }
            Event::SoftBreak => push_span(&mut spans, style(" ".to_string(), false)),
            Event::HardBreak => push_span(&mut spans, Span::plain("\n")),
            Event::Rule => {
                flush(&mut blocks, &mut spans, &mut kind, quote);
                blocks.push(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some(InlineKind::Item { marker, .. }) = kind.as_mut() {
                    *marker = ListMarker::Task(checked);
                }
            }
            Event::FootnoteReference(name) => {
                push_span(&mut spans, style(format!("[{name}]"), false));
            }
        }
    }
    flush(&mut blocks, &mut spans, &mut kind, quote);
    blocks
}

/// Drop anything that looks like an HTML tag, keeping the text between.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_are_rebased_under_the_summary_heading() {
        let blocks = markdown_to_blocks("## Key Points\n\ntext\n\n### Detail\n", 2);
        assert_eq!(
            blocks,
            vec![
                Block::Heading {
                    level: 2,
                    spans: vec![Span::plain("Key Points")]
                },
                Block::Paragraph(vec![Span::plain("text")]),
                Block::Heading {
                    level: 3,
                    spans: vec![Span::plain("Detail")]
                },
            ]
        );
    }

    #[test]
    fn inline_styles_and_links() {
        let blocks = markdown_to_blocks(
            "Hello **bold** and *it* [label](https://example.com) ![alt](x.png)",
            2,
        );
        let Block::Paragraph(spans) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(spans_text(spans), "Hello bold and it label");
        assert!(spans.iter().any(|s| s.bold && s.text == "bold"));
        assert!(spans.iter().any(|s| s.italic && s.text == "it"));
        assert!(!spans_text(spans).contains("example.com"));
    }

    #[test]
    fn nested_ordered_and_task_lists() {
        let md = "- top\n  - nested\n1. one\n2. two\n- [x] done\n- [ ] open\n";
        let blocks = markdown_to_blocks(md, 2);
        let items: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::ListItem {
                    depth,
                    marker,
                    spans,
                } => Some((*depth, *marker, spans_text(spans))),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            vec![
                (0, ListMarker::Bullet, "top".to_string()),
                (1, ListMarker::Bullet, "nested".to_string()),
                (0, ListMarker::Ordered(1), "one".to_string()),
                (0, ListMarker::Ordered(2), "two".to_string()),
                (0, ListMarker::Task(true), "done".to_string()),
                (0, ListMarker::Task(false), "open".to_string()),
            ]
        );
    }

    #[test]
    fn tables_keep_header_and_rows() {
        let md = "| **Owner** | Blocker |\n| --- | --- |\n| Seb | CI |\n";
        let blocks = markdown_to_blocks(md, 2);
        let Block::Table { header, rows } = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(header.len(), 2);
        assert_eq!(spans_text(&header[0]), "Owner");
        assert!(header[0][0].bold);
        assert_eq!(rows.len(), 1);
        assert_eq!(spans_text(&rows[0][1]), "CI");
    }

    #[test]
    fn html_is_reduced_to_text() {
        let blocks = markdown_to_blocks("a<br>b <span>c</span>\n\n<div>block</div>\n", 2);
        let texts: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph(s) => Some(spans_text(s)),
                _ => None,
            })
            .collect();
        assert_eq!(texts[0], "a\nb c");
        assert!(texts
            .iter()
            .any(|t| t.contains("block") && !t.contains('<')));
    }
}
