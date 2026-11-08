//! Raster drawing primitives shared by the suspend-screen renderer and the planner UI.
//! All coordinates are f32 pixels on an RGB buffer (the Move's Gallery 3 panel is color;
//! greyscale is just ink == grey). Ink `Rgb([0,0,0])` = black, `Rgb([255,255,255])` = white.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};
use image::{Rgb, RgbImage};
use std::cell::RefCell;
use std::collections::HashMap;

/* ── Glyph raster cache ─────────────────────────────────────────────────────
A full page re-rasterizes ~2,500–3,000 glyph outlines per redraw for only ~80 distinct glyphs, so
the outline/scan-convert step — the dominant text-drawing cost — is ~97% redundant. The coverage
mask is memoized per (face, px, glyph, sub-pixel position) and replayed through the same blend.

Bit-exactness (the reason the fraction bits key is sufficient): ab_glyph 0.2.x's
`Outline::px_bounds` decomposes the glyph position into trunc + fract and rounds bounds as
`(min·factor + fract).floor() + trunc`, and `OutlinedGlyph::draw` offsets curves by
`position − px_bounds.min` — both exact in f32 for canvas-sized coordinates. So two positions with
bit-equal `fract()` produce identical masks at an integer translation, and a cached mask replays
byte-identically. (Pinned by the reference tests below; revalidate if ab_glyph is ever bumped.) */

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct MaskKey {
    /// The `&FontRef`'s address — stable for the app's single `Fonts` — plus a metrics fingerprint
    /// ([`face_fingerprint`]) so an address reused by a *different* face can never alias masks (the
    /// embedded faces have pairwise-distinct fingerprints; see the test pinning that invariant).
    face_addr: usize,
    fingerprint: u32,
    px_bits: u32,
    gid: u16,
    fx_bits: u32,
    fy_bits: u32,
}

/// A cheap per-face fingerprint: glyph count mixed with the unscaled advance metrics of two
/// reference glyphs. Distinguishes same-family weights (Plex Regular vs SemiBold share a character
/// set but not advance widths). Two cmap + hmtx lookups — negligible next to drawing a string.
fn face_fingerprint(font: &FontRef) -> u32 {
    let m = font.h_advance_unscaled(font.glyph_id('M')).to_bits();
    let i = font.h_advance_unscaled(font.glyph_id('i')).to_bits();
    (font.glyph_count() as u32).wrapping_mul(31).wrapping_add(m).wrapping_mul(31).wrapping_add(i)
}

/// One rasterized glyph: exact coverage values, positioned relative to the glyph's `trunc()`.
struct Mask {
    rel_min_x: i32,
    rel_min_y: i32,
    w: u32,
    h: u32,
    cov: Box<[f32]>,
}

struct MaskCache {
    map: HashMap<MaskKey, Mask>,
    cov_bytes: usize,
}

/// Every page renders ~4–6 MB of masks (fraction bits differ per word position), so the cache is
/// cleared wholesale past this cap: ~1–2 pages stay resident, which keeps the critical warm path —
/// repeated redraws of the *same* page (menus, flashes, toggles, keystrokes) — at ~100% hits.
const MASK_CACHE_MAX_BYTES: usize = 8 << 20;

thread_local! {
    static GLYPH_MASKS: RefCell<MaskCache> = RefCell::new(MaskCache { map: HashMap::new(), cov_bytes: 0 });
}

