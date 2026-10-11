//! DOCX rendering of the export [`Block`] model via `docx-rs`.
//!
//! Headings use real Word heading styles (style names `heading 1`… so Word
//! and LibreOffice treat them as built-in headings for the navigation pane
//! and TOC), bullets use Word numbering, and each ordered list gets its own
//! numbering definition so it restarts at its own first number. Task items
//! render as ☐/☑ glyphs: Word has no portable checkbox list type.

use docx_rs::{
    AbstractNumbering, AlignmentType, BreakType, Docx, IndentLevel, Level, LevelJc, LevelText,
    LineSpacing, NumberFormat, Numbering, NumberingId, Paragraph, Run, RunFonts, Shading,
    SpecialIndentType, Start, Style, StyleType, Table, TableCell, TableRow,
};

use super::document::{Block, ListMarker, Span};

const BULLET_NUMBERING_ID: usize = 1;
/// Ordered lists get ids from here up, one per list.
const FIRST_ORDERED_NUMBERING_ID: usize = 100;
const MUTED_COLOR: &str = "6B7280";
const CODE_FONT: &str = "Courier New";
/// One list-indent step, in twentieths of a point (0.5 in).
const INDENT_STEP: i32 = 720;
const HANGING: i32 = 360;

pub(super) fn render_docx(blocks: &[Block], created_at: &str) -> Result<Vec<u8>, String> {
    let mut docx = Docx::new()
        .default_size(22) // half-points: 11 pt
        .created_at(created_at)
        .add_style(paragraph_style("Title", "Title", 48, true))
        .add_style(paragraph_style("Heading1", "heading 1", 32, true).outline_lvl(0))
        .add_style(paragraph_style("Heading2", "heading 2", 28, true).outline_lvl(1))
        .add_style(paragraph_style("Heading3", "heading 3", 24, true).outline_lvl(2))
        .add_style(paragraph_style("Heading4", "heading 4", 22, true).outline_lvl(3))
        .add_abstract_numbering(bullet_numbering())
        .add_numbering(Numbering::new(BULLET_NUMBERING_ID, BULLET_NUMBERING_ID));

    let mut next_ordered_id = FIRST_ORDERED_NUMBERING_ID;
    // The numbering id of the ordered list currently being emitted, per
    // depth; reset whenever a non-list block interrupts.
    let mut ordered_ids: Vec<Option<usize>> = Vec::new();

    for block in blocks {
        if !matches!(block, Block::ListItem { .. }) {
            ordered_ids.clear();
        }
        match block {
            Block::Title(title) => {
                docx = docx.add_paragraph(
                    Paragraph::new()
                        .style("Title")
                        .add_run(Run::new().add_text(xml_safe(title))),
                );
            }
            Block::Heading { level, spans } => {
                let style = match level {
                    1 => "Heading1",
                    2 => "Heading2",
                    3 => "Heading3",
                    _ => "Heading4",
                };
                docx = docx.add_paragraph(add_spans(
                    Paragraph::new().style(style).keep_next(true),
                    spans,
                ));
            }
            Block::Paragraph(spans) => {
                docx = docx.add_paragraph(add_spans(spaced(Paragraph::new()), spans));
            }
            Block::ListItem {
                depth,
                marker,
                spans,
            } => {
                let depth = *depth as usize;
                ordered_ids.truncate(depth + 1);
                while ordered_ids.len() <= depth {
                    ordered_ids.push(None);
                }
                let p = match marker {
                    ListMarker::Bullet => {
                        ordered_ids[depth] = None;
                        Paragraph::new().numbering(
                            NumberingId::new(BULLET_NUMBERING_ID),
                            IndentLevel::new(depth.min(8)),
                        )
                    }
                    ListMarker::Ordered(n) => {
                        let id = match ordered_ids[depth] {
                            Some(id) => id,
                            None => {
                                let id = next_ordered_id;
                                next_ordered_id += 1;
                                docx = docx
                                    .add_abstract_numbering(ordered_numbering(id, *n, depth))
                                    .add_numbering(Numbering::new(id, id));
                                ordered_ids[depth] = Some(id);
                                id
                            }
                        };
                        Paragraph::new()
                            .numbering(NumberingId::new(id), IndentLevel::new(depth.min(8)))
                    }
                    ListMarker::Task(done) => {
                        ordered_ids[depth] = None;
                        let left = INDENT_STEP * (depth as i32 + 1);
                        Paragraph::new()
                            .indent(
                                Some(left),
                                Some(SpecialIndentType::Hanging(HANGING)),
                                None,
                                None,
                            )
                            .add_run(Run::new().add_text(if *done { "☑" } else { "☐" }))
                            .add_run(Run::new().add_tab())
                    }
                };
                docx = docx.add_paragraph(add_spans(p, spans));
            }
            Block::Quote(spans) => {
                let p = spaced(Paragraph::new())
                    .indent(Some(INDENT_STEP), None, None, None)
                    .italic()
                    .color(MUTED_COLOR);
                docx = docx.add_paragraph(add_spans(p, spans));
            }
            Block::Code(code) => {
                let mut p = spaced(Paragraph::new());
                for (i, line) in code.lines().enumerate() {
                    let mut run = Run::new()
                        .fonts(code_fonts())
                        .size(20)
                        .shading(Shading::new().fill("F3F4F6"));
                    if i > 0 {
                        run = run.add_break(BreakType::TextWrapping);
                    }
                    p = p.add_run(run.add_text(xml_safe(line)));
                }
                docx = docx.add_paragraph(p);
            }
            Block::Rule => {
                docx = docx.add_paragraph(
                    Paragraph::new()
                        .align(AlignmentType::Center)
                        .add_run(Run::new().color(MUTED_COLOR).add_text("* * *")),
                );
            }
            Block::Table { header, rows } => {
                let columns = header
                    .len()
                    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                if columns == 0 {
                    continue;
                }
                let mut table_rows = Vec::new();
                if !header.is_empty() {
                    table_rows.push(table_row(header, columns, true));
                }
                for row in rows {
                    table_rows.push(table_row(row, columns, false));
                }
                // 9026 twips = A4 text width with 1-inch margins.
                let col_width = 9026 / columns;
                docx = docx
                    .add_table(Table::new(table_rows).set_grid(vec![col_width; columns]))
                    .add_paragraph(Paragraph::new());
            }
        }
    }

    let mut buf = std::io::Cursor::new(Vec::new());
    docx.build()
        .pack(&mut buf)
        .map_err(|e| format!("Failed to build DOCX: {e}"))?;
    Ok(buf.into_inner())
}

