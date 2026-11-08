//! Wrap + paginate parsed [`Block`]s into positioned [`GlyphRun`]s, keeping the **glyph→source
//! byte** map that powers pen selection. This is the reader's linchpin: every run records the
//! source range it was cut from, so [`page_hit`] turns a pen `(x, y)` into a byte offset and
//! [`range_rects`] turns a byte range back into on-screen rectangles — inverses of each other.
//!
//! Pure geometry: measures with the shared IBM Plex faces via `render::draw::measure`, no drawing.

use crate::parse::{Block, Doc, Emphasis};
use ab_glyph::FontRef;
use render::draw::measure;
use image::RgbImage;
use render::Fonts;
use std::collections::HashMap;
use std::ops::Range;

/// Page + type geometry. Defaults target the Move's portrait panel; the app may tweak sizes.
#[derive(Debug, Clone, Copy)]
pub struct LayoutOpts {
    pub page_w: u32,
    pub page_h: u32,
    pub margin_x: f32,
    /// Content top — below the running header the app paints.
    pub content_top: f32,
    /// Content bottom — above the page N/M footer.
    pub content_bottom: f32,
    /// Extra top inset applied to **page 1 only** (reserves the reading article masthead); 0 = every
    /// page starts at `content_top`. `Layouter::new` adds it to page 1's initial cursor; the page-break
    /// reset in `ensure` keeps pages 2+ at `content_top`.
    pub first_page_extra: f32,
    pub body_px: f32,
    pub line_h: f32,
    /// Uniform text-size multiplier (1.0 = base). Scales every font size and line height — and the
    /// inter-line/paragraph gaps — together, so one control grows both text and spacing.
    pub scale: f32,
    /// Force a page break at this source byte: the first content whose `src.start >= b` starts a new
    /// page. Pins a known line as a page top across re-paginations — line wraps depend only on the
    /// column width, so any line start in one pagination is a line start in every other. Used to keep
    /// the reader's place when the content area changes (focus toggle) or the text rescales.
    pub force_break_at: Option<usize>,
}

impl Default for LayoutOpts {
    fn default() -> Self {
        LayoutOpts {
            page_w: 954,
            page_h: 1696,
            margin_x: 56.0,
            // Reading body top — clears the slim top chrome (view::READING_HEADER_H = 80) + slack.
            // Hand-matched literals: nothing derives one from the other. Reading-only — the list/search
            // screens paginate on HEADER_H/FOOTER_H, not LayoutOpts. The page-1 article masthead is
            // reserved on top of this via `first_page_extra`.
            content_top: 88.0,
            content_bottom: 1600.0,
            first_page_extra: 0.0,
            body_px: 38.0,
            line_h: 54.0,
            scale: 1.0,
            force_break_at: None,
        }
    }
}

/// One positioned run of text on a page. `text == source[src]` for visible runs, so a byte offset
/// inside `src` corresponds 1:1 to a byte offset inside `text` (used for sub-run hit-testing).
#[derive(Debug, Clone)]
pub struct GlyphRun {
    pub x: f32,
    pub baseline: f32,
    pub w: f32,
    pub px: f32,
    pub line_top: f32,
    pub line_h: f32,
    pub text: String,
    pub emphasis: Emphasis,
    pub highlight: bool,
    pub heading: bool,
    pub src: Range<usize>,
}

#[derive(Debug, Clone)]
pub enum PageItem {
    Text(GlyphRun),
    /// Placeholder box (Phase 1) / decoded image (Phase 3). `src` is the `![](…)` span.
    Image { x: f32, y: f32, w: f32, h: f32, url: String, alt: String, src: Range<usize> },
    Rule { x: f32, y: f32, w: f32 },
    Bullet { x: f32, baseline: f32, px: f32 },
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub items: Vec<PageItem>,
}

impl Page {
    /// The text runs on this page, in reading order.
    pub fn runs(&self) -> impl Iterator<Item = &GlyphRun> {
        self.items.iter().filter_map(|it| match it {
            PageItem::Text(r) => Some(r),
            _ => None,
        })
    }
}