/// Blend one glyph at `(pos_x + extra_dx, pos_y + extra_dy)` via the cache. The blend expression is
/// the pre-cache code verbatim; a hit skips only the outline/scan-convert step. `extra_dx/dy` are
/// the integer centring offsets [`icon`] applies after rasterizing at the origin.
#[allow(clippy::too_many_arguments)]
fn draw_glyph_cached(img: &mut RgbImage, face_addr: usize, fingerprint: u32, font: &FontRef, px: f32, gid: GlyphId, pos_x: f32, pos_y: f32, extra_dx: i32, extra_dy: i32, ink: Rgb<u8>) {
    let key = MaskKey { face_addr, fingerprint, px_bits: px.to_bits(), gid: gid.0, fx_bits: pos_x.fract().to_bits(), fy_bits: pos_y.fract().to_bits() };
    GLYPH_MASKS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.map.contains_key(&key) {
            let glyph = gid.with_scale_and_position(PxScale::from(px), ab_glyph::point(pos_x, pos_y));
            let mask = match font.outline_glyph(glyph) {
                Some(outline) => {
                    let bb = outline.px_bounds();
                    let (mw, mh) = (bb.width() as u32, bb.height() as u32);
                    let mut cov = vec![0.0f32; (mw * mh) as usize].into_boxed_slice();
                    outline.draw(|gx, gy, c| {
                        if gx < mw && gy < mh {
                            cov[(gy * mw + gx) as usize] = c;
                        }
                    });
                    Mask { rel_min_x: bb.min.x as i32 - pos_x.trunc() as i32, rel_min_y: bb.min.y as i32 - pos_y.trunc() as i32, w: mw, h: mh, cov }
                }
                // Cache empty outlines too — spaces are the most frequent glyph on a page.
                None => Mask { rel_min_x: 0, rel_min_y: 0, w: 0, h: 0, cov: Box::new([]) },
            };
            if cache.cov_bytes + mask.cov.len() * 4 > MASK_CACHE_MAX_BYTES {
                cache.map.clear();
                cache.cov_bytes = 0;
            }
            cache.cov_bytes += mask.cov.len() * 4;
            cache.map.insert(key, mask);
        }
        let mask = &cache.map[&key];
        let (w, h) = (img.width() as i32, img.height() as i32);
        let x0 = pos_x.trunc() as i32 + mask.rel_min_x + extra_dx;
        let y0 = pos_y.trunc() as i32 + mask.rel_min_y + extra_dy;
        for gy in 0..mask.h {
            let yy = y0 + gy as i32;
            if yy < 0 || yy >= h {
                continue;
            }
            let row = (gy * mask.w) as usize;
            for gx in 0..mask.w {
                let cov = mask.cov[row + gx as usize];
                // Blending exact 0.0 coverage is the identity (bg·1 + ink·0 rounds back to bg), so
                // skipping it is byte-identical — and most of a glyph's bounding box is empty.
                if cov == 0.0 {
                    continue;
                }
                let xx = x0 + gx as i32;
                if xx >= 0 && xx < w {
                    let p = img.get_pixel_mut(xx as u32, yy as u32);
                    for c in 0..3 {
                        let bg = p[c] as f32;
                        p[c] = (bg * (1.0 - cov) + ink[c] as f32 * cov).round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    });
}

/// Draw `text` with its baseline at `(x, baseline)`. Returns the advance width.
pub fn draw_text(img: &mut RgbImage, font: &FontRef, px: f32, x: f32, baseline: f32, text: &str, ink: Rgb<u8>) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let face_addr = font as *const FontRef as usize;
    let fingerprint = face_fingerprint(font);
    let mut caret = x;
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let gid = scaled.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, gid);
        }
        draw_glyph_cached(img, face_addr, fingerprint, font, px, gid, caret, baseline, 0, 0, ink);
        caret += scaled.h_advance(gid);
        prev = Some(gid);
    }
    caret - x
}

