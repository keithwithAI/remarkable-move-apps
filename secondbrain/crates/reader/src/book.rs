//! Clean a Readwise Kindle-highlights export for reading as prose.
//!
//! A Readwise book file is a frontmatter block, a cover image, `### chapter` headings, and one
//! bullet per highlight — each ending in a `([Location 134](https://readwise.io/…))` link, with the
//! reader's own notes as nested `- Note: …` sub-bullets. Rendered raw that reads like a data dump,
//! so [`clean_book_source`] rewrites the body into clean Markdown prose BEFORE the normal parse:
//!
//!   - a top-level highlight bullet → its own bulleted item (each highlight is a disjoint excerpt,
//!     so the bullet keeps separate points reading as separate points), with the trailing
//!     `([Location …](…))` / `([Page …](…))` parenthetical stripped (continuation lines folded in);
//!   - a nested `- Note: …` (or `- Tags: …`) sub-bullet → a nested (depth-1) list item with a bold
//!     lead, visually a callout hanging under its highlight;
//!   - frontmatter, headings, the cover image, and anything unrecognized pass through untouched.
//!
//! Books are **read-only** on device (Readwise regenerates the files), so unlike articles the
//! cleaned text's byte offsets never need to map back to the original file — the transformed string
//! simply becomes the `Reading.source` and is never written.

/// Rewrite a Readwise book export's body into clean prose Markdown (see module docs). The
/// frontmatter block is preserved verbatim so `parse` still reads the meta from it.
pub fn clean_book_source(src: &str) -> String {
    // Split the frontmatter off by line count, reusing the same fence rules as `parse`.
    let body_start = frontmatter_end(src);
    let mut out = String::with_capacity(src.len());
    out.push_str(&src[..body_start]);

    // Accumulate one logical item at a time: a highlight paragraph or a note sub-item. A new
    // top-level bullet / heading / blank line flushes the previous item.
    let mut para: Option<String> = None; // in-flight highlight text
    let mut note: Option<String> = None; // in-flight note text (sub-bullet)
    for line in src[body_start..].lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if let Some(item) = trimmed.strip_prefix("- ") {
            if indent == 0 {
                flush(&mut out, &mut para, &mut note);
                para = Some(strip_location(item).to_string());
            } else {
                // A nested sub-bullet: a Note/Tags callout under the current highlight.
                flush_note(&mut out, &mut note);
                note = Some(strip_location(item).to_string());
            }
            continue;
        }
        // A non-bullet line while an item is open: blank ends it, an indented line continues it.
        if trimmed.is_empty() {
            flush(&mut out, &mut para, &mut note);
            out.push('\n');
            continue;
        }
        if note.is_some() && indent > 0 {
            let n = note.as_mut().unwrap();
            n.push(' ');
            n.push_str(strip_location(trimmed));
            continue;
        }
        if para.is_some() && indent > 0 {
            let p = para.as_mut().unwrap();
            p.push(' ');
            p.push_str(strip_location(trimmed));
            continue;
        }
        // Readwise's re-sync markers ("## New highlights added January 14, 2026 at 12:52 PM") are
        // bookkeeping, not chapters — dropping them keeps the Contents panel to real headings.
        if trimmed.trim_start_matches('#').trim_start().starts_with("New highlights added ") {
            flush(&mut out, &mut para, &mut note);
            continue;
        }
        // Headings, the cover image, rules… pass through (after closing any open item).
        flush(&mut out, &mut para, &mut note);
        out.push_str(line);
        out.push('\n');
    }
    flush(&mut out, &mut para, &mut note);
    out
}

/// Emit the in-flight highlight and note (in that order), each followed by a blank line. The
/// highlight keeps a `- ` bullet (a loose list item — the layout draws the bullet + indent), so
/// consecutive disjoint excerpts never read as one continuous text.
fn flush(out: &mut String, para: &mut Option<String>, note: &mut Option<String>) {
    if let Some(p) = para.take() {
        let p = p.trim();
        if !p.is_empty() {
            out.push_str(&format!("- {p}\n\n"));
        }
    }
    flush_note(out, note);
}

/// Emit the in-flight note as a NESTED (depth-1) list item leading with a bold **Note:**/**Tags:**
/// — a callout visually subordinate to the bulleted highlight above it. The 4-space indent makes
/// it a nested list in CommonMark (not indented code, which would need 4 more). (Bold, not italic:
/// the reader has no italic face, so `*…*` would render indistinguishably from body text.)
fn flush_note(out: &mut String, note: &mut Option<String>) {
    if let Some(n) = note.take() {
        let n = n.trim();
        if n.is_empty() {
            return;
        }
        // Bold the "Note:"/"Tags:"-style lead-in when present, so the callout announces itself.
        match n.split_once(':') {
            Some((lead, rest)) if !lead.is_empty() && lead.len() <= 12 && !lead.contains(' ') => {
                out.push_str(&format!("    - **{lead}:** {}\n\n", rest.trim_start()));
            }
            // No lead-in (rare) — it's still the reader's own margin note, so label it as one.
            _ => out.push_str(&format!("    - **Note:** {n}\n\n")),
        }
    }
}