fn paragraph_style(id: &str, name: &str, size: usize, bold: bool) -> Style {
    let style = Style::new(id, StyleType::Paragraph)
        .name(name)
        .size(size)
        .line_spacing(LineSpacing::new().before(240).after(120));
    if bold {
        style.bold()
    } else {
        style
    }
}

fn spaced(p: Paragraph) -> Paragraph {
    p.line_spacing(LineSpacing::new().after(120))
}

fn code_fonts() -> RunFonts {
    RunFonts::new().ascii(CODE_FONT).hi_ansi(CODE_FONT)
}

fn bullet_numbering() -> AbstractNumbering {
    let glyphs = ["•", "◦", "▪"];
    let mut numbering = AbstractNumbering::new(BULLET_NUMBERING_ID);
    for level in 0..9usize {
        numbering = numbering.add_level(
            Level::new(
                level,
                Start::new(1),
                NumberFormat::new("bullet"),
                LevelText::new(glyphs[level % glyphs.len()]),
                LevelJc::new("left"),
            )
            .indent(
                Some(INDENT_STEP * (level as i32 + 1)),
                Some(SpecialIndentType::Hanging(HANGING)),
                None,
                None,
            ),
        );
    }
    numbering
}

/// A one-list numbering definition starting at `start`. Only `depth` gets
/// the custom start; the other levels exist so Word never sees a reference
/// to an undefined level.
fn ordered_numbering(id: usize, start: u64, depth: usize) -> AbstractNumbering {
    let mut numbering = AbstractNumbering::new(id);
    for level in 0..9usize {
        let first = if level == depth {
            start.min(u32::MAX as u64) as usize
        } else {
            1
        };
        numbering = numbering.add_level(
            Level::new(
                level,
                Start::new(first),
                NumberFormat::new("decimal"),
                LevelText::new(format!("%{}.", level + 1)),
                LevelJc::new("left"),
            )
            .indent(
                Some(INDENT_STEP * (level as i32 + 1)),
                Some(SpecialIndentType::Hanging(HANGING)),
                None,
                None,
            ),
        );
    }
    numbering
}