/// Draw a single icon-font `glyph` centred on `(cx, cy)`, scaled to fit within a `size × size` box and
/// coverage-blended like text. Icon fonts don't share text's baseline and each glyph fills its em
/// differently, so we probe the glyph's real pixel bounds, rescale so its *larger* dimension == `size`
/// (fitting square glyphs to the box and keeping wide ones like `three-dots` from overflowing), and
/// centre that box on the point. No-op if the font lacks the glyph (empty outline).
pub fn icon(img: &mut RgbImage, font: &FontRef, glyph: char, cx: f32, cy: f32, size: f32, ink: Rgb<u8>) {
    let gid = font.glyph_id(glyph);
    // Probe at a reference scale to learn the glyph's true pixel bounds, then pick the scale that makes
    // its larger dimension exactly `size` — so a wide, short glyph fits the box instead of blowing up.
    let probe = gid.with_scale_and_position(PxScale::from(100.0), ab_glyph::point(0.0, 0.0));
    let Some(p0) = font.outline_glyph(probe) else { return };
    let b0 = p0.px_bounds();
    let scale = size / b0.height().max(b0.width()).max(1.0) * 100.0;
    // Need the placed bounds to centre the box; outline_glyph here is bounds-only (cheap) — the
    // expensive scan-convert happens inside the cached draw below.
    let placed = gid.with_scale_and_position(PxScale::from(scale), ab_glyph::point(0.0, 0.0));
    let Some(outline) = font.outline_glyph(placed) else { return };
    let bb = outline.px_bounds();
    let ox = (cx - (bb.min.x + bb.max.x) / 2.0).round();
    let oy = (cy - (bb.min.y + bb.max.y) / 2.0).round();
    // Rasterized at the origin (fraction bits 0) and translated by the integer centring offset —
    // identical addressing to the old inline draw (`bb.min + ox + gx`).
    let face_addr = font as *const FontRef as usize;
    draw_glyph_cached(img, face_addr, face_fingerprint(font), font, scale, gid, 0.0, 0.0, ox as i32, oy as i32, ink);
}

pub fn measure(font: &FontRef, px: f32, text: &str) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let gid = scaled.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, gid);
        }
        w += scaled.h_advance(gid);
        prev = Some(gid);
    }
    w
}

