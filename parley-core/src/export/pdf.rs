//! PDF rendering of the export [`Block`] model via `krilla`.
//!
//! krilla writes the PDF (font subsetting, ToUnicode maps so text stays
//! selectable/searchable); this module does the layout: word-wrapping,
//! pagination, lists, tables, and a page footer.
//!
//! Fonts come from the system (via fontdb), not from the binary: a sans
//! family from a preference list for body text, a monospace family for
//! code, and per-character fallback across known wide-coverage families
//! (CJK, Arabic, …) and finally every installed face, so a transcript in
//! another script still renders. Width measurement uses ttf-parser advances
//! (no kerning/ligatures), which can only over-estimate Latin widths, so
//! lines never overflow the margin. Known limits: right-to-left paragraphs
//! are laid out left-to-right word by word, and colour-emoji fonts are
//! skipped (emoji fall back to an outline emoji font if one is installed).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use krilla::color::rgb;
use krilla::geom::{PathBuilder, Point, Rect};
use krilla::metadata::Metadata;
use krilla::num::NormalizedF32;
use krilla::page::PageSettings;
use krilla::paint::{Fill, Stroke};
use krilla::surface::Surface;
use krilla::text::{Font, Tag, TextDirection};
use krilla::Document;

use super::document::{Block, ListMarker, Span};

const PAGE_W: f32 = 595.28; // A4
const PAGE_H: f32 = 841.89;
const MARGIN_X: f32 = 56.0;
const MARGIN_TOP: f32 = 56.0;
const MARGIN_BOTTOM: f32 = 64.0;
const CONTENT_W: f32 = PAGE_W - 2.0 * MARGIN_X;
const LINE_FACTOR: f32 = 1.4;

const BODY_SIZE: f32 = 10.5;
const CODE_SIZE: f32 = 9.0;
const FOOTER_SIZE: f32 = 8.0;
const LIST_INDENT: f32 = 16.0;
const MARKER_GAP: f32 = 14.0;
const CELL_PAD: f32 = 4.0;

type Rgb = (u8, u8, u8);
const TEXT: Rgb = (17, 24, 39);
const MUTED: Rgb = (107, 114, 128);
const RULE: Rgb = (209, 213, 219);
const SHADE: Rgb = (243, 244, 246);

/// Body-text families, in preference order. The first installed one wins.
const SANS_FAMILIES: &[&str] = &[
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
    "Helvetica",
    "Open Sans",
    "Adwaita Sans",
    "Cantarell",
    "Ubuntu",
    "Roboto",
    "FreeSans",
];

const MONO_FAMILIES: &[&str] = &[
    "DejaVu Sans Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Adwaita Mono",
    "Source Code Pro",
    "JetBrains Mono",
    "Ubuntu Mono",
    "Courier New",
    "FreeMono",
];

/// Tried, in order, for characters the body font lacks, before falling back
/// to scanning every installed face.
const FALLBACK_FAMILIES: &[&str] = &[
    "DejaVu Sans",
    "Noto Sans Symbols",
    "Noto Sans Symbols 2",
    "Noto Sans CJK SC",
    "Noto Sans CJK JP",
    "Noto Sans CJK KR",
    "Noto Sans CJK TC",
    "Noto Sans Arabic",
    "Noto Sans Hebrew",
    "Noto Sans Devanagari",
    "Noto Sans Bengali",
    "Noto Sans Tamil",
    "Noto Sans Thai",
    "Noto Sans Georgian",
    "Noto Sans Armenian",
    "Noto Sans Ethiopic",
    "Noto Emoji",
    "Droid Sans Fallback",
    "WenQuanYi Micro Hei",
    "Symbola",
    "FreeSerif",
    "Unifont",
];

/// Characters per export allowed to trigger a scan of every installed face.
const MAX_FULL_SCANS: usize = 48;

pub(super) fn render_pdf(blocks: &[Block], title: &str) -> Result<Vec<u8>, String> {
    let mut fonts = Fonts::load()?;
    let mut layout = Layout::new();
    for block in blocks {
        layout.block(&mut fonts, block);
    }
    let pages = layout.finish();
    draw(&mut fonts, pages, title)
}

// ---- Fonts -----------------------------------------------------------------

fn font_db() -> &'static fontdb::Database {
    static DB: OnceLock<fontdb::Database> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        db
    })
}

struct LoadedFace {
    data: Arc<Vec<u8>>,
    index: u32,
    /// Variation coordinates, e.g. `wght=700` to get bold out of a
    /// variable font that has no separate bold face.
    coords: Vec<([u8; 4], f32)>,
    font: Font,
    units_per_em: f32,
}

impl LoadedFace {
    fn load(db: &fontdb::Database, id: fontdb::ID, coords: Vec<([u8; 4], f32)>) -> Option<Self> {
        let (data, index) = db.with_face_data(id, |data, index| (data.to_vec(), index))?;
        let data = Arc::new(data);
        let units_per_em = ttf_parser::Face::parse(&data, index).ok()?.units_per_em() as f32;
        let krilla_coords: Vec<(Tag, f32)> =
            coords.iter().map(|(t, v)| (Tag::new(t), *v)).collect();
        let font = Font::new_variable(data.clone().into(), index, &krilla_coords)?;
        Some(Self {
            data,
            index,
            coords,
            font,
            units_per_em,
        })
    }

