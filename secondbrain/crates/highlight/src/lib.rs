//! Obsidian `==highlight==` insertion, ported from `keithwithAI/reading-highlighter`
//! (`src/main.ts`, MIT).
//!
//! Key difference from the Obsidian plugin: on the reMarkable we already know the exact
//! **source byte range** of the pen selection (from the renderer's glyph→source map), so we
//! do **not** need the plugin's rendered-text→source search (`findMatchWithLinks`). What we
//! reuse is the part that makes the resulting markdown correct:
//!   1. extend the range across inline delimiters (`**`, `_`, `` ` ``, …) so syntax stays valid;
//!   2. wrap each paragraph's trimmed content in `==…==`, never crossing a blank line;
//!   3. stay idempotent (don't double-wrap already-highlighted text).
//!
//! Keeping identical behaviour to the plugin means highlights made on the device are
//! byte-identical to ones made in Obsidian → clean Git merges.

use std::ops::Range;
use thiserror::Error;

/// Inline formatting delimiters, longest first so `***` isn't matched as `*`
/// (mirrors `inlineDelimiters` in the plugin).
const INLINE_DELIMITERS: &[&str] = &["***", "___", "**", "__", "~~", "*", "_", "`"];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HighlightError {
    #[error("range {start}..{end} is out of bounds for source of len {len}")]
    OutOfRange { start: usize, end: usize, len: usize },
    #[error("range {0}..{1} does not fall on UTF-8 char boundaries")]
    NotCharBoundary(usize, usize),
    #[error("selection is empty or whitespace-only")]
    EmptySelection,
}

/// If the selection sits *inside* an inline span but didn't include the delimiters
/// (e.g. selecting `bold` inside `**bold**`), extend it to include them so the
/// emitted markdown stays valid. Returns the possibly-widened `(start, end)`.
pub fn extend_across_delimiters(source: &str, start: usize, end: usize) -> (usize, usize) {
    let before = &source[..start];
    let after = &source[end..];
    for d in INLINE_DELIMITERS {
        if before.ends_with(d) && after.starts_with(d) {
            return (start - d.len(), end + d.len());
        }
    }
    (start, end)
}

/// Wrap each paragraph's trimmed content in `==…==`. A paragraph is a run of text with no
/// fully-blank line between its lines; `==` never spans a blank line. All original bytes
/// (whitespace, blank lines, internal newlines) are preserved exactly. Idempotent: a
/// paragraph already wrapped in `==…==` is left unchanged.
pub fn add_highlights_by_paragraph(text: &str) -> String {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n + 8);
    let mut pos = 0;

    while pos < n {
        // Emit leading blank/whitespace verbatim up to the first non-whitespace byte.
        let mut s = pos;
        while s < n && matches!(b[s], b' ' | b'\t' | b'\n' | b'\r') {
            s += 1;
        }
        out.push_str(&text[pos..s]);
        if s >= n {
            break;
        }

        // Find paragraph end: a blank line (`\n` then optional spaces/tabs then `\n`) or EOF.
        // Only ASCII bytes are inspected, so `e` always lands on a char boundary.
        let mut e = s;
        while e < n {
            if b[e] == b'\n' {
                let mut k = e + 1;
                while k < n && matches!(b[k], b' ' | b'\t' | b'\r') {
                    k += 1;
                }
                if k >= n || b[k] == b'\n' {
                    break; // next line is blank → paragraph ends here
                }
            }
            e += 1;
        }

        let para = &text[s..e];
        let trimmed = para.trim();
        let already = trimmed.len() >= 4 && trimmed.starts_with("==") && trimmed.ends_with("==");
        if trimmed.is_empty() || already {
            out.push_str(para);
        } else {
            let lead = para.len() - para.trim_start().len();
            let core_start = s + lead;
            let core_end = core_start + trimmed.len();
            out.push_str(&text[s..core_start]); // leading ws (within paragraph)
            out.push_str("==");
            out.push_str(&text[core_start..core_end]);
            out.push_str("==");
            out.push_str(&text[core_end..e]); // trailing ws (within paragraph)
        }
        pos = e;
    }
    out
}

