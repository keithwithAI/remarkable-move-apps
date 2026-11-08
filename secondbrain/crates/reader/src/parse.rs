//! Markdown → [`Doc`] with **source byte spans**, plus an Obsidian `==highlight==` overlay.
//!
//! We parse the body with `pulldown-cmark`'s `into_offset_iter()`, which pairs every event with
//! its byte range in the source. Two Obsidian-isms need care:
//!
//!   1. **YAML frontmatter** (`---` … `---`) is not Markdown; we scan it ourselves for the few
//!      meta fields worth showing, then parse only the body — adding the body's start offset back
//!      onto every span so ranges stay valid against the *original* file (the bytes we rewrite
//!      when a highlight is made).
//!   2. **`==highlight==`** is not CommonMark; pulldown sees the `==` as literal text. We locate
//!      highlight pairs directly in the source (paragraph-bounded, emphasis-agnostic), then overlay
//!      a `highlight` flag onto the runs they cover and drop the `==` markers from display. Spans
//!      keep pointing at the *inner* text, so un-highlighting can find the surrounding markers.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

/// Emphasis/face of a run — orthogonal to [`Inline::highlight`], so a highlighted *italic* word
/// keeps both facts. Maps to an IBM Plex face at layout time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Code,
}

/// A styled run of text. `src` is the byte range in the **original** `.md` that produced `text`
/// (with any `==` markers excluded) — the map from a rendered glyph back to the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Inline {
    pub text: String,
    pub emphasis: Emphasis,
    pub highlight: bool,
    pub src: Range<usize>,
}

/// A block-level element. Inline runs are pre-styled; images/code carry their own spans.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading { level: u8, inlines: Vec<Inline> },
    Paragraph { inlines: Vec<Inline> },
    ListItem { depth: u8, ordered: bool, inlines: Vec<Inline> },
    CodeBlock { text: String, src: Range<usize> },
    Image { url: String, alt: String, src: Range<usize> },
    Rule,
}

/// Curated frontmatter — only the fields the reader surfaces in its header.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub web_link: Option<String>,
    /// `title:` frontmatter — Readwise book exports carry the clean book title here (the filename
    /// appends " by <author>"). Unused by article clippings, whose filename IS the title.
    pub title: Option<String>,
    pub author: Option<String>,
    /// `publisher:` frontmatter — the outlet the Web Clipper detected ("Financial Times", "CIDRAP").
    /// Only consulted by the byline when no human is named; see [`crate::byline::derive_byline`].
    pub publisher: Option<String>,
    pub published: Option<String>,
    /// `created:` frontmatter (aliases `saved:` / `clipped:`) — when the clipping was captured by
    /// the Web Clipper, an ISO-ish timestamp that sorts lexically. The "Recently added" sort key.
    pub created: Option<String>,
    /// Reading status the reader maintains: `Some("reading")` / `Some("read")`; absent = not read.
    pub status: Option<String>,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
}

/// A parsed document: a title (first H1 if any; the app fills from the filename otherwise),
/// curated meta, and the body blocks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Doc {
    pub title: String,
    pub meta: Meta,
    pub blocks: Vec<Block>,
}

/// Parse an Obsidian-clipped `.md` source into a [`Doc`]. All `Inline::src` / block spans are
/// byte offsets into `source` exactly as given (frontmatter included), so they can be fed
/// straight to `highlight::apply_highlight`.
pub fn parse(source: &str) -> Doc {
    let (meta, body_start) = split_frontmatter(source);
    let (highlights, markers) = find_highlights(source);
    let (footnote_refs, defs_start) = find_footnotes(source);

    // Bytes hidden from display: the 2-byte `==` highlight markers and the inline `[^n]` note
    // references (both dropped from runs, exactly like the `==` markers always have been).
    let mut hidden: Vec<Range<usize>> = markers.iter().map(|&m| m..m + 2).collect();
    hidden.extend(footnote_refs);

    // The trailing footnote-definition block (`[^n]: …`) is metadata, not article body — stop the
    // body parse at it so the definitions don't render as literal paragraphs.
    let body_end = defs_start.unwrap_or(source.len()).max(body_start);
    let body = &source[body_start..body_end];
    let parser = Parser::new_ext(body, Options::empty()).into_offset_iter();

    let mut blocks: Vec<Block> = Vec::new();
    let mut title = String::new();
    let mut inlines: Vec<Inline> = Vec::new();

    // Inline emphasis nesting (bold/italic can nest and overlap).
    let mut bold = 0u32;
    let mut italic = 0u32;

    // Which block the current `inlines` belong to.
    let mut pending = Pending::None;
    let mut list_ordered: Vec<bool> = Vec::new();

    // Fenced code block accumulation.
    let mut in_code = false;
    let mut code_buf = String::new();
    let mut code_src = 0..0;

    // Image accumulation (alt text arrives as Text events between Start/End).
    let mut in_image = false;
    let mut img_url = String::new();
    let mut img_alt = String::new();
    let mut img_src = 0..0;

    for (ev, range) in parser {
        let abs = (range.start + body_start)..(range.end + body_start);
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    // Keep an enclosing Item's kind; a bare paragraph opens a Paragraph block.
                    if matches!(pending, Pending::None) {
                        pending = Pending::Paragraph;
                    }
                }
                Tag::Heading { level, .. } => pending = Pending::Heading(hl_level(level)),
                Tag::List(start) => list_ordered.push(start.is_some()),
                Tag::Item => {
                    // An item can open while a PARENT item's text is still pending (a nested list:
                    // only TagEnd::Item flushes) — emit the parent's runs as their own block first,
                    // so its text never merges into the child item.
                    flush(&mut blocks, &pending, &mut inlines);
                    let ordered = *list_ordered.last().unwrap_or(&false);
                    let depth = list_ordered.len().saturating_sub(1) as u8;
                    pending = Pending::Item { depth, ordered };
                }
                Tag::Emphasis => italic += 1,
                Tag::Strong => bold += 1,
                Tag::CodeBlock(_) => {
                    in_code = true;
                    code_buf.clear();
                    code_src = abs.clone();
                }
                Tag::Image { dest_url, .. } => {
                    // Flush any text before an (possibly inline) image, then collect the image.
                    flush(&mut blocks, &pending, &mut inlines);
                    in_image = true;
                    img_url = dest_url.to_string();
                    img_alt.clear();
                    img_src = abs.clone();
                }
                _ => {}
            },
            Event::End(end) => match end {
                TagEnd::Paragraph => {
                    if matches!(pending, Pending::Paragraph) {
                        flush(&mut blocks, &pending, &mut inlines);
                        pending = Pending::None;
                    }
                }
                TagEnd::Heading(_) => {
                    if title.is_empty() {
                        if let Pending::Heading(1) = pending {
                            title = inlines.iter().map(|i| i.text.as_str()).collect();
                        }
                    }
                    flush(&mut blocks, &pending, &mut inlines);
                    pending = Pending::None;
                }
                TagEnd::Item => {
                    flush(&mut blocks, &pending, &mut inlines);
                    pending = Pending::None;
                }
                TagEnd::List(_) => {
                    list_ordered.pop();
                }
                TagEnd::Emphasis => italic = italic.saturating_sub(1),
                TagEnd::Strong => bold = bold.saturating_sub(1),
                TagEnd::CodeBlock => {
                    blocks.push(Block::CodeBlock { text: std::mem::take(&mut code_buf), src: code_src.clone() });
                    in_code = false;
                }
                TagEnd::Image => {
                    blocks.push(Block::Image {
                        url: std::mem::take(&mut img_url),
                        alt: std::mem::take(&mut img_alt),
                        src: img_src.clone(),
                    });
                    in_image = false;
                }
                _ => {}
            },
            Event::Text(_) => {
                if in_image {
                    img_alt.push_str(&source[abs.clone()]);
                } else if in_code {
                    code_buf.push_str(&source[abs.clone()]);
                } else {
                    emit_text(&mut inlines, source, abs, emphasis_of(bold, italic), &highlights, &hidden);
                }
            }
            Event::Code(_) => {
                // Inline `code` span: backticks are outside the event range's text but we keep the
                // event's full span so selection still lands sensibly.
                inlines.push(Inline {
                    text: source[abs.clone()].to_string(),
                    emphasis: Emphasis::Code,
                    highlight: in_ranges(abs.start, &highlights),
                    src: abs,
                });
            }
            Event::SoftBreak | Event::HardBreak => {
                // A wrap point between words; carry the highlight flag so a tint stays continuous.
                inlines.push(Inline {
                    text: " ".to_string(),
                    emphasis: Emphasis::Regular,
                    highlight: in_ranges(abs.start, &highlights),
                    src: abs,
                });
            }
            Event::Rule => blocks.push(Block::Rule),
            _ => {}
        }
    }
    // Flush any trailing content (unterminated block at EOF).
    flush(&mut blocks, &pending, &mut inlines);

    Doc { title, meta, blocks }
}