/// Pick the IBM Plex face for a run's emphasis (no italic face is embedded → italics use sans).
pub fn face<'a>(fonts: &'a Fonts, e: Emphasis, heading: bool) -> &'a FontRef<'static> {
    if heading {
        return &fonts.sans_semibold;
    }
    match e {
        Emphasis::Bold | Emphasis::BoldItalic => &fonts.sans_semibold,
        Emphasis::Code => &fonts.mono,
        _ => &fonts.sans,
    }
}

/// Lay a whole document out into pages (placeholder boxes for every image).
pub fn layout(doc: &Doc, fonts: &Fonts, opts: LayoutOpts) -> Vec<Page> {
    layout_with_images(doc, fonts, opts, &HashMap::new())
}

/// Lay a document out, sizing each `![](…)` box from a **prepared** image (see [`crate::imaging`])
/// when one is present in `images` (keyed by URL) — otherwise reserve the placeholder box. The map's
/// images are already scaled to their on-page size, so `paint` blits them 1:1 and layout and paint
/// always agree on geometry.
pub fn layout_with_images(
    doc: &Doc,
    fonts: &Fonts,
    opts: LayoutOpts,
    images: &HashMap<String, RgbImage>,
) -> Vec<Page> {
    let mut l = Layouter::new(fonts, opts, images);
    for block in &doc.blocks {
        l.block(block);
    }
    l.finish()
}

struct Layouter<'f> {
    fonts: &'f Fonts,
    opts: LayoutOpts,
    images: &'f HashMap<String, RgbImage>,
    pages: Vec<Page>,
    cur: Page,
    y: f32,
    left: f32,
    right: f32,
}

/// A word or whitespace token with its source span and measured width.
struct Tok {
    text: String,
    src: Range<usize>,
    emphasis: Emphasis,
    highlight: bool,
    space: bool,
    w: f32,
}

impl<'f> Layouter<'f> {
    fn new(fonts: &'f Fonts, opts: LayoutOpts, images: &'f HashMap<String, RgbImage>) -> Self {
        let left = opts.margin_x;
        let right = opts.page_w as f32 - opts.margin_x;
        // Page 1 starts `first_page_extra` lower (reading masthead reservation); pages 2+ reset to
        // `content_top` in `ensure`, so the inset applies to page 1 only.
        Layouter { fonts, opts, images, pages: Vec::new(), cur: Page::default(), y: opts.content_top + opts.first_page_extra, left, right }
    }

    fn finish(mut self) -> Vec<Page> {
        if !self.cur.items.is_empty() {
            self.pages.push(self.cur);
        }
        if self.pages.is_empty() {
            self.pages.push(Page::default());
        }
        self.pages
    }

    /// Break to a new page if the next `h` pixels won't fit; returns the (possibly reset) `y`.
    fn ensure(&mut self, h: f32) {
        if self.y + h > self.opts.content_bottom && !self.cur.items.is_empty() {
            let full = std::mem::take(&mut self.cur);
            self.pages.push(full);
            self.y = self.opts.content_top;
        }
    }

    /// Honour [`LayoutOpts::force_break_at`]: content starting at-or-after the anchor byte begins a
    /// new page, exactly once — the current page must be non-empty AND have begun before the anchor
    /// (afterwards every page starts at-or-after it, so the condition can't re-fire). A natural
    /// boundary already at the anchor is a no-op (the current page is empty).
    fn maybe_force_break(&mut self, next_src: usize) {
        let Some(b) = self.opts.force_break_at else {
            return;
        };
        if next_src < b || self.cur.items.is_empty() {
            return;
        }
        let starts_before = self.cur.items.iter().any(|it| match it {
            PageItem::Text(r) => r.src.start < b,
            PageItem::Image { src, .. } => src.start < b,
            PageItem::Rule { .. } | PageItem::Bullet { .. } => false,
        });
        if starts_before {
            let full = std::mem::take(&mut self.cur);
            self.pages.push(full);
            self.y = self.opts.content_top;
        }
    }