/// Apply a highlight to `source` over the byte range `start..end` (from hit-testing).
/// Extends across delimiters, wraps per paragraph, and splices back — returning the new
/// document. All bytes outside the (possibly-extended) range are preserved.
pub fn apply_highlight(source: &str, start: usize, end: usize) -> Result<String, HighlightError> {
    if start > end || end > source.len() {
        return Err(HighlightError::OutOfRange { start, end, len: source.len() });
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err(HighlightError::NotCharBoundary(start, end));
    }
    let (a, b) = extend_across_delimiters(source, start, end);
    let selected = &source[a..b];
    if selected.trim().is_empty() {
        return Err(HighlightError::EmptySelection);
    }
    let wrapped = add_highlights_by_paragraph(selected);
    let mut out = String::with_capacity(source.len() + wrapped.len() - selected.len());
    out.push_str(&source[..a]);
    out.push_str(&wrapped);
    out.push_str(&source[b..]);
    Ok(out)
}

/// Remove the `==…==` highlight enclosing the byte range `start..end` (the *inner* text range, as
/// the reader stores it). Strips the nearest `==` marker at/before `start` and at/after `end`.
/// If no enclosing markers are found the source is returned unchanged (safe no-op), so this is
/// the inverse of [`apply_highlight`] for a single highlight.
pub fn remove_highlight(source: &str, start: usize, end: usize) -> Result<String, HighlightError> {
    if start > end || end > source.len() {
        return Err(HighlightError::OutOfRange { start, end, len: source.len() });
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err(HighlightError::NotCharBoundary(start, end));
    }
    let b = source.as_bytes();
    let (Some(open), Some(close)) = (marker_at_or_before(b, start), marker_at_or_after(b, end)) else {
        return Ok(source.to_string());
    };
    // open < close and both are 2-byte `==`; splice them out, keeping everything between.
    let mut out = String::with_capacity(source.len().saturating_sub(4));
    out.push_str(&source[..open]);
    out.push_str(&source[open + 2..close]);
    out.push_str(&source[close + 2..]);
    Ok(out)
}

/// Byte offset of the last `==` at or before `pos` (scanning back), or `None`.
fn marker_at_or_before(b: &[u8], pos: usize) -> Option<usize> {
    let start = pos.min(b.len().saturating_sub(2));
    (0..=start).rev().find(|&i| b[i] == b'=' && b[i + 1] == b'=')
}

/// Byte offset of the first `==` at or after `pos` (scanning forward), or `None`.
fn marker_at_or_after(b: &[u8], pos: usize) -> Option<usize> {
    if b.len() < 2 {
        return None;
    }
    (pos..=b.len() - 2).find(|&i| b[i] == b'=' && b[i + 1] == b'=')
}

// ---------------------------------------------------------------------------
// Notes on highlights (Obsidian inline footnotes)
//
// A note is attached to a highlight as `==text==[^n]` with a matching `[^n]: note` definition
// grouped in a block at the end of the file. These helpers locate the `==…==` a note should hang
// off and splice the footnote in, staying byte-exact / Obsidian-compatible like the rest of this
// crate so device edits merge cleanly with edits made in Obsidian.
// ---------------------------------------------------------------------------