/// The block kind the current inline buffer belongs to.
enum Pending {
    None,
    Paragraph,
    Heading(u8),
    Item { depth: u8, ordered: bool },
}

/// Move `inlines` into a block of the pending kind, skipping all-whitespace blocks.
fn flush(blocks: &mut Vec<Block>, pending: &Pending, inlines: &mut Vec<Inline>) {
    if inlines.is_empty() {
        return;
    }
    let runs = std::mem::take(inlines);
    if runs.iter().all(|i| i.text.trim().is_empty()) {
        return;
    }
    let block = match pending {
        Pending::Paragraph | Pending::None => Block::Paragraph { inlines: runs },
        Pending::Heading(level) => Block::Heading { level: *level, inlines: runs },
        Pending::Item { depth, ordered } => Block::ListItem { depth: *depth, ordered: *ordered, inlines: runs },
    };
    blocks.push(block);
}

fn hl_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn emphasis_of(bold: u32, italic: u32) -> Emphasis {
    match (bold > 0, italic > 0) {
        (true, true) => Emphasis::BoldItalic,
        (true, false) => Emphasis::Bold,
        (false, true) => Emphasis::Italic,
        (false, false) => Emphasis::Regular,
    }
}

/// Emit `source[abs]` as one or more runs of emphasis `e`, splitting around `hidden` spans (the `==`
/// highlight markers and inline `[^n]` note references, both dropped from display) and flipping
/// `highlight` where the byte position falls inside a highlight span. Because the hidden spans sit on
/// run boundaries, splitting at them keeps every remaining run's `src` contiguous and pointing at
/// real (inner) text.
fn emit_text(
    out: &mut Vec<Inline>,
    source: &str,
    abs: Range<usize>,
    e: Emphasis,
    highlights: &[Range<usize>],
    hidden: &[Range<usize>],
) {
    let seg = &source[abs.clone()];
    let mut run = String::new();
    let mut run_start = abs.start;
    let mut run_hl = false;
    let mut open = false;

    for (off, ch) in seg.char_indices() {
        let pos = abs.start + off;
        if in_ranges(pos, hidden) {
            if open && !run.is_empty() {
                out.push(Inline { text: std::mem::take(&mut run), emphasis: e, highlight: run_hl, src: run_start..pos });
            }
            open = false;
            continue;
        }
        let hl = in_ranges(pos, highlights);
        if !open {
            run_start = pos;
            run_hl = hl;
            open = true;
        } else if hl != run_hl {
            out.push(Inline { text: std::mem::take(&mut run), emphasis: e, highlight: run_hl, src: run_start..pos });
            run_start = pos;
            run_hl = hl;
        }
        run.push(ch);
    }
    if open && !run.is_empty() {
        out.push(Inline { text: run, emphasis: e, highlight: run_hl, src: run_start..abs.end });
    }
}

fn in_ranges(pos: usize, ranges: &[Range<usize>]) -> bool {
    ranges.iter().any(|r| r.start <= pos && pos < r.end)
}