    /// Scale a base font size or vertical gap by the current text-size factor.
    fn s(&self, v: f32) -> f32 {
        v * self.opts.scale
    }

    fn block(&mut self, block: &Block) {
        match block {
            Block::Heading { level, inlines } => {
                let (px, line_h, before, after) = match level {
                    1 => (54.0, 70.0, 28.0, 18.0),
                    2 => (44.0, 58.0, 32.0, 14.0),
                    _ => (38.0, 52.0, 24.0, 10.0),
                };
                self.y += self.s(before);
                self.flow(inlines, self.s(px), self.s(line_h), 0.0, true);
                self.y += self.s(after);
            }
            Block::Paragraph { inlines } => {
                self.flow(inlines, self.s(self.opts.body_px), self.s(self.opts.line_h), 0.0, false);
                self.y += self.s(18.0);
            }
            Block::ListItem { depth, inlines, .. } => {
                let indent = self.s(30.0 + *depth as f32 * 30.0);
                let line_h = self.s(self.opts.line_h);
                let px = self.s(self.opts.body_px);
                self.y += self.s(4.0);
                // Bullet shares the first line's baseline; ensure the line fits before placing it.
                self.ensure(line_h);
                let base = self.first_baseline(line_h);
                self.cur.items.push(PageItem::Bullet { x: self.left + indent - self.s(22.0), baseline: base, px });
                self.flow(inlines, px, line_h, indent, false);
                self.y += self.s(6.0);
            }
            Block::CodeBlock { text, src } => {
                self.y += self.s(12.0);
                let px = self.s(30.0);
                let line_h = self.s(40.0);
                let mut off = src.start;
                for line in text.split_inclusive('\n') {
                    self.maybe_force_break(off);
                    self.ensure(line_h);
                    let base = self.first_baseline(line_h);
                    let trimmed = line.trim_end_matches('\n');
                    let w = measure(&self.fonts.mono, px, trimmed);
                    self.cur.items.push(PageItem::Text(GlyphRun {
                        x: self.left + self.s(12.0),
                        baseline: base,
                        w,
                        px,
                        line_top: self.y,
                        line_h,
                        text: trimmed.to_string(),
                        emphasis: Emphasis::Code,
                        highlight: false,
                        heading: false,
                        src: off..off + trimmed.len(),
                    }));
                    off += line.len();
                    self.y += line_h;
                }
                self.y += self.s(14.0);
            }
            Block::Image { url, alt, src } => {
                self.y += 18.0;
                // A prepared image sizes its own (centered) box; otherwise a full-width placeholder.
                let (x, w, h) = match self.images.get(url).map(|im| im.dimensions()) {
                    Some((iw, ih)) => {
                        let w = iw as f32;
                        (self.left + ((self.right - self.left) - w).max(0.0) / 2.0, w, ih as f32)
                    }
                    None => (self.left, self.right - self.left, 320.0),
                };
                self.maybe_force_break(src.start);
                self.ensure(h);
                self.cur.items.push(PageItem::Image {
                    x,
                    y: self.y,
                    w,
                    h,
                    url: url.clone(),
                    alt: alt.clone(),
                    src: src.clone(),
                });
                self.y += h + 20.0;
            }
            Block::Rule => {
                self.y += 20.0;
                self.ensure(4.0);
                self.cur.items.push(PageItem::Rule { x: self.left, y: self.y, w: self.right - self.left });
                self.y += 22.0;
            }
        }
    }

    /// Baseline for a line whose top is the current `y` (empirical vertical centering).
    fn first_baseline(&self, line_h: f32) -> f32 {
        self.y + line_h * 0.72
    }

