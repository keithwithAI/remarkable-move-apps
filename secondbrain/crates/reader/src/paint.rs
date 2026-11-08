//! Paint a laid-out [`Page`] to an RGB buffer using the shared raster primitives. Pure (no
//! device): the app blits the result to the framebuffer, tests snapshot it to PNG.

use crate::layout::{face, GlyphRun, Page, PageItem};
use image::{Rgb, RgbImage};
use render::draw::{draw_text, fill_rect, hline, rect_outline};
use render::{Fonts, FAINT, INK, MUTED, PAPER};
use std::collections::HashMap;
use std::ops::Range;

/// The highlight wash — a warm yellow that survives the Gallery 3 panel's muting and keeps black
/// body text readable on top.
pub const HIGHLIGHT: Rgb<u8> = Rgb([250, 232, 120]);

/// Render `page` onto a fresh white canvas of the layout's page size.
pub fn draw_page(page: &Page, fonts: &Fonts, page_w: u32, page_h: u32) -> RgbImage {
    draw_page_with_images(page, fonts, page_w, page_h, &HashMap::new())
}

/// As [`draw_page`], but blits **prepared** images (keyed by URL, already scaled to their on-page
/// size by [`crate::imaging::prepare`]) in place of the placeholder box where one is available.
pub fn draw_page_with_images(
    page: &Page,
    fonts: &Fonts,
    page_w: u32,
    page_h: u32,
    images: &HashMap<String, RgbImage>,
) -> RgbImage {
    let mut img = RgbImage::from_pixel(page_w, page_h, PAPER);

    // 1. Highlight washes first, so glyphs land on top. (A live pen-selection tint used to be
    // drawn here too — removed: Kindle-style anchor selection never presents mid-drag, so the
    // parameter was always None in practice.)
    for r in page.runs() {
        if r.highlight {
            fill_rect(&mut img, r.x - 1.0, r.line_top + 2.0, r.w + 2.0, r.line_h - 3.0, HIGHLIGHT);
        }
    }

    // 2. Content.
    for item in &page.items {
        match item {
            PageItem::Text(r) => {
                let f = face(fonts, r.emphasis, r.heading);
                draw_text(&mut img, f, r.px, r.x, r.baseline, &r.text, INK);
            }
            PageItem::Bullet { x, baseline, px } => {
                draw_text(&mut img, &fonts.sans, *px, *x, *baseline, "•", MUTED);
            }
            PageItem::Rule { x, y, w } => {
                hline(&mut img, *x, *x + *w, *y, FAINT, 2);
            }
            PageItem::Image { x, y, w, h, alt, url, .. } => {
                if let Some(im) = images.get(url) {
                    blit(&mut img, im, *x, *y);
                } else {
                    draw_image_placeholder(&mut img, fonts, *x, *y, *w, *h, alt, url);
                }
            }
        }
    }
    img
}

/// The text runs of `page` whose lines, shifted by `dy`, fit fully inside `clip` — the exact set
/// [`draw_text_runs_offset`] draws. Shared with the model side (`view::focus_entry_window`) so the
/// bytes shown in a focus preview band and the bytes the first in-focus page turn continues from
/// agree exactly.
pub fn runs_in_clip<'a>(page: &'a Page, dy: f32, clip: &Range<f32>) -> impl Iterator<Item = &'a GlyphRun> + 'a {
    let (start, end) = (clip.start, clip.end);
    page.runs().filter(move |r| {
        let top = r.line_top + dy;
        top >= start - 0.5 && top + r.line_h <= end + 0.5
    })
}

/// Draw only `page`'s text lines shifted vertically by `dy`, keeping lines fully inside `clip`.
/// Used by the focus view's preview bands: the previous page's tail / next page's head continue the
/// line grid into the freed chrome space. Text-only — no image or rule fits the ~90px bands.
pub fn draw_text_runs_offset(img: &mut RgbImage, page: &Page, fonts: &Fonts, dy: f32, clip: &Range<f32>) {
    // Highlight washes first, so glyphs land on top (mirrors [`draw_page_with_images`]).
    for r in runs_in_clip(page, dy, clip) {
        if r.highlight {
            fill_rect(img, r.x - 1.0, r.line_top + dy + 2.0, r.w + 2.0, r.line_h - 3.0, HIGHLIGHT);
        }
    }
    for r in runs_in_clip(page, dy, clip) {
        draw_text(img, face(fonts, r.emphasis, r.heading), r.px, r.x, r.baseline + dy, &r.text, INK);
    }
}