/// Strip a trailing ` ([Location 134](url))` / ` ([Page 12](url))` / ` (Location 134)` parenthetical
/// off a highlight, leaving just the prose.
fn strip_location(s: &str) -> &str {
    let t = s.trim_end();
    if !t.ends_with(')') {
        return t;
    }
    for marker in [" ([Location", " ([Page", " (Location", " (Page"] {
        if let Some(pos) = t.rfind(marker) {
            // Only strip when the parenthetical runs to the end of the line (it always does in
            // Readwise exports); a mid-sentence "(Location …)" earlier in the text is left alone.
            let tail = &t[pos..];
            if balanced_to_end(tail) {
                return t[..pos].trim_end();
            }
        }
    }
    t
}

/// Whether `tail` (starting at " (") closes its parens exactly at the end of the string.
fn balanced_to_end(tail: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in tail.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1 == tail.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// Byte offset where the body begins (just past a `---` … `---` frontmatter block), or 0.
fn frontmatter_end(source: &str) -> usize {
    if !(source.starts_with("---\n") || source.starts_with("---\r\n")) {
        return 0;
    }
    let Some(after_open) = source.find('\n').map(|i| i + 1) else {
        return 0;
    };
    let mut cur = after_open;
    for line in source[after_open..].split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return cur + line.len();
        }
        cur += line.len();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse, Block};

    const SRC: &str = "---\ntitle: Atomic Habits\nauthor: James Clear\ncoverUrl: https://img.example/cover.jpg\n---\n![rw-book-cover](https://img.example/cover.jpg)\n\n### Introduction My Story\n- A habit is a routine or behavior that is performed regularly. ([Location 137](https://readwise.io/to_kindle?action=open&asin=B01N5AX61W&location=137))\n- Small habits helped me fulfill my potential. ([Location 181](https://readwise.io/to_kindle?action=open&asin=B01N5AX61W&location=181))\n    - Note: The question of this book: how to build small habits\n      and fulfill your potential\n- You do not rise to the level of your goals. You fall to the level of your systems. ([Location 419](https://readwise.io/to_kindle?action=open&asin=B01N5AX61W&location=419))\n";

    #[test]
    fn strips_location_links_and_bullets() {
        let out = clean_book_source(SRC);
        assert!(!out.contains("readwise.io"), "location links stripped:\n{out}");
        assert!(!out.contains("([Location"), "no location parentheticals remain");
        assert!(out.contains("- A habit is a routine or behavior that is performed regularly.\n\n"), "highlight keeps a bullet");
        assert!(out.starts_with("---\ntitle: Atomic Habits"), "frontmatter preserved verbatim");
        assert!(out.contains("### Introduction My Story"), "headings pass through");
        assert!(out.contains("![rw-book-cover]"), "cover image passes through");
    }

    #[test]
    fn notes_become_bold_led_callout_items_with_continuations_folded() {
        let out = clean_book_source(SRC);
        assert!(
            out.contains("    - **Note:** The question of this book: how to build small habits and fulfill your potential"),
            "note is a nested list item with a bold Note: lead and its continuation folded in:\n{out}"
        );
        // The note must follow its highlight, before the next one.
        let hl = out.find("Small habits helped me").unwrap();
        let note = out.find("    - **Note:**").unwrap();
        let next = out.find("You do not rise").unwrap();
        assert!(hl < note && note < next, "note sits under its highlight");
    }

    #[test]
    fn cleaned_source_parses_into_bulleted_items_headings_and_nested_notes() {
        let out = clean_book_source(SRC);
        let doc = parse(&out);
        assert_eq!(doc.meta.title.as_deref(), Some("Atomic Habits"));
        assert_eq!(doc.meta.author.as_deref(), Some("James Clear"));
        let highlights = doc.blocks.iter().filter(|b| matches!(b, Block::ListItem { depth: 0, .. })).count();
        assert!(highlights >= 3, "each highlight is a depth-0 bulleted item, got {highlights}");
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Heading { level: 3, .. })));
        let note_item = doc.blocks.iter().find_map(|b| match b {
            Block::ListItem { depth: 1, inlines, .. } => Some(inlines),
            _ => None,
        });
        let note_item = note_item.expect("note renders as a nested depth-1 list item");
        assert!(
            note_item.iter().any(|i| i.emphasis == crate::parse::Emphasis::Bold && i.text.contains("Note:")),
            "note leads with a bold Note: label"
        );
    }

    #[test]
    fn location_variants_and_plain_text_are_handled() {
        assert_eq!(strip_location("text here. ([Location 12](https://x))"), "text here.");
        assert_eq!(strip_location("text here. ([Page 12](https://x))"), "text here.");
        assert_eq!(strip_location("text here. (Location 12)"), "text here.");
        assert_eq!(strip_location("no marker at all."), "no marker at all.");
        assert_eq!(
            strip_location("mentions (Location 5) mid-sentence and continues."),
            "mentions (Location 5) mid-sentence and continues.",
            "mid-sentence parenthetical kept"
        );
    }

    #[test]
    fn readwise_resync_marker_headings_are_dropped() {
        let src = "### Real Chapter\n- one ([Location 1](https://r))\n\n## New highlights added January 14, 2026 at 12:52 PM\n- two ([Location 2](https://r))\n";
        let out = clean_book_source(src);
        assert!(!out.contains("New highlights added"), "sync markers dropped:\n{out}");
        assert!(out.contains("### Real Chapter"));
        assert!(out.contains("two"), "highlights under a dropped marker are kept");
    }

    #[test]
    fn no_frontmatter_body_still_cleans() {
        let out = clean_book_source("- a highlight ([Location 1](https://r))\n");
        assert_eq!(out.trim(), "- a highlight");
    }
}