    /// Word-wrap `inlines` at `px` into lines indented by `indent`, emitting positioned runs.
    fn flow(&mut self, inlines: &[crate::parse::Inline], px: f32, line_h: f32, indent: f32, heading: bool) {
        let toks = self.tokenize(inlines, px, heading);
        let max_w = self.right - (self.left + indent);
        let mut line: Vec<Tok> = Vec::with_capacity(24); // ~a body line of word+space tokens
        let mut line_w = 0.0;

        for tok in toks {
            if tok.space {
                if line.is_empty() {
                    continue; // no leading spaces
                }
                line_w += tok.w;
                line.push(tok);
                continue;
            }
            if !line.is_empty() && line_w + tok.w > max_w {
                self.commit_line(&mut line, px, line_h, indent, heading);
                line_w = 0.0;
            }
            line_w += tok.w;
            line.push(tok);
        }
        if !line.is_empty() {
            self.commit_line(&mut line, px, line_h, indent, heading);
        }
    }

    fn commit_line(&mut self, line: &mut Vec<Tok>, px: f32, line_h: f32, indent: f32, heading: bool) {
        // Drop trailing whitespace so it neither renders nor shifts alignment.
        while line.last().map(|t| t.space).unwrap_or(false) {
            line.pop();
        }
        if line.is_empty() {
            return;
        }
        if let Some(first) = line.first() {
            self.maybe_force_break(first.src.start);
        }
        self.ensure(line_h);
        let baseline = self.first_baseline(line_h);
        let mut x = self.left + indent;
        for tok in line.drain(..) {
            self.cur.items.push(PageItem::Text(GlyphRun {
                x,
                baseline,
                w: tok.w,
                px,
                line_top: self.y,
                line_h,
                text: tok.text,
                emphasis: tok.emphasis,
                highlight: tok.highlight,
                heading,
                src: tok.src,
            }));
            x += tok.w;
        }
        self.y += line_h;
    }

    /// Split inline runs into word/space tokens, each measured at `px` with its own face.
    fn tokenize(&self, inlines: &[crate::parse::Inline], px: f32, heading: bool) -> Vec<Tok> {
        // ~5 chars/word + its space token ≈ len/3 tokens; a hint, not a bound.
        let mut toks = Vec::with_capacity(inlines.iter().map(|i| i.text.len() / 3 + 1).sum());
        for inl in inlines {
            let f = face(self.fonts, inl.emphasis, heading);
            // Group consecutive spaces vs non-spaces, tracking byte offsets into the run.
            let bytes = inl.text.as_bytes();
            let mut i = 0;
            while i < inl.text.len() {
                let is_space = bytes[i] == b' ';
                let start = i;
                if is_space {
                    while i < inl.text.len() && bytes[i] == b' ' {
                        i += 1;
                    }
                } else {
                    // advance to next space, respecting char boundaries
                    while i < inl.text.len() && bytes[i] != b' ' {
                        i += 1;
                        while i < inl.text.len() && !inl.text.is_char_boundary(i) {
                            i += 1;
                        }
                    }
                }
                let text = inl.text[start..i].to_string();
                let w = measure(f, px, &text);
                toks.push(Tok {
                    src: inl.src.start + start..inl.src.start + i,
                    text,
                    emphasis: inl.emphasis,
                    highlight: inl.highlight,
                    space: is_space,
                    w,
                });
            }
        }
        toks
    }
}

/// Map a pen `(x, y)` on a page to a source byte offset (nearest character boundary). Returns
/// `None` if there are no text runs. Picks the run on the nearest line, then the nearest run on
/// that line, then the nearest char boundary within it.
pub fn page_hit(page: &Page, fonts: &Fonts, x: f32, y: f32) -> Option<usize> {
    let runs: Vec<&GlyphRun> = page.runs().collect();
    if runs.is_empty() {
        return None;
    }
    // Nearest line by vertical distance to the line box.
    let line_top = runs
        .iter()
        .min_by(|a, b| line_dist(a, y).total_cmp(&line_dist(b, y)))
        .map(|r| r.line_top)
        .unwrap();
    let on_line: Vec<&&GlyphRun> = runs.iter().filter(|r| (r.line_top - line_top).abs() < 0.5).collect();

    // The run horizontally under x, else the nearest run end on the line.
    let mut best = on_line[0];
    let mut best_d = f32::MAX;
    for r in &on_line {
        let d = if x < r.x {
            r.x - x
        } else if x > r.x + r.w {
            x - (r.x + r.w)
        } else {
            0.0
        };
        if d < best_d {
            best_d = d;
            best = r;
        }
    }
    Some(byte_at_x(best, fonts, x))
}