/// Truncate `text` with an ellipsis so it fits `max_w`. Char widths accumulate kerning-free
/// (`h_advance` per char == `measure` of that char alone), so truncation points are unchanged from
/// the old per-char `measure` loop — just without allocating a `String` per char.
pub fn truncate(font: &FontRef, px: f32, text: &str, max_w: f32) -> String {
    if measure(font, px, text) <= max_w {
        return text.to_string();
    }
    let ell = "…";
    let ell_w = measure(font, px, ell);
    let scaled = font.as_scaled(PxScale::from(px));
    let mut out = String::new();
    let mut w = 0.0;
    for ch in text.chars() {
        let cw = scaled.h_advance(scaled.glyph_id(ch));
        if w + cw + ell_w > max_w {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

pub fn hline(img: &mut RgbImage, x0: f32, x1: f32, y: f32, ink: Rgb<u8>, thick: u32) {
    let (w, h) = (img.width(), img.height());
    for t in 0..thick {
        let yy = y as i32 + t as i32;
        if yy < 0 || yy as u32 >= h {
            continue;
        }
        for xx in (x0.max(0.0) as u32)..((x1 as u32).min(w)) {
            img.put_pixel(xx, yy as u32, ink);
        }
    }
}

pub fn vline(img: &mut RgbImage, x: f32, y0: f32, y1: f32, ink: Rgb<u8>, thick: u32) {
    let (w, h) = (img.width(), img.height());
    for t in 0..thick {
        let xx = x as i32 + t as i32;
        if xx < 0 || xx as u32 >= w {
            continue;
        }
        for yy in (y0.max(0.0) as u32)..((y1 as u32).min(h)) {
            img.put_pixel(xx as u32, yy, ink);
        }
    }
}

/// An empty checkbox outline (top-left at `(x, y)`).
pub fn checkbox(img: &mut RgbImage, x: f32, y: f32, size: f32, ink: Rgb<u8>) {
    hline(img, x, x + size, y, ink, 2);
    hline(img, x, x + size, y + size, ink, 2);
    vline(img, x, y, y + size, ink, 2);
    vline(img, x + size, y, y + size + 2.0, ink, 2);
}

/// A tick mark filling the square at `(x, y)` (drawn, so no font has to carry U+2713).
pub fn tick(img: &mut RgbImage, x: f32, y: f32, size: f32, ink: Rgb<u8>, thick: u32) {
    line(img, x + size * 0.10, y + size * 0.55, x + size * 0.38, y + size * 0.82, ink, thick);
    line(img, x + size * 0.38, y + size * 0.82, x + size * 0.90, y + size * 0.18, ink, thick);
}

/// A checked checkbox: outline plus a tick.
pub fn checkbox_checked(img: &mut RgbImage, x: f32, y: f32, size: f32, ink: Rgb<u8>) {
    checkbox(img, x, y, size, ink);
    tick(img, x + size * 0.12, y + size * 0.05, size * 0.76, ink, 3);
}

/// A small "recurring" cycle icon centered at `(cx, cy)` (replaces the ↻ text glyph).
pub fn recur_arrows(img: &mut RgbImage, cx: f32, cy: f32, r: f32, ink: Rgb<u8>) {
    // Two opposing arcs approximated by chords, each with an arrowhead.
    let pts_top = [(cx - r, cy), (cx - r * 0.6, cy - r * 0.87), (cx + r * 0.6, cy - r * 0.87), (cx + r, cy)];
    let pts_bot = [(cx + r, cy), (cx + r * 0.6, cy + r * 0.87), (cx - r * 0.6, cy + r * 0.87), (cx - r, cy)];
    for pair in pts_top.windows(2) {
        line(img, pair[0].0, pair[0].1, pair[1].0, pair[1].1, ink, 2);
    }
    for pair in pts_bot.windows(2).take(2) {
        line(img, pair[0].0, pair[0].1, pair[1].0, pair[1].1, ink, 2);
    }
    // arrowheads
    line(img, cx + r, cy, cx + r - 6.0, cy - 7.0, ink, 2);
    line(img, cx + r, cy, cx + r + 5.0, cy - 6.0, ink, 2);
    line(img, cx - r * 0.6, cy + r * 0.87, cx - r * 0.6 + 7.0, cy + r * 0.87 - 5.0, ink, 2);
    line(img, cx - r * 0.6, cy + r * 0.87, cx - r * 0.6 + 6.0, cy + r * 0.87 + 5.0, ink, 2);
}

/// A filled disc (the NOW dot on the day rail).
pub fn disc(img: &mut RgbImage, cx: f32, cy: f32, r: f32, ink: Rgb<u8>) {
    let ri = r.ceil() as i32;
    for dy in -ri..=ri {
        let half = (r * r - (dy * dy) as f32).max(0.0).sqrt();
        hline(img, cx - half, cx + half, cy + dy as f32, ink, 1);
    }
}

/// Draw text with extra letterspacing (for eyebrow labels). Returns the advance width.
pub fn draw_text_tracked(
    img: &mut RgbImage,
    font: &FontRef,
    px: f32,
    x: f32,
    baseline: f32,
    text: &str,
    ink: Rgb<u8>,
    tracking: f32,
) -> f32 {
    let mut caret = x;
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        caret += draw_text(img, font, px, caret, baseline, ch.encode_utf8(&mut buf), ink) + tracking;
    }
    caret - x
}

/// A straight line between two points (for tick marks and arrows).
pub fn line(img: &mut RgbImage, x0: f32, y0: f32, x1: f32, y1: f32, ink: Rgb<u8>, thick: u32) {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0);
    let r = thick as f32 / 2.0;
    for i in 0..=(steps as u32) {
        let t = i as f32 / steps;
        fill_rect(img, x0 + dx * t - r, y0 + dy * t - r, thick as f32, thick as f32, ink);
    }
}

pub fn fill_rect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, ink: Rgb<u8>) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let x0 = (x as i32).max(0);
    let y0 = (y as i32).max(0);
    let x1 = ((x + w) as i32).min(iw);
    let y1 = ((y + h) as i32).min(ih);
    for yy in y0..y1 {
        for xx in x0..x1 {
            img.put_pixel(xx as u32, yy as u32, ink);
        }
    }
}