    fn parse(&self) -> Option<ttf_parser::Face<'_>> {
        let mut face = ttf_parser::Face::parse(&self.data, self.index).ok()?;
        for (tag, value) in &self.coords {
            face.set_variation(ttf_parser::Tag::from_bytes(tag), *value);
        }
        Some(face)
    }

    /// Sum of advances, in font units. Exact for Latin/Cyrillic/Greek/CJK
    /// apart from kerning, which only ever makes the real line narrower.
    fn advance_width(&self, text: &str) -> f32 {
        self.parse()
            .map(|f| {
                text.chars()
                    .map(|c| {
                        f.glyph_index(c)
                            .and_then(|g| f.glyph_hor_advance(g))
                            .unwrap_or(0) as f32
                    })
                    .sum()
            })
            .unwrap_or(0.0)
    }

    /// Width after shaping, in font units — what krilla will actually draw
    /// for scripts whose glyphs change with context (Arabic, Indic, …).
    fn shaped_width(&self, text: &str) -> f32 {
        let Some(mut face) = rustybuzz::Face::from_slice(&self.data, self.index) else {
            return 0.0;
        };
        let variations: Vec<rustybuzz::Variation> = self
            .coords
            .iter()
            .map(|(tag, value)| rustybuzz::Variation {
                tag: ttf_parser::Tag::from_bytes(tag),
                value: *value,
            })
            .collect();
        face.set_variations(&variations);
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        buffer.guess_segment_properties();
        rustybuzz::shape(&face, &[], buffer)
            .glyph_positions()
            .iter()
            .map(|p| p.x_advance as f32)
            .sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Role {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Mono,
}

struct Fonts {
    faces: Vec<LoadedFace>,
    regular: usize,
    bold: usize,
    italic: usize,
    bold_italic: usize,
    mono: usize,
    loaded: HashMap<(fontdb::ID, bool), usize>,
    coverage: HashMap<(usize, char), bool>,
    fallback: HashMap<char, Option<usize>>,
    /// Width of a word in font units (scale by size / units_per_em).
    widths: HashMap<(usize, String), f32>,
    full_scans: usize,
}

impl Fonts {
    fn load() -> Result<Self, String> {
        let db = font_db();
        let mut fonts = Fonts {
            faces: Vec::new(),
            regular: 0,
            bold: 0,
            italic: 0,
            bold_italic: 0,
            mono: 0,
            loaded: HashMap::new(),
            coverage: HashMap::new(),
            fallback: HashMap::new(),
            widths: HashMap::new(),
            full_scans: 0,
        };

        let regular_id = query(
            db,
            SANS_FAMILIES,
            true,
            fontdb::Weight::NORMAL,
            fontdb::Style::Normal,
        )
        .ok_or(
            "No usable system font found for PDF export (install e.g. DejaVu Sans or Noto Sans)",
        )?;
        fonts.regular = fonts
            .add(db, regular_id, false)
            .ok_or("Failed to load the system font for PDF export")?;

        fonts.bold = fonts.styled(db, regular_id, fontdb::Weight::BOLD, fontdb::Style::Normal);
        fonts.italic = fonts.styled(
            db,
            regular_id,
            fontdb::Weight::NORMAL,
            fontdb::Style::Italic,
        );
        fonts.bold_italic =
            fonts.styled(db, regular_id, fontdb::Weight::BOLD, fontdb::Style::Italic);
        fonts.mono = query(
            db,
            MONO_FAMILIES,
            false,
            fontdb::Weight::NORMAL,
            fontdb::Style::Normal,
        )
        .and_then(|id| fonts.add(db, id, false))
        .unwrap_or(fonts.regular);
        Ok(fonts)
    }

    /// The bold/italic variant of the body family. Falls back to a `wght`
    /// variation for variable fonts, then to the regular face.
    fn styled(
        &mut self,
        db: &fontdb::Database,
        regular_id: fontdb::ID,
        weight: fontdb::Weight,
        style: fontdb::Style,
    ) -> usize {
        let family = db
            .face(regular_id)
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()));
        let wants_bold = weight == fontdb::Weight::BOLD;
        let found = family.as_deref().and_then(|family| {
            db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(family)],
                weight,
                style,
                ..Default::default()
            })
        });
        let fits = |id: fontdb::ID| {
            db.face(id).is_some_and(|f| {
                (f.style != fontdb::Style::Normal) == (style != fontdb::Style::Normal)
            })
        };
        match found {
            Some(id) if id != regular_id && fits(id) => {
                // A static face of the right style; still bolden it with a
                // variation if it's variable and came back regular weight.
                let synth = wants_bold && db.face(id).is_some_and(|f| f.weight.0 < 600);
                self.add(db, id, synth).unwrap_or(self.regular)
            }
            _ if wants_bold => self.add(db, regular_id, true).unwrap_or(self.regular),
            _ => self.regular,
        }
    }

    fn add(
        &mut self,
        db: &fontdb::Database,
        id: fontdb::ID,
        bold_variation: bool,
    ) -> Option<usize> {
        if let Some(&idx) = self.loaded.get(&(id, bold_variation)) {
            return Some(idx);
        }
        let has_wght = db
            .with_face_data(id, |data, index| {
                ttf_parser::Face::parse(data, index).ok().is_some_and(|f| {
                    f.variation_axes()
                        .into_iter()
                        .any(|a| a.tag == ttf_parser::Tag::from_bytes(b"wght"))
                })
            })
            .unwrap_or(false);
        if bold_variation && !has_wght {
            return None;
        }
        // Pin the weight of every variable face: some (Noto Sans CJK VF)
        // default to a light instance, which reads as greyed-out text.
        let coords = if has_wght {
            vec![(*b"wght", if bold_variation { 700.0 } else { 400.0 })]
        } else {
            Vec::new()
        };
        let face = LoadedFace::load(db, id, coords)?;
        self.faces.push(face);
        let idx = self.faces.len() - 1;
        self.loaded.insert((id, bold_variation), idx);
        Some(idx)
    }

    fn role(&self, role: Role) -> usize {
        match role {
            Role::Regular => self.regular,
            Role::Bold => self.bold,
            Role::Italic => self.italic,
            Role::BoldItalic => self.bold_italic,
            Role::Mono => self.mono,
        }
    }

    fn covers(&mut self, face: usize, c: char) -> bool {
        if let Some(&hit) = self.coverage.get(&(face, c)) {
            return hit;
        }
        let hit = self.faces[face]
            .parse()
            .is_some_and(|f| f.glyph_index(c).is_some());
        self.coverage.insert((face, c), hit);
        hit
    }

    /// A face that has `c`, preferring `preferred`, then the body font, then
    /// fallbacks. Returns `preferred` (which will draw `.notdef`) if nothing
    /// installed covers it.
    fn face_for(&mut self, preferred: usize, c: char) -> usize {
        if c.is_whitespace() || c.is_control() || self.covers(preferred, c) {
            return preferred;
        }
        if self.covers(self.regular, c) {
            return self.regular;
        }
        self.fallback_face(c).unwrap_or(preferred)
    }

    fn fallback_face(&mut self, c: char) -> Option<usize> {
        if let Some(&cached) = self.fallback.get(&c) {
            return cached;
        }
        // Faces loaded so far (earlier fallbacks often cover a whole script).
        let mut found = (0..self.faces.len()).find(|&i| self.covers(i, c));

        let db = font_db();
        if found.is_none() {
            for family in FALLBACK_FAMILIES {
                let Some(id) = db.query(&fontdb::Query {
                    families: &[fontdb::Family::Name(family)],
                    ..Default::default()
                }) else {
                    continue;
                };
                if face_has_char(db, id, c) {
                    found = self.add(db, id, false);
                    if found.is_some() {
                        break;
                    }
                }
            }
        }
        if found.is_none() && self.full_scans < MAX_FULL_SCANS {
            self.full_scans += 1;
            for info in db.faces() {
                // Colour-bitmap emoji fonts need image support we don't build.
                if info
                    .families
                    .iter()
                    .any(|(name, _)| name.contains("Color Emoji"))
                {
                    continue;
                }
                if face_has_char(db, info.id, c) {
                    found = self.add(db, info.id, false);
                    if found.is_some() {
                        break;
                    }
                }
            }
        }
        self.fallback.insert(c, found);
        found
    }

    /// Split `text` into runs that each use one face.
    fn runs(&mut self, preferred: usize, text: &str) -> Vec<(usize, String)> {
        let mut runs: Vec<(usize, String)> = Vec::new();
        for c in text.chars() {
            let mut face = self.face_for(preferred, c);
            // Keep combining marks and spaces with the run they belong to.
            if let Some((last_face, last)) = runs.last_mut() {
                if face == preferred && (c.is_whitespace() || is_mark(c)) {
                    face = *last_face;
                }
                if *last_face == face {
                    last.push(c);
                    continue;
                }
            }
            runs.push((face, c.to_string()));
        }
        runs
    }

    fn width(&mut self, face: usize, text: &str, size: f32) -> f32 {
        let key = (face, text.to_string());
        let units = match self.widths.get(&key) {
            Some(&w) => w,
            None => {
                // Shaping builds a full rustybuzz face per call, which is
                // slow for big CJK fonts; only scripts whose glyphs change
                // with context need it.
                let units = if text.chars().any(needs_shaping) {
                    self.faces[face].shaped_width(text)
                } else {
                    self.faces[face].advance_width(text)
                };
                self.widths.insert(key, units);
                units
            }
        };
        units * size / self.faces[face].units_per_em
    }
}

