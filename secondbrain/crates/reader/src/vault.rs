//! Scan the vault folder into the file list. Titles come from the filename (Obsidian Web Clipper
//! names files after the article title), so the list is available instantly without reading 330
//! files; the `==highlights==` dot is a cheap best-effort content peek.

use crate::model::{DocKind, ReadStatus, Tag, VaultFile};
use crate::parse;
use std::io::Read;
use std::path::Path;

/// Subfolder of the vault holding Readwise Kindle-highlight book exports. Scanned as read-only
/// [`DocKind::Book`]s (kept for dev/test vaults that bundle books inside the vault folder).
pub const BOOKS_SUBDIR: &str = "Kindle Highlights";

/// Sibling directory of the vault holding the clone of a dedicated Kindle highlights repo
/// (the device's `[books]` vaultsync section: vault at `…/secondbrain/vault` → books at
/// `…/secondbrain/books`). Also scanned as read-only [`DocKind::Book`]s.
pub const BOOKS_SIBLING: &str = "books";

/// List `*.md` files in `dir` (non-recursive) as articles, plus any Kindle books — from the
/// `Kindle Highlights/` subfolder and/or the sibling `books/` repo clone — sorted
/// case-insensitively by title. Each file's front-matter head is read (cheap) to pull
/// `published:` + `status:` + managed `tags:` for the list's sort/filter; books also pull
/// `title:`/`author:` (the Readwise filename appends " by <author>").
/// `peek_highlights` additionally flags `==highlights==` (the dot) — usually answered from the same
/// 8KB head; only longer files without a highlight in the head need a full read. Books skip the
/// peek: a book file IS highlights.
pub fn scan(dir: &Path, peek_highlights: bool) -> Vec<VaultFile> {
    let mut files = Vec::new();
    scan_dir(dir, peek_highlights, DocKind::Article, &mut files);
    scan_dir(&dir.join(BOOKS_SUBDIR), false, DocKind::Book, &mut files);
    if let Some(parent) = dir.parent() {
        scan_dir(&parent.join(BOOKS_SIBLING), false, DocKind::Book, &mut files);
    }
    files.sort_by(|a, b| a.title_lower.cmp(&b.title_lower));
    files
}

fn scan_dir(dir: &Path, peek_highlights: bool, kind: DocKind, files: &mut Vec<VaultFile>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let mut title = title_from_path(&path);
        // Rough reading-time from file size. Markdown bytes ≈ characters; ~2200 bytes/min absorbs
        // markup + frontmatter overhead and lands close to the reading view's
        // 1500-alphanumeric-chars/min estimate on a typical article.
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let minutes = (bytes / 2200) as u32;
        const HEAD_CAP: u64 = 8 * 1024;
        let head = read_head(&path, HEAD_CAP as usize);
        let meta = head.as_deref().map(parse::frontmatter).unwrap_or_default();
        // Highlight peek: the 8KB head often already answers (a hit anywhere in it, or the file fits
        // entirely inside it) — only fall back to a full read for longer, so-far-unhighlighted files.
        let has_highlights = peek_highlights
            && (head.as_deref().is_some_and(contains_highlight)
                || (bytes > HEAD_CAP
                    && std::fs::read_to_string(&path).map(|s| contains_highlight(&s)).unwrap_or(false)));
        // A book's frontmatter `title:` is the clean book title (the filename is "Title by Author").
        if kind == DocKind::Book {
            if let Some(t) = meta.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                title = t.to_string();
            }
        }
        let byline = crate::byline::derive_byline(&meta);
        files.push(VaultFile {
            path,
            kind,
            title_lower: title.to_lowercase(),
            title,
            has_highlights,
            published: meta.published,
            created: meta.created,
            byline_lower: byline.as_deref().map(str::to_lowercase),
            byline,
            status: ReadStatus::from_meta(meta.status.as_deref()),
            tags: meta.tags.iter().filter_map(|t| Tag::from_slug(t)).collect(),
            minutes,
        });
    }
}

/// Best-effort `==highlight==` peek for the list dot: an opening marker is `==` immediately
/// followed by highlighted text — not whitespace or a third `=` — which skips the common false
/// positives (`===`-style rules, spaced `a == b` comparisons in code).
fn contains_highlight(s: &str) -> bool {
    s.as_bytes()
        .windows(3)
        .any(|w| w[0] == b'=' && w[1] == b'=' && w[2] != b'=' && !w[2].is_ascii_whitespace())
}