fn table_row(cells: &[Vec<Span>], columns: usize, header: bool) -> TableRow {
    let mut out = Vec::with_capacity(columns);
    for i in 0..columns {
        let spans = cells.get(i).map(Vec::as_slice).unwrap_or(&[]);
        let mut p = Paragraph::new();
        if header {
            p = p.bold();
        }
        let mut cell = TableCell::new().add_paragraph(add_spans(p, spans));
        if header {
            cell = cell.shading(Shading::new().fill("F3F4F6"));
        }
        out.push(cell);
    }
    TableRow::new(out)
}

fn add_spans(mut p: Paragraph, spans: &[Span]) -> Paragraph {
    for span in spans {
        for (i, line) in span.text.split('\n').enumerate() {
            let mut run = Run::new();
            if i > 0 {
                run = run.add_break(BreakType::TextWrapping);
            }
            if span.bold {
                run = run.bold();
            }
            if span.italic {
                run = run.italic();
            }
            if span.strike {
                run = run.strike();
            }
            if span.code {
                run = run
                    .fonts(code_fonts())
                    .shading(Shading::new().fill("F3F4F6"));
            }
            if span.muted {
                run = run.color(MUTED_COLOR);
            }
            if !line.is_empty() {
                run = run.add_text(xml_safe(line));
            }
            p = p.add_run(run);
        }
    }
    p
}

/// Strip characters XML 1.0 can't carry at all (C0 controls other than tab
/// and newline). docx-rs escapes `&`/`<`/`>` itself, but a stray control
/// character from a model or a transcript would make Word refuse the file.
pub(super) fn xml_safe(text: &str) -> String {
    text.chars()
        .filter(|&c| matches!(c, '\t' | '\n' | '\r') || !(c.is_control() && (c as u32) < 0x20))
        .filter(|&c| !matches!(c, '\u{FFFE}' | '\u{FFFF}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip through docx-rs's own reader: proves the package is a
    /// well-formed DOCX and gives the document back as inspectable JSON.
    fn document_json(bytes: &[u8]) -> String {
        docx_rs::read_docx(bytes).expect("readable docx").json()
    }

    #[test]
    fn renders_a_valid_docx_with_escaped_text() {
        let blocks = vec![
            Block::Title("Q3 <Budget> & \u{1}Review".to_string()),
            Block::Heading {
                level: 1,
                spans: vec![Span::plain("Summary")],
            },
            Block::Paragraph(vec![Span::bold("Bold"), Span::plain(" text")]),
            Block::ListItem {
                depth: 0,
                marker: ListMarker::Ordered(3),
                spans: vec![Span::plain("third")],
            },
            Block::ListItem {
                depth: 0,
                marker: ListMarker::Task(true),
                spans: vec![Span::plain("done")],
            },
            Block::Table {
                header: vec![vec![Span::plain("Owner")], vec![Span::plain("Task")]],
                rows: vec![vec![vec![Span::plain("Seb")]]],
            },
        ];
        let bytes = render_docx(&blocks, "2026-07-15T10:00:00Z").unwrap();
        assert!(bytes.starts_with(b"PK"));
        let json = document_json(&bytes);
        assert!(json.contains("Q3 <Budget> & Review"), "{json}");
        assert!(json.contains("Heading1"));
        assert!(json.contains("third"));
        assert!(json.contains("☑"));
        assert!(json.contains("\"table\""));
    }

    #[test]
    fn xml_safe_drops_control_characters_only() {
        assert_eq!(xml_safe("a\u{0}b\u{1b}c\td\u{FFFF}é"), "abc\tdé");
    }
}