fn query(
    db: &fontdb::Database,
    families: &[&str],
    generic_fallback: bool,
    weight: fontdb::Weight,
    style: fontdb::Style,
) -> Option<fontdb::ID> {
    let mut list: Vec<fontdb::Family> = families.iter().map(|f| fontdb::Family::Name(f)).collect();
    if generic_fallback {
        list.push(fontdb::Family::SansSerif);
    }
    db.query(&fontdb::Query {
        families: &list,
        weight,
        style,
        ..Default::default()
    })
    .or_else(|| {
        // Nothing from the list: any face at all beats failing the export.
        generic_fallback
            .then(|| {
                db.faces()
                    .find(|f| f.style == fontdb::Style::Normal)
                    .map(|f| f.id)
            })
            .flatten()
    })
}

fn face_has_char(db: &fontdb::Database, id: fontdb::ID, c: char) -> bool {
    db.with_face_data(id, |data, index| {
        ttf_parser::Face::parse(data, index)
            .ok()
            .is_some_and(|f| f.glyph_index(c).is_some())
    })
    .unwrap_or(false)
}

/// Scripts whose rendered width differs from the sum of nominal advances
/// (joining, reordering, conjuncts): Hebrew through Arabic Extended, Indic,
/// Thai/Lao/Tibetan, Myanmar, Khmer, and the Arabic presentation forms.
fn needs_shaping(c: char) -> bool {
    matches!(c as u32, 0x0590..=0x08FF | 0x0900..=0x0DFF | 0x0E00..=0x0FFF | 0x1000..=0x109F | 0x1780..=0x17FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF)
}