/// Phase-1 image stand-in: a bordered box labelled with the alt text (or the URL's file name),
/// so the reader is legible before the image fetcher lands.
fn draw_image_placeholder(img: &mut RgbImage, fonts: &Fonts, x: f32, y: f32, w: f32, h: f32, alt: &str, url: &str) {
    rect_outline(img, x, y, w, h, FAINT, 2);
    let label = if !alt.is_empty() {
        alt.to_string()
    } else {
        url.rsplit('/').next().unwrap_or("image").to_string()
    };
    let label = render::draw::truncate(&fonts.sans, 26.0, &format!("🖼 {label}"), w - 40.0);
    draw_text(img, &fonts.sans, 26.0, x + 20.0, y + h / 2.0, &label, MUTED);
}

/// Copy a prepared (already display-sized) image onto the page at `(x, y)`, clipped to the canvas.
/// Row-wise `copy_from_slice` (both buffers are tightly packed RGB8) — a hero image is hundreds of
/// thousands of pixels, so the per-pixel bounds-checked loop this replaces was a real cost.
fn blit(dst: &mut RgbImage, src: &RgbImage, x: f32, y: f32) {
    let (dw, dh) = (dst.dimensions().0 as i64, dst.dimensions().1 as i64);
    let (sw, sh) = (src.dimensions().0 as i64, src.dimensions().1 as i64);
    let (ox, oy) = (x.round() as i64, y.round() as i64);
    // Clip once: source rows [sy0, sy1) land on dst rows oy+sy; columns likewise.
    let sx0 = (-ox).max(0);
    let sy0 = (-oy).max(0);
    let sx1 = sw.min(dw - ox);
    let sy1 = sh.min(dh - oy);
    if sx0 >= sx1 || sy0 >= sy1 {
        return;
    }
    let row = ((sx1 - sx0) * 3) as usize;
    let (src_raw, dst_w) = (src.as_raw(), dst.width() as i64);
    let dst_raw = dst.as_mut();
    for sy in sy0..sy1 {
        let s = ((sy * sw + sx0) * 3) as usize;
        let d = (((oy + sy) * dst_w + ox + sx0) * 3) as usize;
        dst_raw[d..d + row].copy_from_slice(&src_raw[s..s + row]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{layout, LayoutOpts};
    use crate::parse::parse;

    fn ink_count(img: &RgbImage) -> usize {
        img.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count()
    }

    #[test]
    fn a_prepared_image_is_sized_and_blitted_in_place_of_the_placeholder() {
        let doc = parse("Intro paragraph.\n\n![a chart](https://example.com/pic.png)\n\nAfter.");
        let fonts = Fonts::embedded();
        let opts = LayoutOpts::default();

        // A prepared 200×100 near-black image, keyed by the exact URL the parser recorded.
        let mut images = HashMap::new();
        images.insert("https://example.com/pic.png".to_string(), RgbImage::from_pixel(200, 100, Rgb([8, 8, 8])));

        // Layout sizes the image box to the prepared pixels and centres it in the text column.
        let pages = crate::layout::layout_with_images(&doc, &fonts, opts, &images);
        let (ix, iy) = pages
            .iter()
            .flat_map(|p| &p.items)
            .find_map(|it| match it {
                PageItem::Image { x, y, w, h, .. } => {
                    assert_eq!((*w, *h), (200.0, 100.0), "box should match the prepared image");
                    let expected_x = opts.margin_x + ((opts.page_w as f32 - 2.0 * opts.margin_x) - 200.0) / 2.0;
                    assert!((x - expected_x).abs() < 0.5, "image should be centred: {x} vs {expected_x}");
                    Some((*x, *y))
                }
                _ => None,
            })
            .expect("an Image item");

        // Paint blits the actual pixels: the 200×100 near-black block lands at (ix, iy).
        let page = pages.iter().find(|p| p.items.iter().any(|it| matches!(it, PageItem::Image { .. }))).unwrap();
        let img = draw_page_with_images(page, &fonts, opts.page_w, opts.page_h, &images);
        let mid = img.get_pixel((ix + 100.0) as u32, (iy + 50.0) as u32);
        assert!(mid[0] < 40 && mid[1] < 40 && mid[2] < 40, "image centre should be near-black, got {mid:?}");
        // A solid blit paints ~20k dark px; the placeholder outline paints only a few hundred.
        let dark = img.pixels().filter(|p| p[0] < 40 && p[1] < 40 && p[2] < 40).count();
        assert!(dark > 15_000, "expected a solid blit, got {dark} dark px");
    }

    #[test]
    fn blit_matches_the_per_pixel_reference_at_every_clipping_edge() {
        // The old loop, verbatim.
        fn reference(dst: &mut RgbImage, src: &RgbImage, x: f32, y: f32) {
            let (dw, dh) = dst.dimensions();
            let (ox, oy) = (x.round() as i64, y.round() as i64);
            for (sx, sy, px) in src.enumerate_pixels() {
                let (dx, dy) = (ox + sx as i64, oy + sy as i64);
                if dx >= 0 && dy >= 0 && (dx as u32) < dw && (dy as u32) < dh {
                    dst.put_pixel(dx as u32, dy as u32, *px);
                }
            }
        }
        let src = RgbImage::from_fn(30, 20, |x, y| Rgb([x as u8 * 7, y as u8 * 11, (x + y) as u8]));
        for (x, y) in [
            (10.0, 10.0),           // interior
            (-5.4, -3.6),           // overhang top-left (rounds to -5, -4)
            (85.0, 45.0),           // overhang bottom-right (dst 100×50)
            (-40.0, 10.0),          // fully off left
            (10.0, 60.0),           // fully off bottom
            (70.0, 30.0),           // exact fit to the corner
            (0.49, 0.51),           // rounding both ways
        ] {
            let mut fast = RgbImage::from_pixel(100, 50, Rgb([9, 9, 9]));
            let mut slow = fast.clone();
            blit(&mut fast, &src, x, y);
            reference(&mut slow, &src, x, y);
            assert_eq!(fast, slow, "blit at ({x},{y})");
        }
    }

    #[test]
    fn a_page_renders_identically_cold_and_warm() {
        // The render crate's glyph-mask cache must be invisible: the first draw (cold, rasterizing)
        // and a repeat draw (warm, replaying cached masks) produce byte-identical frames.
        let src = "# A Heading\n\nBody text with ==a highlight== and *emphasis*, repeated. ".repeat(8);
        let doc = parse(&src);
        let fonts = Fonts::embedded();
        let opts = LayoutOpts::default();
        let pages = layout(&doc, &fonts, opts);
        let cold = draw_page(&pages[0], &fonts, opts.page_w, opts.page_h);
        let warm = draw_page(&pages[0], &fonts, opts.page_w, opts.page_h);
        assert_eq!(cold, warm);
    }

    #[test]
    fn renders_substantial_ink_and_highlight_color() {
        let src = "# Title\n\nA paragraph with ==a highlighted phrase== inside it, long enough to draw.";
        let doc = parse(src);
        let fonts = Fonts::embedded();
        let opts = LayoutOpts::default();
        let pages = layout(&doc, &fonts, opts);
        let img = draw_page(&pages[0], &fonts, opts.page_w, opts.page_h);
        assert_eq!(img.dimensions(), (954, 1696));
        assert!(ink_count(&img) > 2000, "expected real text ink");
        // The yellow highlight wash must appear somewhere.
        let yellow = img.pixels().filter(|p| p[0] > 230 && p[1] > 200 && p[2] < 180).count();
        assert!(yellow > 500, "expected a highlight wash, got {yellow}");
    }
}