/// The `(open, close)` byte offsets of every `==…==` marker pair in `source` (each covers `m..m+2`).
/// A pair never crosses a blank line, mirroring [`find_close`] in the reader's parser.
fn highlight_pairs(source: &str) -> Vec<(usize, usize)> {
    let b = source.as_bytes();
    let n = b.len();
    let mut pairs = Vec::new();
    let mut i = 0;
    while i + 1 < n {
        if b[i] == b'=' && b[i + 1] == b'=' {
            if let Some(j) = find_close(b, i + 2) {
                if j > i + 2 {
                    pairs.push((i, j));
                    i = j + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    pairs
}

/// Find the next `==` at/after `from`, stopping (no match) at a blank line — so a stray `==` can't
/// pair across a paragraph break. Mirrors the reader parser's `find_close`.
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
                return None;
            }
        }
        if b[j] == b'=' && b[j + 1] == b'=' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Inner byte range (between the markers) of the first `==…==` highlight at or after `from`, or
/// `None`. Used by the runtime to locate a freshly-inserted highlight in the rewritten source so a
/// note can be hung off it.
pub fn first_highlight_after(source: &str, from: usize) -> Option<Range<usize>> {
    highlight_pairs(source)
        .into_iter()
        .find(|&(open, _)| open >= from)
        .map(|(open, close)| (open + 2)..close)
}

/// The next free footnote number: `max(existing [^<int>]) + 1`, or `1` if there are none.
fn next_footnote_number(source: &str) -> u32 {
    let b = source.as_bytes();
    let n = b.len();
    let mut max = 0u32;
    let mut i = 0;
    while i + 2 < n {
        if b[i] == b'[' && b[i + 1] == b'^' {
            let mut k = i + 2;
            let mut val: u32 = 0;
            let mut any = false;
            while k < n && b[k].is_ascii_digit() {
                val = val.saturating_mul(10).saturating_add((b[k] - b'0') as u32);
                any = true;
                k += 1;
            }
            if any && k < n && b[k] == b']' {
                max = max.max(val);
                i = k + 1;
                continue;
            }
        }
        i += 1;
    }
    max + 1
}

/// Attach a note to the highlight enclosing `anchor` (a byte offset inside its inner text): insert
/// `[^n]` immediately after that highlight's closing `==`, and append a `[^n]: {text}` definition to
/// the footnote block at the end of the file (a blank line separates it from the body; consecutive
/// definitions are grouped without blank lines). `n` is the next free footnote number. Newlines in
/// `text` are flattened to spaces (a footnote definition is one line). If no highlight encloses
/// `anchor`, the source is returned unchanged (safe no-op).
pub fn add_note(source: &str, anchor: usize, text: &str) -> Result<String, HighlightError> {
    if anchor > source.len() {
        return Err(HighlightError::OutOfRange { start: anchor, end: anchor, len: source.len() });
    }
    // Locate the highlight whose inner text contains `anchor`; `close` is its closing `==` offset.
    let Some((_, close)) = highlight_pairs(source)
        .into_iter()
        .find(|&(open, close)| open + 2 <= anchor && anchor < close)
    else {
        return Ok(source.to_string());
    };
    let n = next_footnote_number(source);
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();

    // Insert the `[^n]` reference just after the closing `==` (at `close + 2`).
    let ref_at = close + 2;
    let marker = format!("[^{n}]");
    let mut out = String::with_capacity(source.len() + marker.len() + flat.len() + 16);
    out.push_str(&source[..ref_at]);
    out.push_str(&marker);
    out.push_str(&source[ref_at..]);

    // Append the definition to the trailing footnote block.
    let last_line_is_def = out
        .trim_end_matches('\n')
        .lines()
        .last()
        .map(is_footnote_def_line)
        .unwrap_or(false);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !last_line_is_def {
        out.push('\n'); // blank line separating the footnote block from the body
    }
    out.push_str(&format!("[^{n}]: {flat}\n"));
    Ok(out)
}

/// Whether a line is a footnote *definition* (`[^label]: …`).
fn is_footnote_def_line(line: &str) -> bool {
    let l = line.trim_start();
    l.starts_with("[^") && l[2..].split_once(']').is_some_and(|(_, rest)| rest.starts_with(':'))
}

/// The `[^label]` reference label immediately after the highlight closing `==` at `close` (marker
/// `close..close+2`), or `None` if there's no reference there.
fn note_label_after_close(source: &str, close: usize) -> Option<String> {
    let b = source.as_bytes();
    let ref_at = close + 2;
    if ref_at + 1 >= b.len() || b[ref_at] != b'[' || b[ref_at + 1] != b'^' {
        return None;
    }
    let mut j = ref_at + 2;
    while j < b.len() && b[j] != b']' && b[j] != b'\n' {
        j += 1;
    }
    if j >= b.len() || b[j] != b']' || j == ref_at + 2 {
        return None;
    }
    Some(source[ref_at + 2..j].to_string())
}

/// Remove the `[^label]: …` definition line (with its trailing newline) from `source`, keeping any
/// leading indentation on lines it doesn't touch.
fn remove_footnote_def_line(source: &str, label: &str) -> String {
    let needle = format!("[^{label}]:");
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        if line.trim_start().starts_with(&needle) {
            continue; // drop this definition line
        }
        out.push_str(line);
    }
    out
}

/// Replace the text of the `[^label]: …` definition in place (the reference stays). `text`'s newlines
/// are flattened to spaces (a definition is one line); leading indentation is preserved.
fn replace_footnote_def(source: &str, label: &str, text: &str) -> String {
    let needle = format!("[^{label}]:");
    let flat = text.replace(['\n', '\r'], " ");
    let flat = flat.trim();
    let mut out = String::with_capacity(source.len() + flat.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with(&needle) {
            let nl = if line.ends_with('\n') { "\n" } else { "" };
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str(&format!("[^{label}]: {flat}{nl}"));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Remove a `[^label]` reference immediately after the highlight closing `==` at `close` **and** its
/// `[^label]: …` definition line. Returns `(new_source, removed)`; unchanged + `false` if there's no
/// reference right after the close. Collapses any trailing blank lines left when the footnote block
/// empties (trailing blank lines at EOF are insignificant in Markdown).
fn strip_note_after_close(source: &str, close: usize) -> (String, bool) {
    let Some(label) = note_label_after_close(source, close) else {
        return (source.to_string(), false);
    };
    let ref_at = close + 2;
    let ref_end = ref_at + 2 + label.len() + 1; // `[^` + label + `]`
    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..ref_at]);
    out.push_str(&source[ref_end..]);
    let mut out = remove_footnote_def_line(&out, &label);
    while out.ends_with("\n\n") {
        out.pop();
    }
    (out, true)
}

/// Attach a note to the highlight enclosing `anchor`, or update/clear an existing one. If the highlight
/// already carries a `[^label]`, its definition text is *replaced* (or the reference + definition
/// dropped when `text` is empty); otherwise this behaves like [`add_note`]. A no-op when no highlight
/// encloses `anchor`, or when clearing a highlight that has no note. Byte-exact / Obsidian-compatible.
pub fn upsert_note(source: &str, anchor: usize, text: &str) -> Result<String, HighlightError> {
    if anchor > source.len() {
        return Err(HighlightError::OutOfRange { start: anchor, end: anchor, len: source.len() });
    }
    let Some((_, close)) = highlight_pairs(source)
        .into_iter()
        .find(|&(open, close)| open + 2 <= anchor && anchor < close)
    else {
        return Ok(source.to_string());
    };
    match note_label_after_close(source, close) {
        None if text.trim().is_empty() => Ok(source.to_string()), // nothing to write
        None => add_note(source, anchor, text),
        Some(_) if text.trim().is_empty() => Ok(strip_note_after_close(source, close).0),
        Some(label) => Ok(replace_footnote_def(source, &label, text)),
    }
}

/// Remove the `==…==` highlight enclosing the inner byte range `start..end` **and** any note it carries
/// (its `[^label]` reference + `[^label]: …` definition). The note is stripped first (its bytes all sit
/// after the closing `==`, so `start`/`end` stay valid), then the markers. The inverse of
/// add-highlight-then-note; used by the tap-a-highlight Delete action.
pub fn remove_highlight_and_note(source: &str, start: usize, end: usize) -> Result<String, HighlightError> {
    if start > end || end > source.len() {
        return Err(HighlightError::OutOfRange { start, end, len: source.len() });
    }
    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Err(HighlightError::NotCharBoundary(start, end));
    }
    let stripped = match marker_at_or_after(source.as_bytes(), end) {
        Some(close) => strip_note_after_close(source, close).0,
        None => source.to_string(),
    };
    remove_highlight(&stripped, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hl(src: &str, range: std::ops::Range<usize>) -> String {
        apply_highlight(src, range.start, range.end).unwrap()
    }

    #[test]
    fn wraps_a_single_word() {
        assert_eq!(add_highlights_by_paragraph("hello"), "==hello==");
    }

    #[test]
    fn preserves_surrounding_whitespace() {
        assert_eq!(add_highlights_by_paragraph("  hi  "), "  ==hi==  ");
        assert_eq!(add_highlights_by_paragraph("\n\thi\n"), "\n\t==hi==\n");
    }

    #[test]
    fn each_paragraph_wrapped_separately_blank_line_preserved() {
        assert_eq!(add_highlights_by_paragraph("a\n\nb"), "==a==\n\n==b==");
        // exact blank-line count preserved
        assert_eq!(add_highlights_by_paragraph("a\n\n\nb"), "==a==\n\n\n==b==");
    }

    #[test]
    fn soft_wrapped_lines_stay_one_paragraph() {
        assert_eq!(add_highlights_by_paragraph("one\ntwo"), "==one\ntwo==");
    }

    #[test]
    fn idempotent_on_already_highlighted() {
        assert_eq!(add_highlights_by_paragraph("==hi=="), "==hi==");
        // re-applying over the same text does not nest
        let once = hl("hi", 0..2);
        assert_eq!(once, "==hi==");
        assert_eq!(add_highlights_by_paragraph(&once), "==hi==");
    }

    #[test]
    fn extends_across_inline_bold() {
        let src = "a **bold** b";
        let start = src.find("bold").unwrap();
        let end = start + "bold".len();
        // selecting just `bold` should swallow the `**` so markdown stays valid
        assert_eq!(hl(src, start..end), "a ==**bold**== b");
    }

    #[test]
    fn extends_across_code_and_triple() {
        assert_eq!(hl("x `c` y", 3..4), "x ==`c`== y");
        let s = "p ***e*** q";
        let i = s.find('e').unwrap();
        assert_eq!(hl(s, i..i + 1), "p ==***e***== q");
    }

    #[test]
    fn unicode_is_safe() {
        assert_eq!(add_highlights_by_paragraph("café"), "==café==");
        let s = "go café now";
        let i = s.find("café").unwrap();
        assert_eq!(hl(s, i..i + "café".len()), "go ==café== now");
    }

    #[test]
    fn empty_selection_errors() {
        assert_eq!(apply_highlight("   ", 0, 3), Err(HighlightError::EmptySelection));
    }

    #[test]
    fn out_of_range_and_boundary_errors() {
        assert!(matches!(apply_highlight("hi", 0, 5), Err(HighlightError::OutOfRange { .. })));
        // byte 1 is mid-char in a 2-byte é at bytes 0..2
        assert!(matches!(apply_highlight("é", 0, 1), Err(HighlightError::NotCharBoundary(..))));
    }

    #[test]
    fn highlights_a_phrase_inside_a_sentence() {
        let s = "The quick brown fox.";
        let i = s.find("quick brown").unwrap();
        assert_eq!(hl(s, i..i + "quick brown".len()), "The ==quick brown== fox.");
    }

    #[test]
    fn remove_strips_the_enclosing_markers() {
        let s = "a ==bold claim== z";
        let inner = s.find("bold claim").unwrap();
        let out = remove_highlight(s, inner, inner + "bold claim".len()).unwrap();
        assert_eq!(out, "a bold claim z");
    }

    #[test]
    fn apply_then_remove_round_trips() {
        let s = "The quick brown fox.";
        let i = s.find("quick brown").unwrap();
        let hled = apply_highlight(s, i, i + "quick brown".len()).unwrap();
        assert_eq!(hled, "The ==quick brown== fox.");
        // The reader would re-parse and hand back the inner range; here it's shifted by the `==`.
        let ni = hled.find("quick brown").unwrap();
        let back = remove_highlight(&hled, ni, ni + "quick brown".len()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn remove_without_markers_is_a_noop() {
        let s = "no markers here at all";
        let i = s.find("markers").unwrap();
        assert_eq!(remove_highlight(s, i, i + 7).unwrap(), s);
    }

    // ---- notes on highlights -------------------------------------------------

    /// Byte inside the first highlight's inner text (what the runtime passes as `anchor`).
    fn anchor_of(src: &str) -> usize {
        first_highlight_after(src, 0).unwrap().start
    }

    #[test]
    fn first_highlight_after_finds_inner_range() {
        let s = "a ==bold claim== z";
        assert_eq!(first_highlight_after(s, 0), Some(4..14)); // "bold claim"
        assert_eq!(&s[4..14], "bold claim");
        // nothing after the pair
        assert_eq!(first_highlight_after(s, 16), None);
    }

    #[test]
    fn next_number_increments_over_existing_refs() {
        assert_eq!(next_footnote_number("no notes here"), 1);
        assert_eq!(next_footnote_number("x[^1] y\n[^1]: a"), 2);
        assert_eq!(next_footnote_number("x[^3][^1][^2]\n[^3]: c"), 4);
    }

    #[test]
    fn add_note_places_ref_and_definition() {
        let s = "The ==claim== stands.\n";
        let out = add_note(s, anchor_of(s), "why it matters").unwrap();
        assert_eq!(out, "The ==claim==[^1] stands.\n\n[^1]: why it matters\n");
    }

    #[test]
    fn add_note_appends_second_definition_without_blank_between() {
        let s = "The ==claim== stands.\n";
        let one = add_note(s, anchor_of(s), "first").unwrap();
        // A second highlight + note anywhere after the first.
        let two_src = one.replace("stands", "==stands==");
        let a2 = highlight_pairs(&two_src)
            .into_iter()
            .find(|&(o, _)| o > two_src.find("claim").unwrap())
            .map(|(o, _)| o + 2)
            .unwrap();
        let two = add_note(&two_src, a2, "second").unwrap();
        assert!(two.contains("==claim==[^1]"));
        assert!(two.contains("==stands==[^2]"));
        // definitions grouped, no blank line between them
        assert!(two.ends_with("[^1]: first\n[^2]: second\n"));
    }

    #[test]
    fn add_note_flattens_newlines_and_trims() {
        let s = "==hi==\n";
        let out = add_note(s, anchor_of(s), "  a\nb  ").unwrap();
        assert!(out.ends_with("[^1]: a b\n"));
    }

    #[test]
    fn add_note_unicode_anchor_is_safe() {
        let s = "café ==déjà vu== ici\n";
        let out = add_note(s, anchor_of(s), "noté").unwrap();
        assert!(out.contains("==déjà vu==[^1]"));
        assert!(out.ends_with("[^1]: noté\n"));
    }

    #[test]
    fn add_note_without_a_highlight_is_a_noop() {
        let s = "plain text, no highlight\n";
        assert_eq!(add_note(s, 3, "note").unwrap(), s);
    }

    #[test]
    fn upsert_adds_then_edits_then_clears() {
        let s = "The ==claim== stands.\n";
        // Add.
        let added = upsert_note(s, anchor_of(s), "first").unwrap();
        assert_eq!(added, "The ==claim==[^1] stands.\n\n[^1]: first\n");
        // Edit in place — the reference number is unchanged, only the definition text.
        let edited = upsert_note(&added, anchor_of(&added), "second").unwrap();
        assert_eq!(edited, "The ==claim==[^1] stands.\n\n[^1]: second\n");
        // Clear — reference and definition both gone, no dangling blank line.
        let cleared = upsert_note(&edited, anchor_of(&edited), "").unwrap();
        assert_eq!(cleared, "The ==claim== stands.\n");
    }

    #[test]
    fn upsert_empty_on_a_noteless_highlight_is_a_noop() {
        let s = "The ==claim== stands.\n";
        assert_eq!(upsert_note(s, anchor_of(s), "").unwrap(), s);
    }

    #[test]
    fn upsert_edits_only_the_targeted_note_leaving_others() {
        // Two noted highlights; editing the first must not disturb the second's definition.
        let s = "==one==[^1] and ==two==[^2] end.\n\n[^1]: a\n[^2]: b\n";
        let a1 = anchor_of(s); // inside ==one==
        let edited = upsert_note(s, a1, "A!").unwrap();
        assert!(edited.contains("[^1]: A!\n"));
        assert!(edited.contains("[^2]: b\n"), "the other note is untouched");
        assert!(edited.contains("==one==[^1]") && edited.contains("==two==[^2]"));
    }

    #[test]
    fn remove_highlight_and_note_drops_both() {
        let s = "The ==claim==[^1] stands.\n\n[^1]: why it matters\n";
        let inner = first_highlight_after(s, 0).unwrap(); // "claim"
        let out = remove_highlight_and_note(s, inner.start, inner.end).unwrap();
        assert_eq!(out, "The claim stands.\n", "== markers, the [^1] ref and its definition all gone");
        assert!(!out.contains("[^"), "no footnote residue");
    }

    #[test]
    fn remove_highlight_and_note_on_a_plain_highlight_matches_remove_highlight() {
        let s = "The ==claim== stands.\n";
        let inner = first_highlight_after(s, 0).unwrap();
        let a = remove_highlight_and_note(s, inner.start, inner.end).unwrap();
        let b = remove_highlight(s, inner.start, inner.end).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "The claim stands.\n");
    }

    #[test]
    fn remove_one_of_two_noted_highlights_keeps_the_other_note() {
        let s = "==one==[^1] and ==two==[^2] end.\n\n[^1]: a\n[^2]: b\n";
        let first = first_highlight_after(s, 0).unwrap(); // ==one==
        let out = remove_highlight_and_note(s, first.start, first.end).unwrap();
        assert!(!out.contains("[^1]"), "the removed highlight's note is gone");
        assert!(out.contains("==two==[^2]") && out.contains("[^2]: b\n"), "the other highlight + note survive");
    }
}