/// Combining marks (the common blocks), kept in their base character's run.
fn is_mark(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
        || matches!(c, '\u{200D}' | '\u{FE0F}' | '\u{FE0E}')
}

// ---- Layout ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct TextStyle {
    size: f32,
    color: Rgb,
    role: Role,
    strike: bool,
}

impl TextStyle {
    fn body() -> Self {
        TextStyle {
            size: BODY_SIZE,
            color: TEXT,
            role: Role::Regular,
            strike: false,
        }
    }
    fn with_span(self, span: &Span) -> Self {
        let (bold, italic) = match self.role {
            Role::Bold => (true, false),
            Role::Italic => (false, true),
            Role::BoldItalic => (true, true),
            _ => (false, false),
        };
        let bold = bold || span.bold;
        let italic = italic || span.italic;
        let role = if span.code {
            Role::Mono
        } else {
            match (bold, italic) {
                (true, true) => Role::BoldItalic,
                (true, false) => Role::Bold,
                (false, true) => Role::Italic,
                (false, false) => Role::Regular,
            }
        };
        TextStyle {
            size: if span.code {
                self.size * 0.92
            } else {
                self.size
            },
            color: if span.muted { MUTED } else { self.color },
            role,
            strike: self.strike || span.strike,
        }
    }
}

#[derive(Debug, Clone)]
struct Piece {
    x: f32,
    width: f32,
    face: usize,
    text: String,
    style: TextStyle,
}

#[derive(Debug, Clone, Default)]
struct Line {
    pieces: Vec<Piece>,
    width: f32,
    height: f32,
}

impl Line {
    fn push(&mut self, piece: Piece, joined_by_space: bool) {
        if let Some(last) = self.pieces.last_mut() {
            if last.face == piece.face && last.style == piece.style {
                if joined_by_space {
                    last.text.push(' ');
                }
                last.text.push_str(&piece.text);
                last.width = piece.x + piece.width - last.x;
                self.width = self.width.max(piece.x + piece.width);
                return;
            }
        }
        self.width = self.width.max(piece.x + piece.width);
        self.pieces.push(piece);
    }
}

/// Greedy word-wrap of `spans` into lines no wider than `max_width`.
fn wrap(fonts: &mut Fonts, spans: &[Span], base: TextStyle, max_width: f32) -> Vec<Line> {
    let mut lines = vec![Line {
        height: base.size * LINE_FACTOR,
        ..Default::default()
    }];
    let mut x = 0.0f32;
    let mut pending_space: Option<f32> = None;

    let new_line = |lines: &mut Vec<Line>, x: &mut f32| {
        lines.push(Line {
            height: base.size * LINE_FACTOR,
            ..Default::default()
        });
        *x = 0.0;
    };

    for span in spans {
        let style = base.with_span(span);
        let preferred = fonts.role(style.role);
        for (i, hard_line) in span.text.split('\n').enumerate() {
            if i > 0 {
                new_line(&mut lines, &mut x);
                pending_space = None;
            }
            for (is_space, token) in tokens(hard_line) {
                if is_space {
                    pending_space = Some(fonts.width(preferred, " ", style.size));
                    continue;
                }
                let runs = fonts.runs(preferred, token);
                let mut measured: Vec<(usize, String, f32)> = runs
                    .into_iter()
                    .map(|(face, text)| {
                        let w = fonts.width(face, &text, style.size);
                        (face, text, w)
                    })
                    .collect();
                let word_w: f32 = measured.iter().map(|r| r.2).sum();
                let line_empty = lines.last().is_none_or(|l| l.pieces.is_empty());
                let mut space = if line_empty {
                    0.0
                } else {
                    pending_space.unwrap_or(0.0)
                };
                pending_space = None;

                if !line_empty && x + space + word_w > max_width {
                    new_line(&mut lines, &mut x);
                    space = 0.0;
                }
                if word_w > max_width {
                    // A single word wider than the line: break between chars.
                    x += space;
                    for (face, text, _) in measured.drain(..) {
                        for c in text.chars() {
                            let cw = fonts.width(face, &c.to_string(), style.size);
                            if x > 0.0 && x + cw > max_width {
                                new_line(&mut lines, &mut x);
                            }
                            let line = lines.last_mut().expect("at least one line");
                            line.height = line.height.max(style.size * LINE_FACTOR);
                            line.push(
                                Piece {
                                    x,
                                    width: cw,
                                    face,
                                    text: c.to_string(),
                                    style,
                                },
                                false,
                            );
                            x += cw;
                        }
                    }
                    continue;
                }
                let line = lines.last_mut().expect("at least one line");
                line.height = line.height.max(style.size * LINE_FACTOR);
                let mut first = true;
                x += space;
                for (face, text, w) in measured {
                    line.push(
                        Piece {
                            x,
                            width: w,
                            face,
                            text,
                            style,
                        },
                        first && space > 0.0,
                    );
                    first = false;
                    x += w;
                }
            }
        }
    }
    lines
}