/// Read up to `cap` bytes off the top of a file. Front matter lives at the very start, so a small
/// head read pulls `published:`/`status:` without loading a whole clipping. Lossy UTF-8 is fine —
/// the keys we read are ASCII and well inside the head.
fn read_head(path: &Path, cap: usize) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(cap as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The display title for a clipping: its filename stem, minus Obsidian's " (Clipped)" suffix.
fn title_from_path(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled");
    let stem = stem.strip_suffix(" (Clipped)").unwrap_or(stem);
    stem.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_md_sorted_and_strips_clipped_suffix() {
        let dir = std::env::temp_dir().join(format!("sb-vault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Zebra Facts (Clipped).md"), "plain").unwrap();
        std::fs::write(dir.join("Apple Notes (Clipped).md"), "has ==a highlight== here").unwrap();
        std::fs::write(
            dir.join("Tagged Piece (Clipped).md"),
            "---\ntags:\n  - ai\n  - Health\n  - archive\n  - randomobsidian\n---\nbody",
        )
        .unwrap();
        std::fs::write(dir.join("ignore.txt"), "nope").unwrap();

        let files = scan(&dir, true);
        assert_eq!(files.len(), 3, "only .md files");
        let by = |t: &str| files.iter().find(|f| f.title == t).unwrap();
        assert_eq!(files[0].title, "Apple Notes", "sorted, suffix stripped");
        assert!(by("Apple Notes").has_highlights);
        assert!(!by("Zebra Facts").has_highlights);
        // Managed tags (incl. the special `archive`) parsed case-insensitively; unmanaged ignored.
        assert_eq!(by("Tagged Piece").tags, vec![Tag::Ai, Tag::Health, Tag::Archive]);
        assert!(by("Zebra Facts").tags.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scans_books_subdir_as_read_only_books_with_frontmatter_titles() {
        let dir = std::env::temp_dir().join(format!("sb-vault-books-{}", std::process::id()));
        let books = dir.join(BOOKS_SUBDIR);
        std::fs::create_dir_all(&books).unwrap();
        std::fs::write(dir.join("An Article (Clipped).md"), "plain").unwrap();
        std::fs::write(
            books.join("Atomic Habits by James Clear.md"),
            "---\ntitle: Atomic Habits\nauthor: James Clear\n---\n- A habit is… ([Location 1](https://r))\n",
        )
        .unwrap();
        std::fs::write(books.join("No Frontmatter by Someone.md"), "- text ([Location 2](https://r))\n").unwrap();

        let files = scan(&dir, true);
        assert_eq!(files.len(), 3);
        let habit = files.iter().find(|f| f.title == "Atomic Habits").expect("frontmatter title used");
        assert_eq!(habit.kind, DocKind::Book);
        assert_eq!(habit.byline.as_deref(), Some("James Clear"), "author is the byline");
        assert!(!habit.has_highlights, "books skip the highlight peek");
        let bare = files.iter().find(|f| f.title == "No Frontmatter by Someone").expect("filename fallback");
        assert_eq!(bare.kind, DocKind::Book);
        assert!(files.iter().any(|f| f.title == "An Article" && f.kind == DocKind::Article));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scans_sibling_books_clone_as_books() {
        let root = std::env::temp_dir().join(format!("sb-vault-sib-{}", std::process::id()));
        let vault = root.join("vault");
        let books = root.join(BOOKS_SIBLING);
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::create_dir_all(&books).unwrap();
        std::fs::write(vault.join("An Article.md"), "plain").unwrap();
        std::fs::write(books.join("Deep Work by Cal Newport.md"), "---\ntitle: Deep Work\nauthor: Cal Newport\n---\n- focus ([Location 9](https://r))\n").unwrap();

        let files = scan(&vault, false);
        assert_eq!(files.len(), 2);
        let book = files.iter().find(|f| f.title == "Deep Work").expect("sibling books clone scanned");
        assert_eq!(book.kind, DocKind::Book);
        assert_eq!(book.byline.as_deref(), Some("Cal Newport"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn highlight_peek_skips_rules_and_spaced_comparisons() {
        assert!(contains_highlight("has ==a highlight== here"));
        assert!(contains_highlight("tail word==end")); // closing marker mid-text still opens later
        assert!(!contains_highlight("===\nheading rule\n==="), "=== rules are not highlights");
        assert!(!contains_highlight("if a == b { }"), "spaced comparison is not a highlight");
        assert!(!contains_highlight("no markers at all"));
        assert!(!contains_highlight("=="), "bare marker with nothing after");
    }

    #[test]
    fn highlight_peek_finds_highlights_beyond_the_head() {
        let dir = std::env::temp_dir().join(format!("sb-vault-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A file longer than the 8KB head whose only highlight is at the very end.
        let mut long = "x".repeat(20 * 1024);
        long.push_str(" ==deep highlight==");
        std::fs::write(dir.join("Deep (Clipped).md"), &long).unwrap();
        let files = scan(&dir, true);
        assert!(files[0].has_highlights, "full-read fallback still finds a tail highlight");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