fn line_dist(r: &GlyphRun, y: f32) -> f32 {
    if y < r.line_top {
        r.line_top - y
    } else if y > r.line_top + r.line_h {
        y - (r.line_top + r.line_h)
    } else {
        0.0
    }
}

/// Nearest char boundary in `run` to horizontal position `x` → absolute source byte offset.
fn byte_at_x(run: &GlyphRun, fonts: &Fonts, x: f32) -> usize {
    let f = face(fonts, run.emphasis, run.heading);
    let target = x - run.x;
    if target <= 0.0 {
        return run.src.start;
    }
    let mut best_off = 0usize;
    let mut best_d = target.abs();
    for (off, _) in run.text.char_indices().skip(1) {
        let w = measure(f, run.px, &run.text[..off]);
        let d = (w - target).abs();
        if d < best_d {
            best_d = d;
            best_off = off;
        }
    }
    // The full-width position (end of run).
    let end_d = (run.w - target).abs();
    if end_d < best_d {
        best_off = run.text.len();
    }
    run.src.start + best_off
}

/// Turn a source byte `range` into on-page rectangles `(x, y, w, h)` — for drawing a live
/// selection or an existing highlight tint. Only text runs that overlap `range` contribute.
pub fn range_rects(page: &Page, fonts: &Fonts, range: &Range<usize>) -> Vec<(f32, f32, f32, f32)> {
    let mut rects = Vec::new();
    for r in page.runs() {
        let s = r.src.start.max(range.start);
        let e = r.src.end.min(range.end);
        if s >= e {
            continue;
        }
        let f = face(fonts, r.emphasis, r.heading);
        let a = s - r.src.start;
        let b = e - r.src.start;
        let pre = if a == 0 { 0.0 } else { measure(f, r.px, &r.text[..a]) };
        let mid = measure(f, r.px, &r.text[a..b]);
        rects.push((r.x + pre, r.line_top, mid, r.line_h));
    }
    rects
}