/// Preformatted text: whitespace kept, wrapped between characters.
fn wrap_preformatted(fonts: &mut Fonts, text: &str, style: TextStyle, max_width: f32) -> Vec<Line> {
    let preferred = fonts.role(style.role);
    let mut lines = Vec::new();
    for source_line in text.split('\n') {
        let mut line = Line {
            height: style.size * LINE_FACTOR,
            ..Default::default()
        };
        let mut x = 0.0;
        let expanded = source_line.replace('\t', "    ");
        for (face, run) in fonts.runs(preferred, &expanded) {
            for c in run.chars() {
                let cw = fonts.width(face, &c.to_string(), style.size);
                if x > 0.0 && x + cw > max_width {
                    lines.push(std::mem::replace(
                        &mut line,
                        Line {
                            height: style.size * LINE_FACTOR,
                            ..Default::default()
                        },
                    ));
                    x = 0.0;
                }
                line.push(
                    Piece {
                        x,
                        width: cw,
                        face,
                        text: c.to_string(),
                        style,
                    },
                    false,
                );
                x += cw;
            }
        }
        lines.push(line);
    }
    lines
}

/// Split into alternating whitespace / non-whitespace tokens.
fn tokens(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut current: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let space = c.is_whitespace();
        match current {
            Some(kind) if kind == space => {}
            Some(kind) => {
                out.push((kind, &text[start..i]));
                start = i;
                current = Some(space);
            }
            None => current = Some(space),
        }
    }
    if let Some(kind) = current {
        out.push((kind, &text[start..]));
    }
    out
}

#[derive(Debug, Clone)]
enum Op {
    Text {
        x: f32,
        baseline: f32,
        face: usize,
        size: f32,
        color: Rgb,
        text: String,
    },
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Rgb,
    },
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
        color: Rgb,
    },
    CheckBox {
        x: f32,
        y: f32,
        size: f32,
        checked: bool,
    },
}

enum Marker {
    Text(String),
    CheckBox(bool),
}

struct Layout {
    pages: Vec<Vec<Op>>,
    y: f32,
}

impl Layout {
    fn new() -> Self {
        Layout {
            pages: vec![Vec::new()],
            y: MARGIN_TOP,
        }
    }

    fn ops(&mut self) -> &mut Vec<Op> {
        self.pages.last_mut().expect("at least one page")
    }

    fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.y = MARGIN_TOP;
    }

    /// Start a new page unless `height` fits (or the page is still empty,
    /// where breaking wouldn't help).
    fn ensure(&mut self, height: f32) {
        if self.y + height > PAGE_H - MARGIN_BOTTOM && self.y > MARGIN_TOP + 0.5 {
            self.new_page();
        }
    }

    fn gap(&mut self, height: f32) {
        if self.y > MARGIN_TOP + 0.5 {
            self.y += height;
        }
    }

    fn emit_lines(
        &mut self,
        fonts: &mut Fonts,
        lines: &[Line],
        x0: f32,
        mut marker: Option<(Marker, f32)>,
    ) {
        for line in lines {
            self.ensure(line.height);
            let size = line
                .pieces
                .iter()
                .map(|p| p.style.size)
                .fold(0.0f32, f32::max)
                .max(BODY_SIZE * 0.8);
            let baseline = self.y + (line.height - size) / 2.0 + size * 0.8;
            if let Some((marker, marker_x)) = marker.take() {
                match marker {
                    Marker::Text(text) => {
                        let preferred = fonts.regular;
                        let mut x = marker_x;
                        for (face, run) in fonts.runs(preferred, &text) {
                            let w = fonts.width(face, &run, BODY_SIZE);
                            self.ops().push(Op::Text {
                                x,
                                baseline,
                                face,
                                size: BODY_SIZE,
                                color: TEXT,
                                text: run,
                            });
                            x += w;
                        }
                    }
                    Marker::CheckBox(checked) => {
                        let box_size = BODY_SIZE * 0.8;
                        self.ops().push(Op::CheckBox {
                            x: marker_x,
                            y: baseline - box_size,
                            size: box_size,
                            checked,
                        });
                    }
                }
            }
            for piece in &line.pieces {
                let x = x0 + piece.x;
                self.ops().push(Op::Text {
                    x,
                    baseline,
                    face: piece.face,
                    size: piece.style.size,
                    color: piece.style.color,
                    text: piece.text.clone(),
                });
                if piece.style.strike {
                    let y = baseline - piece.style.size * 0.3;
                    self.ops().push(Op::Line {
                        x1: x,
                        y1: y,
                        x2: x + piece.width,
                        y2: y,
                        width: 0.6,
                        color: piece.style.color,
                    });
                }
            }
            self.y += line.height;
        }
    }

    fn block(&mut self, fonts: &mut Fonts, block: &Block) {
        match block {
            Block::Title(title) => {
                let style = TextStyle {
                    size: 20.0,
                    role: Role::Bold,
                    ..TextStyle::body()
                };
                let lines = wrap(fonts, &[Span::plain(title.as_str())], style, CONTENT_W);
                self.emit_lines(fonts, &lines, MARGIN_X, None);
                self.y += 4.0;
                let y = self.y;
                self.ops().push(Op::Line {
                    x1: MARGIN_X,
                    y1: y,
                    x2: MARGIN_X + CONTENT_W,
                    y2: y,
                    width: 0.8,
                    color: RULE,
                });
                self.y += 10.0;
            }
            Block::Heading { level, spans } => {
                let (size, before) = match level {
                    1 => (15.0, 14.0),
                    2 => (13.0, 11.0),
                    3 => (11.5, 9.0),
                    _ => (BODY_SIZE, 8.0),
                };
                let style = TextStyle {
                    size,
                    role: Role::Bold,
                    ..TextStyle::body()
                };
                let lines = wrap(fonts, spans, style, CONTENT_W);
                self.gap(before);
                // Keep the heading with at least a couple of lines of what follows.
                let height: f32 = lines.iter().map(|l| l.height).sum();
                self.ensure(height + BODY_SIZE * LINE_FACTOR * 2.0);
                self.emit_lines(fonts, &lines, MARGIN_X, None);
                self.y += 3.0;
            }
            Block::Paragraph(spans) => {
                let lines = wrap(fonts, spans, TextStyle::body(), CONTENT_W);
                self.emit_lines(fonts, &lines, MARGIN_X, None);
                self.y += 5.0;
            }
            Block::ListItem {
                depth,
                marker,
                spans,
            } => {
                let marker_x = MARGIN_X + LIST_INDENT * *depth as f32;
                let (marker, text_offset) = match marker {
                    ListMarker::Bullet => (
                        Marker::Text(if *depth % 2 == 0 { "•" } else { "◦" }.to_string()),
                        MARKER_GAP,
                    ),
                    ListMarker::Ordered(n) => {
                        let label = format!("{n}.");
                        let w = fonts.width(fonts.regular, &label, BODY_SIZE);
                        (Marker::Text(label), MARKER_GAP.max(w + 5.0))
                    }
                    ListMarker::Task(done) => (Marker::CheckBox(*done), MARKER_GAP),
                };
                let x0 = marker_x + text_offset;
                let mut lines = wrap(fonts, spans, TextStyle::body(), MARGIN_X + CONTENT_W - x0);
                if lines.is_empty() {
                    lines.push(Line {
                        height: BODY_SIZE * LINE_FACTOR,
                        ..Default::default()
                    });
                }
                self.emit_lines(fonts, &lines, x0, Some((marker, marker_x)));
                self.y += 2.0;
            }
            Block::Quote(spans) => {
                let style = TextStyle {
                    color: MUTED,
                    role: Role::Italic,
                    ..TextStyle::body()
                };
                let x0 = MARGIN_X + 12.0;
                let lines = wrap(fonts, spans, style, CONTENT_W - 12.0);
                let start_page = self.pages.len();
                let start_y = self.y;
                self.emit_lines(fonts, &lines, x0, None);
                // The bar only spans the part on the final page if it broke.
                let top = if self.pages.len() == start_page {
                    start_y
                } else {
                    MARGIN_TOP
                };
                let bottom = self.y;
                self.ops().push(Op::Line {
                    x1: MARGIN_X + 3.0,
                    y1: top,
                    x2: MARGIN_X + 3.0,
                    y2: bottom,
                    width: 2.0,
                    color: RULE,
                });
                self.y += 5.0;
            }
            Block::Code(code) => {
                let style = TextStyle {
                    size: CODE_SIZE,
                    role: Role::Mono,
                    ..TextStyle::body()
                };
                let lines = wrap_preformatted(fonts, code, style, CONTENT_W - 2.0 * CELL_PAD);
                self.y += 2.0;
                for line in &lines {
                    self.ensure(line.height);
                    let (y, h) = (self.y, line.height);
                    self.ops().push(Op::Rect {
                        x: MARGIN_X,
                        y,
                        w: CONTENT_W,
                        h,
                        color: SHADE,
                    });
                    self.emit_lines(fonts, std::slice::from_ref(line), MARGIN_X + CELL_PAD, None);
                }
                self.y += 7.0;
            }
            Block::Rule => {
                self.y += 4.0;
                self.ensure(8.0);
                let y = self.y;
                self.ops().push(Op::Line {
                    x1: MARGIN_X,
                    y1: y,
                    x2: MARGIN_X + CONTENT_W,
                    y2: y,
                    width: 0.6,
                    color: RULE,
                });
                self.y += 8.0;
            }
            Block::Table { header, rows } => self.table(fonts, header, rows),
        }
    }

    fn table(&mut self, fonts: &mut Fonts, header: &[Vec<Span>], rows: &[Vec<Vec<Span>>]) {
        let columns = header
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let col_w = CONTENT_W / columns as f32;
        self.y += 2.0;
        let all_rows = (!header.is_empty())
            .then_some((header, true))
            .into_iter()
            .chain(rows.iter().map(|r| (r.as_slice(), false)));
        for (cells, is_header) in all_rows {
            let base = if is_header {
                TextStyle {
                    role: Role::Bold,
                    ..TextStyle::body()
                }
            } else {
                TextStyle::body()
            };
            let laid: Vec<Vec<Line>> = (0..columns)
                .map(|i| {
                    let spans = cells.get(i).map(Vec::as_slice).unwrap_or(&[]);
                    wrap(fonts, spans, base, col_w - 2.0 * CELL_PAD)
                })
                .collect();
            let row_h = laid
                .iter()
                .map(|lines| lines.iter().map(|l| l.height).sum::<f32>())
                .fold(0.0f32, f32::max)
                + 2.0 * CELL_PAD;
            self.ensure(row_h);
            let top = self.y;
            if is_header {
                self.ops().push(Op::Rect {
                    x: MARGIN_X,
                    y: top,
                    w: CONTENT_W,
                    h: row_h,
                    color: SHADE,
                });
            }
            for (i, lines) in laid.iter().enumerate() {
                self.y = top + CELL_PAD;
                // Cells taller than a page are clipped by the page edge
                // rather than split; summary tables are never that tall.
                let x0 = MARGIN_X + col_w * i as f32 + CELL_PAD;
                for line in lines {
                    let size = line
                        .pieces
                        .iter()
                        .map(|p| p.style.size)
                        .fold(BODY_SIZE, f32::max);
                    let baseline = self.y + (line.height - size) / 2.0 + size * 0.8;
                    for piece in &line.pieces {
                        self.ops().push(Op::Text {
                            x: x0 + piece.x,
                            baseline,
                            face: piece.face,
                            size: piece.style.size,
                            color: piece.style.color,
                            text: piece.text.clone(),
                        });
                    }
                    self.y += line.height;
                }
            }
            let bottom = top + row_h;
            let ops = self.ops();
            ops.push(Op::Line {
                x1: MARGIN_X,
                y1: top,
                x2: MARGIN_X + CONTENT_W,
                y2: top,
                width: 0.5,
                color: RULE,
            });
            ops.push(Op::Line {
                x1: MARGIN_X,
                y1: bottom,
                x2: MARGIN_X + CONTENT_W,
                y2: bottom,
                width: 0.5,
                color: RULE,
            });
            for i in 0..=columns {
                let x = MARGIN_X + col_w * i as f32;
                ops.push(Op::Line {
                    x1: x,
                    y1: top,
                    x2: x,
                    y2: bottom,
                    width: 0.5,
                    color: RULE,
                });
            }
            self.y = bottom;
        }
        self.y += 8.0;
    }

    fn finish(self) -> Vec<Vec<Op>> {
        self.pages
    }
}