/// Locate `==highlight==` pairs in `source`. Returns the **inner** byte ranges (between the
/// markers) and the byte offset of every `==` marker (each covering `m..m+2`). A pair never
/// crosses a blank line, mirroring Obsidian, so a stray `==` can't run away to the end of the file.
fn find_highlights(source: &str) -> (Vec<Range<usize>>, Vec<usize>) {
    let b = source.as_bytes();
    let n = b.len();
    let mut inner = Vec::new();
    let mut markers = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        if b[i] == b'=' && b[i + 1] == b'=' {
            if let Some(j) = find_close(b, i + 2) {
                if j > i + 2 {
                    inner.push((i + 2)..j);
                    markers.push(i);
                    markers.push(j);
                    i = j + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    (inner, markers)
}

/// Find the next `==` at/after `from`, stopping (no match) at a blank line.
fn find_close(b: &[u8], from: usize) -> Option<usize> {
    let n = b.len();
    let mut j = from;
    while j + 1 < n {
        if b[j] == b'\n' {
            let mut k = j + 1;
            while k < n && matches!(b[k], b' ' | b'\t' | b'\r') {
                k += 1;
            }
            if k >= n || b[k] == b'\n' {
                return None; // blank line → paragraph ended, no highlight here
            }
        }
        if b[j] == b'=' && b[j + 1] == b'=' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Locate Obsidian footnotes (`==text==[^n]` references + a trailing `[^n]: note` definition block).
/// Returns `(ref_spans, defs_start)` where `ref_spans` are the byte ranges of every inline `[^label]`
/// reference in the body (to hide from display) and `defs_start` is the byte offset where the trailing
/// definition block begins (the first line that looks like `[^label]: …`), or `None`. The reader only
/// ever writes definitions in one block at the end of the file, so bounding the body there drops the
/// whole block cleanly.
fn find_footnotes(source: &str) -> (Vec<Range<usize>>, Option<usize>) {
    let defs_start = footnote_defs_start(source);
    let scan_end = defs_start.unwrap_or(source.len());
    let b = source.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0;
    while i + 1 < scan_end {
        if b[i] == b'[' && b[i + 1] == b'^' {
            // Read the label up to the closing `]`, staying on one line.
            let mut j = i + 2;
            while j < scan_end && b[j] != b']' && b[j] != b'\n' {
                j += 1;
            }
            if j < scan_end && b[j] == b']' && j > i + 2 {
                refs.push(i..(j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    (refs, defs_start)
}

/// Byte offset of the first line that is a footnote *definition* (`[^label]: …`), or `None`.
fn footnote_defs_start(source: &str) -> Option<usize> {
    let mut off = 0;
    for line in source.split_inclusive('\n') {
        if is_footnote_def_line(line) {
            return Some(off);
        }
        off += line.len();
    }
    None
}

/// Whether a line is a footnote definition: `[^label]:` (optionally indented).
fn is_footnote_def_line(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("[^") && l[2..].split_once(']').is_some_and(|(label, rest)| !label.is_empty() && rest.starts_with(':'))
}

/// The note attached to the highlight enclosing byte `anchor`, if any: reads the `[^label]` reference
/// immediately after that highlight's closing `==`, then returns `(label, text)` from the matching
/// `[^label]: text` definition. `None` if `anchor` isn't inside a highlight, the highlight carries no
/// `[^label]`, or the definition is missing. Used by the reading view to label the tap-a-highlight menu
/// ("Edit note") and to prefill the note editor.
pub fn note_for_highlight(source: &str, anchor: usize) -> Option<(String, String)> {
    let (inner, _markers) = find_highlights(source);
    let hl = inner.into_iter().find(|r| r.contains(&anchor))?;
    let b = source.as_bytes();
    // The closing `==` sits at `hl.end`; a `[^label]` reference, if present, starts right after it.
    let ref_start = hl.end + 2;
    if ref_start + 1 >= b.len() || b[ref_start] != b'[' || b[ref_start + 1] != b'^' {
        return None;
    }
    let mut j = ref_start + 2;
    while j < b.len() && b[j] != b']' && b[j] != b'\n' {
        j += 1;
    }
    if j >= b.len() || b[j] != b']' || j == ref_start + 2 {
        return None; // no closing `]`, or an empty label
    }
    let label = source.get(ref_start + 2..j)?.to_string();
    let text = footnote_def_text(source, &label)?;
    Some((label, text))
}

/// The text of the `[^label]: …` definition line (whitespace-trimmed), or `None` if absent.
fn footnote_def_text(source: &str, label: &str) -> Option<String> {
    let needle = format!("[^{label}]:");
    source
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(&needle).map(|rest| rest.trim().to_string()))
}

/// Byte offsets of a leading `---`…`---` block: `(yaml_start, yaml_end, body_start)` — `yaml_start`
/// is just after the opening `---` line, `yaml_end` is at the start of the closing `---` line, and
/// `body_start` is just after it. `None` if there's no valid (terminated) frontmatter.
fn frontmatter_span(source: &str) -> Option<(usize, usize, usize)> {
    if !(source.starts_with("---\n") || source.starts_with("---\r\n")) {
        return None;
    }
    let after_open = source.find('\n')? + 1;
    let mut cur = after_open;
    for line in source[after_open..].split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Some((after_open, cur, cur + line.len()));
        }
        cur += line.len();
    }
    None // unterminated → no frontmatter
}

/// Split leading `---`…`---` YAML frontmatter off `source`. Returns the curated [`Meta`] and the
/// byte offset where the Markdown body begins (0 if there's no valid frontmatter).
fn split_frontmatter(source: &str) -> (Meta, usize) {
    match frontmatter_span(source) {
        Some((ys, ye, body_start)) => (parse_yaml(&source[ys..ye]), body_start),
        None => (Meta::default(), 0),
    }
}

/// Parse only the leading YAML frontmatter of `source` into [`Meta`], skipping the Markdown body.
/// Cheap enough to run over every clipping when scanning the vault (title/status/published for the
/// list) without the full block/glyph parse.
pub fn frontmatter(source: &str) -> Meta {
    split_frontmatter(source).0
}

/// Set (or clear) the `status:` field in `source`'s YAML frontmatter, returning the new source.
/// `Some("reading")` / `Some("read")` inserts or replaces the line; `None` removes it (not-read → no
/// field, so Obsidian stays clean). The body — and its byte layout relative to the frontmatter — is
/// otherwise preserved; only the frontmatter block changes (callers re-parse to refresh spans).
pub fn set_status(source: &str, status: Option<&str>) -> String {
    let Some((ys, ye, _)) = frontmatter_span(source) else {
        // No frontmatter block — synthesise a minimal one only when setting a status.
        return match status {
            Some(v) => format!("---\nstatus: {v}\n---\n{source}"),
            None => source.to_string(),
        };
    };
    // Rebuild the YAML lines, dropping any existing status line, then append the new one.
    let mut new_yaml = String::new();
    for line in source[ys..ye].split_inclusive('\n') {
        if !line.trim_start().to_lowercase().starts_with("status:") {
            new_yaml.push_str(line);
        }
    }
    if let Some(v) = status {
        if !new_yaml.is_empty() && !new_yaml.ends_with('\n') {
            new_yaml.push('\n');
        }
        new_yaml.push_str(&format!("status: {v}\n"));
    }
    let mut out = String::with_capacity(source.len() + 16);
    out.push_str(&source[..ys]); // opening `---\n`
    out.push_str(&new_yaml);
    out.push_str(&source[ye..]); // closing `---` + body, byte-for-byte
    out
}

/// Set (or clear) the `tags:` field in `source`'s YAML frontmatter, returning the new source.
/// `tags` is written verbatim as an Obsidian-native block list (`tags:\n  - a\n  - b\n`) so the
/// values stay clickable/filterable in Obsidian; an empty slice removes the field entirely. Unlike
/// [`set_status`] the old value is a *block list*, so we drop the `tags:` line **and** its following
/// `  - item` continuation lines (the list ends at the next non-item line), keeping every other key
/// — including a sibling `category:` block list — and the body byte-for-byte. The caller is
/// responsible for merging in any non-managed tags it wants to preserve before calling.
pub fn set_tags(source: &str, tags: &[&str]) -> String {
    let block = |out: &mut String| {
        out.push_str("tags:\n");
        for t in tags {
            out.push_str(&format!("  - {t}\n"));
        }
    };
    let Some((ys, ye, _)) = frontmatter_span(source) else {
        // No frontmatter block — synthesise a minimal one only when setting tags.
        if tags.is_empty() {
            return source.to_string();
        }
        let mut yaml = String::from("---\n");
        block(&mut yaml);
        yaml.push_str("---\n");
        return format!("{yaml}{source}");
    };
    // Rebuild the YAML lines, dropping the existing `tags:` line and its list items, then append the
    // new block. `in_tags` stays true across the `  - item` continuation lines that follow the key.
    let mut new_yaml = String::new();
    let mut in_tags = false;
    for line in source[ys..ye].split_inclusive('\n') {
        if in_tags && line.trim_start().starts_with("- ") {
            continue; // a tags list item — drop it
        }
        in_tags = false;
        if let Some((key, _)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("tags") {
                in_tags = true; // drop this line + any following `  - item` lines
                continue;
            }
        }
        new_yaml.push_str(line);
    }
    if !tags.is_empty() {
        if !new_yaml.is_empty() && !new_yaml.ends_with('\n') {
            new_yaml.push('\n');
        }
        block(&mut new_yaml);
    }
    let mut out = String::with_capacity(source.len() + 32);
    out.push_str(&source[..ys]); // opening `---\n`
    out.push_str(&new_yaml);
    out.push_str(&source[ye..]); // closing `---` + body, byte-for-byte
    out
}

/// The frontmatter keys the reader keeps; every other key is dropped by [`tidy_frontmatter`].
const TIDY_KEEP: [&str; 10] =
    ["web link", "author", "publisher", "published", "created", "saved", "clipped", "category", "tags", "status"];

/// Rewrite a clipping's YAML frontmatter to a tidy, canonical key set, preserving the **body exactly**.
/// A one-shot vault cleanup (run by `examples/tidy_frontmatter.rs`), not part of the reading path:
///   - keeps only [`TIDY_KEEP`]; drops every other key **and** any kept key that is empty (no scalar
///     value and no list items) — e.g. a bare `published:`,
///   - canonicalizes a **URL**-valued `source`/`url`/`link` to `web link` (deduped if one exists), and
///     normalizes a quoted or miscased key to its canonical spelling (`"web link"` → `web link`),
///   - rewrites an inline `tags: [a, b]` as a `tags:` block list.
/// Lines it doesn't recognize (blank lines, a multi-line quoted value's continuation) are kept as-is —
/// the safety property: never drop what we can't classify. Returns `source` unchanged when there is no
/// standard `---` frontmatter (so `-----`-fenced / no-frontmatter files are left for manual fixing).
pub fn tidy_frontmatter(source: &str) -> String {
    let Some((ys, ye, _)) = frontmatter_span(source) else {
        return source.to_string();
    };
    // Group the YAML into blocks: a top-level `key:` line plus the (indented / list-item / blank)
    // lines that follow it until the next top-level key.
    let is_top_key =
        |line: &str| !line.starts_with([' ', '\t']) && !line.trim_start().starts_with("- ") && line.contains(':');
    let mut groups: Vec<Vec<&str>> = Vec::new();
    for line in source[ys..ye].split_inclusive('\n') {
        if groups.is_empty() || is_top_key(line) {
            groups.push(vec![line]);
        } else {
            groups.last_mut().unwrap().push(line);
        }
    }

    let mut new_yaml = String::new();
    let mut have_web_link = false;
    for group in &groups {
        let head = group[0];
        let Some(colon) = head.find(':') else {
            continue; // a stray non-key line before the first key — drop it
        };
        let raw_key = head[..colon].trim(); // as written, e.g. `"web link"` (quoted) or `Source`
        let key = raw_key.trim_matches('"').to_lowercase();
        let val = head[colon + 1..].trim_end_matches(['\n', '\r']).trim();
        let has_items = group[1..].iter().any(|l| l.trim_start().starts_with("- "));

        // A URL-valued source alias becomes `web link`; everything else keeps its own name.
        let canon: &str = match key.as_str() {
            "source" | "url" | "link" if val.starts_with("http") => "web link",
            k => k,
        };
        if !TIDY_KEEP.contains(&canon) {
            continue; // drop the whole group (key line + its continuation lines)
        }
        if val.is_empty() && !has_items {
            continue; // drop an empty kept key
        }
        // Inline `tags: [a, b]` → block list.
        if canon == "tags" && val.starts_with('[') {
            let items: Vec<String> = val
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(clean)
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                continue;
            }
            new_yaml.push_str("tags:\n");
            for it in items {
                new_yaml.push_str(&format!("  - {it}\n"));
            }
            continue;
        }
        if canon == "web link" {
            if have_web_link {
                continue; // keep the first web link, drop later aliases
            }
            have_web_link = true;
        }
        // Emit the head — normalizing the key to `canon` when it was renamed, quoted, or miscased
        // (`"web link"` → `web link`, `source` → `web link`) — then the continuation lines as-is.
        if raw_key == canon {
            new_yaml.push_str(head);
        } else {
            new_yaml.push_str(canon);
            new_yaml.push_str(&head[colon..]); // ": <value>\n" — value + spacing + newline preserved
        }
        for l in &group[1..] {
            new_yaml.push_str(l);
        }
    }

    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..ys]); // opening `---\n`
    out.push_str(&new_yaml);
    out.push_str(&source[ye..]); // closing `---` + body, byte-for-byte
    out
}

/// A deliberately tiny YAML reader for Obsidian Web Clipper frontmatter: `key: value` scalars and
/// `key:` followed by `  - item` lists. Values are cleaned of quotes and `[[wikilink]]` brackets.
fn parse_yaml(yaml: &str) -> Meta {
    let mut meta = Meta::default();
    let mut list_key: Option<String> = None;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        if let Some(item) = trimmed.strip_prefix("- ") {
            match list_key.as_deref() {
                Some("category") | Some("categories") => meta.categories.push(clean(item)),
                Some("tags") => meta.tags.push(clean(item)),
                // Block-list author (`author:\n  - "[[X]]"`); keep the first so a scalar isn't clobbered.
                Some("author") | Some("authors") if meta.author.is_none() => meta.author = Some(clean(item)),
                Some("publisher") if meta.publisher.is_none() => meta.publisher = Some(clean(item)),
                _ => {}
            }
            continue;
        }
        // A new top-level key resets any active list.
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let value = value.trim();
            if value.is_empty() {
                list_key = Some(key);
                continue;
            }
            list_key = None;
            match key.as_str() {
                "web link" | "link" | "url" | "source" => meta.web_link = Some(clean(value)),
                "title" => meta.title = Some(clean(value)),
                "author" | "authors" => meta.author = Some(clean(value)),
                "publisher" => meta.publisher = Some(clean(value)),
                "published" | "date" => meta.published = Some(clean(value)),
                "created" | "saved" | "clipped" => meta.created = Some(clean(value)),
                "status" => meta.status = Some(clean(value)),
                "category" | "categories" => meta.categories.push(clean(value)),
                "tags" => meta.tags.push(clean(value)),
                _ => {}
            }
        }
    }
    meta
}

/// Strip surrounding quotes and `[[ ]]` wikilink brackets from a frontmatter value.
fn clean(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(s);
    let s = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).unwrap_or(s);
    let s = s.trim();
    let s = s.strip_prefix("[[").and_then(|x| x.strip_suffix("]]")).unwrap_or(s);
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every *visible* run's `text` must equal exactly the source bytes it points at. A synthesized
    /// break separator (a space rendered for a source newline) may map whitespace→whitespace.
    fn assert_spans_roundtrip(source: &str, doc: &Doc) {
        for block in &doc.blocks {
            let inlines = match block {
                Block::Paragraph { inlines } | Block::Heading { inlines, .. } | Block::ListItem { inlines, .. } => inlines,
                _ => continue,
            };
            for run in inlines {
                let slice = &source[run.src.clone()];
                if run.text.trim().is_empty() {
                    assert!(slice.trim().is_empty(), "separator run maps to non-space {slice:?}: {run:?}");
                } else {
                    assert_eq!(slice, run.text, "span mismatch for run {run:?}");
                }
            }
        }
    }

    fn paras(doc: &Doc) -> Vec<&Vec<Inline>> {
        doc.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { inlines } => Some(inlines),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_paragraph_spans_roundtrip() {
        let src = "Hello there, world.";
        let doc = parse(src);
        assert_spans_roundtrip(src, &doc);
        let p = paras(&doc);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0][0].text, "Hello there, world.");
        assert!(!p[0][0].highlight);
    }

    #[test]
    fn frontmatter_is_parsed_and_offsets_body() {
        let src = "---\nweb link: https://example.com/x\nauthor: \"[[Jane Roe]]\"\npublished: 2025-01-06\ncategory:\n  - \"[[diet]]\"\n  - \"[[longevity]]\"\ntags:\n  - read\n---\nBody starts here.";
        let doc = parse(src);
        assert_eq!(doc.meta.web_link.as_deref(), Some("https://example.com/x"));
        assert_eq!(doc.meta.author.as_deref(), Some("Jane Roe"));
        assert_eq!(doc.meta.published.as_deref(), Some("2025-01-06"));
        assert_eq!(doc.meta.categories, vec!["diet", "longevity"]);
        assert_eq!(doc.meta.tags, vec!["read"]);
        // The body run's span must index the ORIGINAL source (past the frontmatter), not the body.
        assert_spans_roundtrip(src, &doc);
        let p = paras(&doc);
        assert_eq!(p[0][0].text, "Body starts here.");
    }

    #[test]
    fn status_frontmatter_is_parsed() {
        let src = "---\nauthor: \"[[A]]\"\nstatus: reading\npublished: 2025-01-06\n---\nBody.";
        assert_eq!(parse(src).meta.status.as_deref(), Some("reading"));
        assert_eq!(frontmatter(src).status.as_deref(), Some("reading"));
        assert_eq!(frontmatter("---\nauthor: X\n---\nBody.").status, None, "no status line → None");
    }

    #[test]
    fn set_status_inserts_replaces_and_removes_keeping_body_intact() {
        let src = "---\nauthor: \"[[A]]\"\npublished: 2025-01-06\n---\nBody stays put.";
        let body = "---\nBody stays put."; // the closing fence onward must never change

        let reading = set_status(src, Some("reading"));
        assert_eq!(frontmatter(&reading).status.as_deref(), Some("reading"));
        assert_eq!(frontmatter(&reading).author.as_deref(), Some("A"), "other fields kept");
        assert!(reading.ends_with(body), "body preserved byte-for-byte");

        let read = set_status(&reading, Some("read"));
        assert_eq!(frontmatter(&read).status.as_deref(), Some("read"));
        assert_eq!(read.matches("status:").count(), 1, "replaced, not duplicated");
        assert!(read.ends_with(body));

        let cleared = set_status(&read, None);
        assert_eq!(frontmatter(&cleared).status, None);
        assert!(!cleared.contains("status:"));
        assert!(cleared.ends_with(body));
    }

    #[test]
    fn set_status_after_write_keeps_spans_valid() {
        // Adding a status line shifts the body; a re-parse must still round-trip spans — the
        // invariant the runtime leans on before the next highlight write.
        let s = set_status("---\nauthor: X\n---\nThe quick brown fox.", Some("read"));
        assert_spans_roundtrip(&s, &parse(&s));
    }

    #[test]
    fn set_status_creates_a_block_only_when_setting() {
        let bare = "Just a body, no frontmatter.";
        let out = set_status(bare, Some("read"));
        assert_eq!(frontmatter(&out).status.as_deref(), Some("read"));
        assert!(out.ends_with(bare));
        assert_eq!(set_status(bare, None), bare, "clearing a bare file is a no-op");
    }

    #[test]
    fn set_tags_inserts_replaces_and_removes_keeping_body_and_other_keys() {
        // Frontmatter with a sibling `category:` BLOCK LIST plus a tags block and scalars — the hard
        // case: dropping the tags list must not disturb the category list or the body.
        let src = "---\nauthor: \"[[A]]\"\ncategory:\n  - \"[[diet]]\"\n  - \"[[longevity]]\"\ntags:\n  - old1\n  - old2\nstatus: reading\n---\nBody stays put.";
        let body = "---\nBody stays put."; // the closing fence onward must never change

        let out = set_tags(src, &["ai", "health", "work"]);
        let m = frontmatter(&out);
        assert_eq!(m.tags, vec!["ai", "health", "work"], "tags replaced");
        assert_eq!(m.categories, vec!["diet", "longevity"], "sibling block list untouched");
        assert_eq!(m.author.as_deref(), Some("A"), "scalar key kept");
        assert_eq!(m.status.as_deref(), Some("reading"), "scalar key kept");
        assert_eq!(out.matches("tags:").count(), 1, "replaced, not duplicated");
        assert!(!out.contains("old1") && !out.contains("old2"), "old items dropped");
        assert!(out.ends_with(body), "body preserved byte-for-byte");

        let cleared = set_tags(&out, &[]);
        assert!(frontmatter(&cleared).tags.is_empty());
        assert!(!cleared.contains("tags:"), "field removed when empty");
        assert_eq!(frontmatter(&cleared).categories, vec!["diet", "longevity"], "category still intact");
        assert!(cleared.ends_with(body));

        let re = set_tags(&cleared, &["life"]);
        assert_eq!(frontmatter(&re).tags, vec!["life"]);
        assert!(re.ends_with(body));
    }

    #[test]
    fn set_tags_writes_the_slice_verbatim_preserving_order() {
        // set_tags is agnostic — it writes exactly what it's handed (the runtime merges in any
        // non-managed tags first). Order is preserved; a kept non-managed tag survives.
        let src = "---\ntags:\n  - keepme\n  - ai\n---\nBody.";
        let out = set_tags(src, &["keepme", "health"]);
        assert_eq!(frontmatter(&out).tags, vec!["keepme", "health"]);
    }

    #[test]
    fn set_tags_creates_a_block_only_when_setting() {
        let bare = "Just a body, no frontmatter.";
        let out = set_tags(bare, &["ai", "work"]);
        assert_eq!(frontmatter(&out).tags, vec!["ai", "work"]);
        assert!(out.ends_with(bare));
        assert_eq!(set_tags(bare, &[]), bare, "clearing a bare file is a no-op");
    }

    #[test]
    fn set_tags_after_write_keeps_spans_valid() {
        // Rewriting the tags block shifts the body; a re-parse must still round-trip spans.
        let s = set_tags("---\nauthor: X\n---\nThe quick brown fox.", &["ai"]);
        assert_spans_roundtrip(&s, &parse(&s));
    }

    #[test]
    fn tidy_drops_cruft_keys_and_their_block_lists_keeping_the_rest() {
        let src = "---\nweb link: https://x.com/a\nauthor: \"[[Jane]]\"\npublished: 2025-01-06\ncategory:\n  - \"[[diet]]\"\ntags:\n  - ai\ncreated: 2025-01-01\ntitle: Redundant Title\ntopics:\n  - foo\n  - bar\nclipped: 2025-01-02\nstable_url: <https://s>\npublisher: ACME\nstatus: reading\n---\nBody stays.";
        let out = tidy_frontmatter(src);
        for k in ["title:", "topics:", "stable_url:"] {
            assert!(!out.contains(k), "{k} should be dropped");
        }
        assert!(!out.contains("- foo") && !out.contains("- bar"), "the dropped topics block's items go too");
        let m = frontmatter(&out);
        // The capture date (`created:` and its aliases) is kept — it's the "Recently added" sort key.
        assert!(out.contains("created: 2025-01-01"), "created is a kept key");
        // The outlet name is a kept key too: the byline falls back to it when nothing names a
        // human, so tidying it away would silently downgrade those rows to a guessed domain.
        assert!(out.contains("publisher: ACME"), "publisher is a kept key");
        assert_eq!(m.publisher.as_deref(), Some("ACME"));
        assert!(out.contains("clipped: 2025-01-02"), "clipped (created alias) is a kept key");
        assert_eq!(m.created.as_deref(), Some("2025-01-02"), "last created-alias wins the parse");
        assert_eq!(m.web_link.as_deref(), Some("https://x.com/a"));
        assert_eq!(m.author.as_deref(), Some("Jane"));
        assert_eq!(m.published.as_deref(), Some("2025-01-06"));
        assert_eq!(m.categories, vec!["diet"]);
        assert_eq!(m.tags, vec!["ai"]);
        assert_eq!(m.status.as_deref(), Some("reading"));
        assert!(out.ends_with("---\nBody stays."), "body preserved byte-for-byte");
    }

    #[test]
    fn tidy_renames_url_source_aliases_but_drops_a_non_url_source() {
        let a = tidy_frontmatter("---\nsource: https://ex.com/p\nauthor: X\n---\nB");
        assert!(a.contains("web link: https://ex.com/p") && !a.contains("source:"), "URL source → web link");
        let c = tidy_frontmatter("---\nurl: http://u\n---\nB");
        assert!(c.contains("web link: http://u"), "url alias → web link");
        let b = tidy_frontmatter("---\nsource: The Review of Economic Studies, Vol. 29\nauthor: X\n---\nB");
        assert!(!b.contains("source:") && !b.contains("web link:"), "a citation source is not a URL → dropped");
        assert_eq!(frontmatter(&b).author.as_deref(), Some("X"));
    }

    #[test]
    fn tidy_normalizes_inline_tags_to_a_block_list() {
        let out = tidy_frontmatter("---\ntags: [clippings, ai]\nauthor: X\n---\nB");
        assert!(out.contains("tags:\n  - clippings\n  - ai"), "inline → block");
        assert_eq!(frontmatter(&out).tags, vec!["clippings", "ai"]);
        let empty = tidy_frontmatter("---\ntags: []\nauthor: X\n---\nB");
        assert!(!empty.contains("tags:"), "empty inline tags dropped");
    }

    #[test]
    fn tidy_drops_empty_kept_keys_and_dedups_web_link() {
        let out = tidy_frontmatter("---\npublished: \ncategory:\nauthor: X\n---\nB");
        assert!(!out.contains("published:") && !out.contains("category:"), "empty kept keys dropped");
        assert_eq!(frontmatter(&out).author.as_deref(), Some("X"));
        let dd = tidy_frontmatter("---\nweb link: https://first\nsource: https://second\n---\nB");
        assert_eq!(dd.matches("web link:").count(), 1, "deduped");
        assert!(dd.contains("https://first") && !dd.contains("https://second"), "first web link wins");
    }

    #[test]
    fn tidy_normalizes_a_quoted_key_to_its_canonical_unquoted_spelling() {
        // The Obsidian clipper wrote `"web link": "URL"` (quoted key *and* value) in ~a third of files;
        // the key must normalize to `web link` — not be dropped as unrecognized (which would lose the URL).
        let src = "---\n\"web link\": \"https://www.angi.com/x\"\nauthor:\n  - \"[[Mizuki Hisaka]]\"\npublished: 2021-11-30\ncategory:\n---\nBody.";
        let out = tidy_frontmatter(src);
        assert!(out.contains("web link: \"https://www.angi.com/x\""), "key unquoted, value kept verbatim");
        assert!(!out.contains("\"web link\":"), "no quoted key remains");
        assert!(out.contains("author:\n  - \"[[Mizuki Hisaka]]\""), "block-list author preserved");
        assert!(!out.contains("category:"), "empty category dropped");
        let m = frontmatter(&out);
        assert_eq!(m.web_link.as_deref(), Some("https://www.angi.com/x"), "URL survives the rename");
        assert_eq!(m.published.as_deref(), Some("2021-11-30"));
        assert!(out.ends_with("---\nBody."), "body preserved byte-for-byte");
    }

    #[test]
    fn tidy_is_idempotent_and_a_noop_without_standard_frontmatter() {
        let src = "---\nweb link: https://x\nsource: https://y\ntitle: T\ntags: [a]\ncreated: 1\n---\nBody.";
        let once = tidy_frontmatter(src);
        assert_eq!(once, tidy_frontmatter(&once), "running tidy twice changes nothing");
        assert_eq!(tidy_frontmatter("Just a body.\nNo fm."), "Just a body.\nNo fm.", "no frontmatter → no-op");
        let arrow = "-----\ntitle: X\n-----\nBody.";
        assert_eq!(tidy_frontmatter(arrow), arrow, "non-standard ----- fences → no-op (hand-fixed)");
    }

    #[test]
    fn tidy_preserves_block_lists_and_spans_roundtrip() {
        let src = "---\nauthor: \"[[A]]\"\ncategory:\n  - \"[[diet]]\"\n  - \"[[longevity]]\"\ntags:\n  - ai\n  - health\nstatus: read\ncreated: 2025-01-01\n---\nThe quick brown fox.";
        let out = tidy_frontmatter(src);
        assert!(out.ends_with("---\nThe quick brown fox."), "body preserved");
        assert!(out.contains("created: 2025-01-01"), "the capture date is kept");
        let m = frontmatter(&out);
        assert_eq!(m.created.as_deref(), Some("2025-01-01"));
        assert_eq!(m.categories, vec!["diet", "longevity"], "kept block list intact");
        assert_eq!(m.tags, vec!["ai", "health"]);
        assert_spans_roundtrip(&out, &parse(&out));
    }

    #[test]
    fn highlight_markers_dropped_flag_set_and_span_points_at_inner() {
        let src = "a ==bold claim== z";
        let doc = parse(src);
        assert_spans_roundtrip(src, &doc);
        let p = &paras(&doc)[0];
        // Runs: "a " (plain), "bold claim" (highlight), " z" (plain).
        let hl: Vec<_> = p.iter().filter(|r| r.highlight).collect();
        assert_eq!(hl.len(), 1);
        assert_eq!(hl[0].text, "bold claim");
        // The `==` markers must sit immediately around the inner span in the source.
        assert_eq!(&src[hl[0].src.start - 2..hl[0].src.start], "==");
        assert_eq!(&src[hl[0].src.end..hl[0].src.end + 2], "==");
        // No run's visible text contains `==`.
        assert!(p.iter().all(|r| !r.text.contains("==")));
    }

    #[test]
    fn highlight_spans_inline_emphasis() {
        // Mirrors the real New Yorker clipping: a highlight wrapping an *italic* word.
        let src = "==Group 1 foods use *NOVA* rules== today";
        let doc = parse(src);
        assert_spans_roundtrip(src, &doc);
        let p = &paras(&doc)[0];
        let italic_hl = p.iter().find(|r| r.emphasis == Emphasis::Italic).unwrap();
        assert_eq!(italic_hl.text, "NOVA");
        assert!(italic_hl.highlight, "italic word inside == must stay highlighted");
        // Everything up to "today" is highlighted; "today" is not.
        let today = p.iter().find(|r| r.text.contains("today")).unwrap();
        assert!(!today.highlight);
    }

    #[test]
    fn highlight_ending_midparagraph_then_plain_tail() {
        // Line-45 shape: `==…==` closes, then ordinary text continues in the same paragraph.
        let src = "==the rubric== (did not account for farms) and more.";
        let doc = parse(src);
        assert_spans_roundtrip(src, &doc);
        let p = &paras(&doc)[0];
        assert_eq!(p.iter().filter(|r| r.highlight).map(|r| r.text.as_str()).collect::<String>(), "the rubric");
        assert!(p.iter().any(|r| !r.highlight && r.text.contains("did not account")));
    }

    #[test]
    fn unbalanced_marker_does_not_highlight_rest() {
        let src = "one == two three\n\nfour five";
        let doc = parse(src);
        // A lone `==` with no partner in the paragraph must not turn anything into a highlight.
        assert!(doc.blocks.iter().all(|b| match b {
            Block::Paragraph { inlines } => inlines.iter().all(|r| !r.highlight),
            _ => true,
        }));
    }

    #[test]
    fn footnote_reference_is_hidden_and_definition_excluded() {
        let src = "The ==claim==[^1] stands firm.\n\n[^1]: my private note\n";
        let doc = parse(src);
        assert_spans_roundtrip(src, &doc);
        let text: String = doc
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Paragraph { inlines } => Some(inlines.iter().map(|r| r.text.as_str()).collect::<String>()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        // The `[^1]` marker never renders, and the definition line is not part of the body.
        assert!(!text.contains("[^1]"), "footnote ref must be hidden, got {text:?}");
        assert!(!text.contains("my private note"), "definition must be excluded from the body");
        assert!(text.contains("claim") && text.contains("stands firm"));
        // The highlight the note hangs off is still highlighted.
        let hl: String = paras(&doc)[0].iter().filter(|r| r.highlight).map(|r| r.text.as_str()).collect();
        assert_eq!(hl, "claim");
    }

    #[test]
    fn footnote_defs_start_finds_the_block_and_ignores_body_brackets() {
        // A `[^x]` inside body text is a reference, not the definition block.
        let src = "see [^2] here\n\ntext\n\n[^2]: note two\n[^5]: note five\n";
        let (refs, defs) = find_footnotes(src);
        assert_eq!(defs, Some(src.find("[^2]: ").unwrap()));
        // Only the inline `[^2]` reference before the block is collected.
        assert_eq!(refs.len(), 1);
        assert_eq!(&src[refs[0].clone()], "[^2]");
    }

    #[test]
    fn headings_lists_and_title() {
        let src = "# The Title\n\nIntro para.\n\n- first\n- second\n\n## Section";
        let doc = parse(src);
        assert_eq!(doc.title, "The Title");
        let heads: Vec<_> = doc.blocks.iter().filter(|b| matches!(b, Block::Heading { .. })).collect();
        assert_eq!(heads.len(), 2);
        let items: Vec<_> = doc.blocks.iter().filter(|b| matches!(b, Block::ListItem { .. })).collect();
        assert_eq!(items.len(), 2);
        if let Block::ListItem { inlines, ordered, .. } = items[0] {
            assert!(!ordered);
            assert_eq!(inlines[0].text, "first");
        }
        assert_spans_roundtrip(src, &doc);
    }

    #[test]
    fn image_becomes_block_with_url_and_alt() {
        let src = "Before.\n\n![a cat](https://img.example/cat.png)\n\nAfter.";
        let doc = parse(src);
        let img = doc.blocks.iter().find_map(|b| match b {
            Block::Image { url, alt, .. } => Some((url.as_str(), alt.as_str())),
            _ => None,
        });
        assert_eq!(img, Some(("https://img.example/cat.png", "a cat")));
        // Reading order preserved: Before paragraph, image, After paragraph.
        assert!(matches!(doc.blocks.first(), Some(Block::Paragraph { .. })));
        assert!(matches!(doc.blocks.last(), Some(Block::Paragraph { .. })));
    }

    #[test]
    fn bold_and_italic_faces() {
        let src = "a **strong** and *soft* end";
        let doc = parse(src);
        let p = &paras(&doc)[0];
        assert!(p.iter().any(|r| r.text == "strong" && r.emphasis == Emphasis::Bold));
        assert!(p.iter().any(|r| r.text == "soft" && r.emphasis == Emphasis::Italic));
        assert_spans_roundtrip(src, &doc);
    }

    #[test]
    fn soft_break_becomes_a_space_mapping_to_the_newline() {
        // Web clippings often hard-wrap a paragraph across lines; each wrap renders as a space but
        // its span points at the source newline (so a selection crossing it still maps correctly).
        let src = "first line\nsecond line";
        let doc = parse(src);
        let p = &paras(&doc)[0];
        let text: String = p.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "first line second line");
        let sep = p.iter().find(|r| r.text == " ").unwrap();
        assert_eq!(&src[sep.src.clone()], "\n");
        assert_spans_roundtrip(src, &doc);
    }

    #[test]
    fn code_block_captured() {
        let src = "text\n\n```\nlet x = 1;\n```\n\nmore";
        let doc = parse(src);
        let code = doc.blocks.iter().find_map(|b| match b {
            Block::CodeBlock { text, .. } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(code, Some("let x = 1;\n"));
    }
}