/// The index of the page that contains source byte `byte` — the last page whose first run starts at
/// or before it (pages are laid out in source order). Used to hold the reading position across a
/// re-layout at a new text size. Falls back to 0.
pub fn page_of_byte(pages: &[Page], byte: usize) -> usize {
    let mut best = 0;
    for (i, page) in pages.iter().enumerate() {
        if page.runs().map(|r| r.src.start).min().is_some_and(|start| start <= byte) {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn fonts() -> Fonts {
        Fonts::embedded()
    }

    #[test]
    fn wraps_long_paragraph_into_multiple_lines() {
        let src = "word ".repeat(200);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let lines: std::collections::BTreeSet<i32> =
            pages.iter().flat_map(|p| p.runs().map(|r| r.line_top as i32)).collect();
        assert!(lines.len() > 5, "expected many wrapped lines, got {}", lines.len());
    }

    #[test]
    fn long_doc_paginates() {
        let src = "Lorem ipsum dolor sit amet. ".repeat(1500);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(pages.len() > 1, "expected multiple pages, got {}", pages.len());
        // No run may spill past the content bottom.
        let opts = LayoutOpts::default();
        for p in &pages {
            for r in p.runs() {
                assert!(r.line_top + r.line_h <= opts.content_bottom + 0.5, "run past bottom: {}", r.line_top);
            }
        }
    }

    #[test]
    fn force_break_pins_the_anchor_line_as_a_page_top() {
        let src = "Lorem ipsum dolor sit amet consectetur. ".repeat(400);
        let doc = parse(&src);
        let natural = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(natural.len() > 3);
        // Anchor on page 2's first byte (a line start in ANY pagination of this width), then
        // re-paginate with a taller content area: the anchor line must still open a page.
        let anchor = natural[2].runs().map(|r| r.src.start).min().unwrap();
        let focus = LayoutOpts { content_top: 48.0, content_bottom: 1648.0, force_break_at: Some(anchor), ..LayoutOpts::default() };
        let pages = layout(&doc, &fonts(), focus);
        let ix = page_of_byte(&pages, anchor);
        assert_eq!(pages[ix].runs().map(|r| r.src.start).min(), Some(anchor), "the anchor byte starts its page");
        // And the tiling stays sound: no page's content overlaps the next page's.
        for w in pages.windows(2) {
            let end = w[0].runs().map(|r| r.src.end).max().unwrap_or(0);
            let next = w[1].runs().map(|r| r.src.start).min().unwrap_or(usize::MAX);
            assert!(next >= end, "pages overlap: {next} < {end}");
        }
    }

    #[test]
    fn force_break_on_a_natural_boundary_changes_nothing() {
        let src = "Lorem ipsum dolor sit amet consectetur. ".repeat(400);
        let doc = parse(&src);
        let natural = layout(&doc, &fonts(), LayoutOpts::default());
        let anchor = natural[2].runs().map(|r| r.src.start).min().unwrap();
        let pinned = layout(&doc, &fonts(), LayoutOpts { force_break_at: Some(anchor), ..LayoutOpts::default() });
        assert_eq!(natural.len(), pinned.len(), "an anchor already on a boundary is a no-op");
        for (a, b) in natural.iter().zip(&pinned) {
            assert_eq!(a.runs().map(|r| r.src.clone()).collect::<Vec<_>>(), b.runs().map(|r| r.src.clone()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn hit_and_rects_are_inverse_on_a_run() {
        let src = "The quick brown fox jumps over the lazy dog.";
        let doc = parse(src);
        let f = fonts();
        let pages = layout(&doc, &f, LayoutOpts::default());
        let page = &pages[0];
        // Pick a run in the middle of the line and hit its start x → we should land on its first byte.
        let run = page.runs().find(|r| r.text == "brown").expect("brown run present");
        let hit = page_hit(page, &f, run.x + 0.5, run.baseline - 5.0).unwrap();
        assert_eq!(hit, run.src.start, "hitting a run's left edge maps to its start byte");
        assert_eq!(&src[hit..hit + 5], "brown");

        // range_rects over "brown" yields one rect roughly at the run's position/width.
        let rects = range_rects(page, &f, &run.src);
        assert_eq!(rects.len(), 1);
        let (rx, _ry, rw, _rh) = rects[0];
        assert!((rx - run.x).abs() < 1.0 && (rw - run.w).abs() < 1.0, "rect matches run box");
    }

    #[test]
    fn existing_highlight_runs_carry_the_flag_through_layout() {
        let src = "plain and ==vivid highlighted text== after";
        let doc = parse(src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let hl: String = pages[0].runs().filter(|r| r.highlight).map(|r| r.text.as_str()).collect();
        assert_eq!(hl.replace(' ', ""), "vividhighlightedtext");
    }

    #[test]
    fn scale_grows_type_and_paginates_more() {
        let doc = parse(&"word ".repeat(400));
        let small = layout(&doc, &fonts(), LayoutOpts { scale: 0.8, ..Default::default() });
        let big = layout(&doc, &fonts(), LayoutOpts { scale: 1.4, ..Default::default() });
        let body_px = |ps: &[Page]| ps[0].runs().next().unwrap().px;
        assert!(body_px(&big) > body_px(&small), "a bigger scale raises the glyph px");
        assert!(big.len() >= small.len(), "bigger text needs at least as many pages");
    }

    #[test]
    fn page_of_byte_finds_the_page_for_a_source_offset() {
        let doc = parse(&"Lorem ipsum dolor sit amet. ".repeat(1500));
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(pages.len() > 2);
        let last = pages.len() - 1;
        let b = pages[last].runs().next().unwrap().src.start;
        assert_eq!(page_of_byte(&pages, b), last, "a byte on the last page maps back to it");
        assert_eq!(page_of_byte(&pages, 0), 0);
    }
}