// ---- Drawing ---------------------------------------------------------------

fn draw(fonts: &mut Fonts, mut pages: Vec<Vec<Op>>, title: &str) -> Result<Vec<u8>, String> {
    // Footer: "<title> · Page n of N".
    let total = pages.len();
    let short_title: String = {
        let t = title.trim();
        if t.chars().count() > 60 {
            format!("{}…", t.chars().take(59).collect::<String>())
        } else {
            t.to_string()
        }
    };
    for (i, ops) in pages.iter_mut().enumerate() {
        let text = if short_title.is_empty() {
            format!("Page {} of {}", i + 1, total)
        } else {
            format!("{short_title} · Page {} of {}", i + 1, total)
        };
        let runs = fonts.runs(fonts.regular, &text);
        let width: f32 = runs
            .iter()
            .map(|(f, t)| fonts.width(*f, t, FOOTER_SIZE))
            .sum();
        let mut x = (PAGE_W - width) / 2.0;
        let baseline = PAGE_H - MARGIN_BOTTOM / 2.0;
        for (face, run) in runs {
            let w = fonts.width(face, &run, FOOTER_SIZE);
            ops.push(Op::Text {
                x,
                baseline,
                face,
                size: FOOTER_SIZE,
                color: MUTED,
                text: run,
            });
            x += w;
        }
    }

    let mut document = Document::new();
    document.set_metadata(
        Metadata::new()
            .title(title.to_string())
            .creator("Parley".to_string()),
    );
    let settings = PageSettings::from_wh(PAGE_W, PAGE_H).ok_or("Invalid page size")?;
    for ops in &pages {
        let mut page = document.start_page_with(settings.clone());
        let mut surface = page.surface();
        for op in ops {
            draw_op(&mut surface, fonts, op);
        }
        surface.finish();
        page.finish();
    }
    document
        .finish()
        .map_err(|e| format!("Failed to write PDF: {e:?}"))
}