pub fn rect_outline(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, ink: Rgb<u8>, thick: u32) {
    hline(img, x, x + w, y, ink, thick);
    hline(img, x, x + w, y + h - thick as f32, ink, thick);
    vline(img, x, y, y + h, ink, thick);
    vline(img, x + w - thick as f32, y, y + h, ink, thick);
}

/// Invert a region (instant tap feedback before any real work happens).
pub fn invert_rect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    let x0 = (x as i32).max(0);
    let y0 = (y as i32).max(0);
    let x1 = ((x + w) as i32).min(iw);
    let y1 = ((y + h) as i32).min(ih);
    for yy in y0..y1 {
        for xx in x0..x1 {
            let p = img.get_pixel_mut(xx as u32, yy as u32);
            for c in 0..3 {
                p[c] = 255 - p[c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Rgb<u8> = Rgb([0, 0, 0]);
    const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

    fn blank() -> RgbImage {
        RgbImage::from_pixel(100, 100, WHITE)
    }
    fn dark(p: &Rgb<u8>) -> bool {
        (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384
    }

    /// The pre-cache `draw_text`, verbatim — the byte-exactness oracle for the glyph mask cache.
    fn draw_text_uncached(img: &mut RgbImage, font: &FontRef, px: f32, x: f32, baseline: f32, text: &str, ink: Rgb<u8>) -> f32 {
        let scaled = font.as_scaled(PxScale::from(px));
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut caret = x;
        let mut prev: Option<GlyphId> = None;
        for ch in text.chars() {
            let gid = scaled.glyph_id(ch);
            if let Some(p) = prev {
                caret += scaled.kern(p, gid);
            }
            let glyph = gid.with_scale_and_position(PxScale::from(px), ab_glyph::point(caret, baseline));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bb = outline.px_bounds();
                outline.draw(|gx, gy, cov| {
                    let xx = bb.min.x as i32 + gx as i32;
                    let yy = bb.min.y as i32 + gy as i32;
                    if xx >= 0 && yy >= 0 && xx < w && yy < h {
                        let p = img.get_pixel_mut(xx as u32, yy as u32);
                        for c in 0..3 {
                            let bg = p[c] as f32;
                            p[c] = (bg * (1.0 - cov) + ink[c] as f32 * cov).round().clamp(0.0, 255.0) as u8;
                        }
                    }
                });
            }
            caret += scaled.h_advance(gid);
            prev = Some(gid);
        }
        caret - x
    }

    /// The pre-cache `icon`, verbatim.
    fn icon_uncached(img: &mut RgbImage, font: &FontRef, glyph: char, cx: f32, cy: f32, size: f32, ink: Rgb<u8>) {
        let (w, h) = (img.width() as i32, img.height() as i32);
        let gid = font.glyph_id(glyph);
        let probe = gid.with_scale_and_position(PxScale::from(100.0), ab_glyph::point(0.0, 0.0));
        let Some(p0) = font.outline_glyph(probe) else { return };
        let b0 = p0.px_bounds();
        let scale = size / b0.height().max(b0.width()).max(1.0) * 100.0;
        let placed = gid.with_scale_and_position(PxScale::from(scale), ab_glyph::point(0.0, 0.0));
        let Some(outline) = font.outline_glyph(placed) else { return };
        let bb = outline.px_bounds();
        let ox = (cx - (bb.min.x + bb.max.x) / 2.0).round();
        let oy = (cy - (bb.min.y + bb.max.y) / 2.0).round();
        outline.draw(|gx, gy, cov| {
            let xx = bb.min.x as i32 + ox as i32 + gx as i32;
            let yy = bb.min.y as i32 + oy as i32 + gy as i32;
            if xx >= 0 && yy >= 0 && xx < w && yy < h {
                let p = img.get_pixel_mut(xx as u32, yy as u32);
                for c in 0..3 {
                    let bg = p[c] as f32;
                    p[c] = (bg * (1.0 - cov) + ink[c] as f32 * cov).round().clamp(0.0, 255.0) as u8;
                }
            }
        });
    }

    #[test]
    fn glyph_masks_replay_bit_identically_across_positions() {
        // Draw the same strings through the cache and through the pre-cache oracle at many
        // positions. Positions repeat sub-pixel fractions at different integer offsets (13.25 then
        // 803.25 …) so later draws replay masks rasterized earlier elsewhere — the exact reuse the
        // cache exists for. Every frame must match byte-for-byte, cold and warm.
        let fonts = crate::Fonts::embedded();
        let texts = ["The quick brown fox, 42 — “fine”.", "WAVE To Yield karma", "iiillliii"];
        let mut k = 0u32;
        for font in [&fonts.sans, &fonts.sans_semibold, &fonts.mono] {
            for px in [22.0f32, 38.0, 52.44] {
                for base_x in [13.25f32, 803.25, 56.0, 56.5, -0.75, 402.3] {
                    for base_y in [40.72f32, 900.72, 60.0, 1650.5] {
                        let text = texts[(k as usize) % texts.len()];
                        k += 1;
                        let mut cached = RgbImage::from_pixel(954, 200, WHITE);
                        let mut oracle = RgbImage::from_pixel(954, 200, WHITE);
                        // Map tall y positions into the strip while keeping their fract bits.
                        let y = base_y % 180.0;
                        let wc = draw_text(&mut cached, font, px, base_x, y, text, BLACK);
                        let wu = draw_text_uncached(&mut oracle, font, px, base_x, y, text, BLACK);
                        assert_eq!(wc, wu, "advance differs at px={px} x={base_x} y={y}");
                        assert_eq!(cached, oracle, "pixels differ at px={px} x={base_x} y={y}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_warm_cache_render_equals_the_cold_one() {
        let fonts = crate::Fonts::embedded();
        let paint = |img: &mut RgbImage| {
            for (i, s) in ["A Clean, Well-Lighted Place", "the dew settled the dust", "42 min left"].iter().enumerate() {
                draw_text(img, &fonts.serif_bold, 44.5, 56.3, 60.7 + i as f32 * 74.52, s, BLACK);
            }
        };
        let mut cold = RgbImage::from_pixel(954, 300, WHITE);
        paint(&mut cold); // populates the cache
        let mut warm = RgbImage::from_pixel(954, 300, WHITE);
        paint(&mut warm); // pure replay
        assert_eq!(cold, warm);
    }

    #[test]
    fn icon_cached_equals_uncached() {
        let fonts = crate::Fonts::embedded();
        // `icon` works for any face; letters exercise the probe→scale→centre path the same way.
        for (glyph, cx, cy, size) in [('A', 60.0, 60.0, 44.0), ('W', 300.5, 80.25, 60.0), ('o', 12.0, 12.0, 30.0)] {
            let mut cached = RgbImage::from_pixel(400, 160, WHITE);
            let mut oracle = RgbImage::from_pixel(400, 160, WHITE);
            icon(&mut cached, &fonts.sans, glyph, cx, cy, size, BLACK);
            icon_uncached(&mut oracle, &fonts.sans, glyph, cx, cy, size, BLACK);
            assert_eq!(cached, oracle, "icon {glyph} at ({cx},{cy}) size {size}");
        }
    }

    #[test]
    fn embedded_faces_have_pairwise_distinct_fingerprints() {
        // The cache fingerprints a face by glyph count + reference-glyph metrics (plus address): if
        // two different faces ever shared a fingerprint, an address reused across `Fonts` instances
        // could alias masks. Extend `face_fingerprint` if adding a font breaks this.
        let fonts = crate::Fonts::embedded();
        let fps = [face_fingerprint(&fonts.sans), face_fingerprint(&fonts.sans_semibold), face_fingerprint(&fonts.serif_bold), face_fingerprint(&fonts.mono), face_fingerprint(&fonts.icons)];
        for i in 0..fps.len() {
            for j in i + 1..fps.len() {
                assert_ne!(fps[i], fps[j], "faces {i} and {j} share fingerprint {}", fps[i]);
            }
        }
    }

    #[test]
    fn truncate_matches_the_per_char_measure_reference() {
        // The old body, verbatim: per-char `measure` of a 1-char String. The new loop must make
        // byte-identical truncation decisions (h_advance == measure of a lone char).
        fn reference(font: &FontRef, px: f32, text: &str, max_w: f32) -> String {
            if measure(font, px, text) <= max_w {
                return text.to_string();
            }
            let ell = "…";
            let ell_w = measure(font, px, ell);
            let mut out = String::new();
            let mut w = 0.0;
            for ch in text.chars() {
                let cw = measure(font, px, &ch.to_string());
                if w + cw + ell_w > max_w {
                    break;
                }
                out.push(ch);
                w += cw;
            }
            out.push('…');
            out
        }
        let fonts = crate::Fonts::embedded();
        let samples = [
            "A Clean, Well-Lighted Place — Hemingway",
            "iiiiiiiiiiiiiiiiiiiillllllll",
            "WWWWWWWWWW wide capitals WWWW",
            "short",
            "unicode — “curly” … déjà-vu ﬁnal",
        ];
        for font in [&fonts.sans, &fonts.sans_semibold, &fonts.mono] {
            for px in [22.0f32, 26.0, 34.0, 44.5] {
                for s in samples {
                    let full = measure(font, px, s);
                    for max_w in [full + 1.0, full, full * 0.75, full * 0.5, full * 0.25, 30.0, 0.0] {
                        assert_eq!(truncate(font, px, s, max_w), reference(font, px, s, max_w), "font px={px} s={s:?} max_w={max_w}");
                    }
                }
            }
        }
    }

    #[test]
    fn invert_is_involution() {
        let mut img = blank();
        fill_rect(&mut img, 10.0, 10.0, 20.0, 20.0, Rgb([213, 0, 0]));
        let before = img.clone();
        invert_rect(&mut img, 0.0, 0.0, 100.0, 100.0);
        assert_ne!(before, img);
        invert_rect(&mut img, 0.0, 0.0, 100.0, 100.0);
        assert_eq!(before, img);
    }

    #[test]
    fn checked_checkbox_has_more_ink_than_empty() {
        let mut empty = blank();
        checkbox(&mut empty, 20.0, 20.0, 40.0, BLACK);
        let mut checked = blank();
        checkbox_checked(&mut checked, 20.0, 20.0, 40.0, BLACK);
        let ink = |i: &RgbImage| i.pixels().filter(|p| dark(p)).count();
        assert!(ink(&checked) > ink(&empty));
    }

    #[test]
    fn primitives_clip_at_edges_without_panicking() {
        let mut img = blank();
        fill_rect(&mut img, -10.0, -10.0, 200.0, 5.0, BLACK);
        line(&mut img, -20.0, 50.0, 200.0, 120.0, BLACK, 3);
        rect_outline(&mut img, 90.0, 90.0, 50.0, 50.0, BLACK, 2);
        invert_rect(&mut img, 95.0, 95.0, 50.0, 50.0);
    }

    #[test]
    fn colored_fill_keeps_channels() {
        let mut img = blank();
        fill_rect(&mut img, 0.0, 0.0, 10.0, 10.0, Rgb([246, 191, 38]));
        assert_eq!(img.get_pixel(5, 5), &Rgb([246, 191, 38]));
    }
}