fn fill(color: Rgb) -> Fill {
    Fill {
        paint: rgb::Color::new(color.0, color.1, color.2).into(),
        opacity: NormalizedF32::ONE,
        ..Default::default()
    }
}

fn stroke(color: Rgb, width: f32) -> Stroke {
    Stroke {
        paint: rgb::Color::new(color.0, color.1, color.2).into(),
        width,
        ..Default::default()
    }
}

fn draw_op(surface: &mut Surface<'_>, fonts: &Fonts, op: &Op) {
    match op {
        Op::Text {
            x,
            baseline,
            face,
            size,
            color,
            text,
        } => {
            surface.set_stroke(None);
            surface.set_fill(Some(fill(*color)));
            surface.draw_text(
                Point::from_xy(*x, *baseline),
                fonts.faces[*face].font.clone(),
                *size,
                text,
                false,
                TextDirection::Auto,
            );
        }
        Op::Rect { x, y, w, h, color } => {
            let Some(rect) = Rect::from_xywh(*x, *y, *w, *h) else {
                return;
            };
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            if let Some(path) = pb.finish() {
                surface.set_stroke(None);
                surface.set_fill(Some(fill(*color)));
                surface.draw_path(&path);
            }
        }
        Op::Line {
            x1,
            y1,
            x2,
            y2,
            width,
            color,
        } => {
            let mut pb = PathBuilder::new();
            pb.move_to(*x1, *y1);
            pb.line_to(*x2, *y2);
            if let Some(path) = pb.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(stroke(*color, *width)));
                surface.draw_path(&path);
            }
        }
        Op::CheckBox {
            x,
            y,
            size,
            checked,
        } => {
            let Some(rect) = Rect::from_xywh(*x, *y, *size, *size) else {
                return;
            };
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            if let Some(path) = pb.finish() {
                surface.set_fill(None);
                surface.set_stroke(Some(stroke(TEXT, 0.7)));
                surface.draw_path(&path);
            }
            if *checked {
                let mut pb = PathBuilder::new();
                pb.move_to(x + size * 0.2, y + size * 0.55);
                pb.line_to(x + size * 0.42, y + size * 0.78);
                pb.line_to(x + size * 0.82, y + size * 0.22);
                if let Some(path) = pb.finish() {
                    surface.set_fill(None);
                    surface.set_stroke(Some(stroke(TEXT, 1.0)));
                    surface.draw_path(&path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_alternate_words_and_spaces() {
        assert_eq!(
            tokens("a  bc d"),
            vec![
                (false, "a"),
                (true, "  "),
                (false, "bc"),
                (true, " "),
                (false, "d")
            ]
        );
        assert!(tokens("").is_empty());
    }

    /// Needs at least one installed font; every desktop that can run the
    /// app has one, but skip quietly on a bare CI box rather than fail.
    #[test]
    fn renders_a_pdf_with_wrapping_and_pages() {
        let Ok(mut fonts) = Fonts::load() else {
            eprintln!("no system fonts; skipping");
            return;
        };
        let long = "word ".repeat(400);
        let lines = wrap(
            &mut fonts,
            &[Span::plain(long.as_str())],
            TextStyle::body(),
            200.0,
        );
        assert!(lines.len() > 10);
        assert!(
            lines.iter().all(|l| l.width <= 200.0 + 0.01),
            "a line overflowed"
        );

        let mut blocks = vec![
            Block::Title("Weekly Sync".to_string()),
            Block::Heading {
                level: 1,
                spans: vec![Span::plain("Summary")],
            },
            Block::ListItem {
                depth: 0,
                marker: ListMarker::Task(true),
                spans: vec![Span::plain("done")],
            },
            Block::Table {
                header: vec![vec![Span::bold("Owner")], vec![Span::plain("Task")]],
                rows: vec![vec![vec![Span::plain("Seb")], vec![Span::plain("Ship it")]]],
            },
            Block::Code("fn main() {\n    println!(\"hi\");\n}".to_string()),
        ];
        for i in 0..30 {
            blocks.push(Block::Paragraph(vec![Span::plain(format!(
                "Paragraph {i} {long}"
            ))]));
        }
        let mut layout = Layout::new();
        for block in &blocks {
            layout.block(&mut fonts, block);
        }
        assert!(layout.pages.len() > 5, "long content should paginate");

        let pdf = render_pdf(&blocks, "Weekly Sync").expect("pdf renders");
        assert!(pdf.starts_with(b"%PDF-"));
        if let Ok(path) = std::env::var("PARLEY_PDF_TEST_OUT") {
            std::fs::write(path, &pdf).unwrap();
        }
    }
}
