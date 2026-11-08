//! Generate the AppLoad launcher icons for all six apps.
//!
//! Apple-official-icon language, ALL-WHITE edition (the white icons read best on
//! the launcher, so the whole family is white): every icon is a white squircle with a quiet grey
//! keyline — without it a white icon dissolves into the launcher's paper background — and ONE
//! bold coloured glyph that is the app's own UI in miniature. Flat colour only — gradients wash
//! out / band on the Move's Gallery 3 panel. Glyph colours are the apps' real vivid accents.
//!
//! Craft: we draw at 4× on the render crate's hard-edged raster primitives, then downscale with
//! Lanczos3 so the squircle rim and every glyph edge come out smoothly anti-aliased. The squircle
//! is a supersampled alpha mask; the keyline is the band between the outer mask and an inset
//! twin, welded in at finish time so it hugs the rim exactly.
//!
//! Run:  cargo run -p render --example appicons
//! Writes deploy/appload/{agenda,reader,browser,habits,recipes,onething}/icon.png.

use image::{imageops, GrayImage, Luma, Rgb, RgbImage, Rgba, RgbaImage};
use render::draw::{disc, fill_rect};
use render::Fonts;
use std::fs;
use std::path::Path;

const SS: u32 = 4; // supersample factor
const OUT: u32 = 512; // final icon edge
const N: u32 = OUT * SS; // working edge (2048²)

// Brand palette — the apps' real (vivid, v0.20.2) accents, now as glyph colours on white.
const TOMATO: Rgb<u8> = Rgb([228, 87, 46]); // Agenda accent (Calendar red's role)
const INDIGO: Rgb<u8> = Rgb([42, 46, 58]); // Kindle book ink
const WHITE: Rgb<u8> = Rgb([255, 255, 255]);
const INK: Rgb<u8> = Rgb([20, 20, 24]); // near-black numeral/text ink
const HIGHLIGHT: Rgb<u8> = Rgb([250, 232, 120]); // the Reader's highlight yellow
const TEXT_GREY: Rgb<u8> = Rgb([150, 157, 171]); // cool grey "text" lines
const SAGE: Rgb<u8> = Rgb([51, 182, 121]); // Habits done-green
const GREEN: Rgb<u8> = Rgb([32, 138, 92]); // One Thing brand green
const GOLD: Rgb<u8> = Rgb([230, 170, 40]); // shared gold accent (dial tip, pot knob)
const MAROON: Rgb<u8> = Rgb([140, 58, 74]); // Recipes pot
const KEYLINE: Rgb<u8> = Rgb([185, 185, 185]); // the rim that keeps white-on-white visible

fn main() {
    let root = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .expect("workspace root resolves");
    let fonts = Fonts::embedded();
    let outer = squircle_mask(0.0);
    let inner = squircle_mask(7.0); // keyline ≈ 7px at 512 — a confident hairline, not a border
    agenda(&outer, &inner, &fonts, &root.join("deploy/appload/agenda/icon.png"));
    reader(&outer, &inner, &root.join("deploy/appload/reader/icon.png"));
    kindle(&outer, &inner, &root.join("deploy/appload/browser/icon.png"));
    habits(&outer, &inner, &root.join("deploy/appload/habits/icon.png"));
    recipes(&outer, &inner, &root.join("deploy/appload/recipes/icon.png"));
    onething(&outer, &inner, &root.join("deploy/appload/onething/icon.png"));
}

/// Agenda — Fantastical-style, CONTAINED like the rest of the family: a
/// rounded calendar card floating in white space — tomato header, white month body with a faint
/// cell grid, a few whole cells filled solid in the app's real priority palette (Sage / Banana /
/// Tangerine / Tomato — the planner's own rows).
fn agenda(outer: &GrayImage, inner: &GrayImage, _fonts: &Fonts, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let k = SS as f32;
    const GRID: Rgb<u8> = Rgb([214, 218, 224]); // hairline cell borders + card outline
    // The card: x 96..416, y 104..408, radius 36; header 84 tall.
    let (cx0, cy0, cw_all, ch_all, r) = (96.0, 104.0, 320.0, 304.0, 36.0);
    let hh = 84.0;
    rrect(&mut img, cx0 - 3.0, cy0 - 3.0, cw_all + 6.0, ch_all + 6.0, r + 3.0, GRID); // outline
    rrect(&mut img, cx0, cy0, cw_all, ch_all, r, WHITE); // body
    // Header: rounded card top; its bottom half squared off by the body overlap.
    rrect(&mut img, cx0, cy0, cw_all, hh + r, r, TOMATO);
    fill_rect(&mut img, cx0 * k, (cy0 + hh) * k, cw_all * k, r * k, WHITE);
    // Month grid inside the body: 4 columns × 3 rows.
    let cols = 4.0;
    let rows = 3.0;
    let cw = cw_all / cols;
    let rh = (ch_all - hh) / rows;
    let cell = |img: &mut RgbImage, c: f32, rr: f32, colour: Rgb<u8>| {
        fill_rect(
            img,
            (cx0 + c * cw) * k,
            (cy0 + hh + rr * rh) * k,
            cw * k,
            rh * k,
            colour,
        );
    };
    cell(&mut img, 0.0, 0.0, SAGE); // the planner's P4..P1 palette as busy days
    cell(&mut img, 2.0, 1.0, Rgb([246, 191, 38])); // Banana
    cell(&mut img, 1.0, 2.0, Rgb([244, 81, 30])); // Tangerine
    cell(&mut img, 3.0, 1.0, TOMATO); // (not row 0 — it would fuse with the tomato header)
    for i in 1..4 {
        fill_rect(&mut img, (cx0 + i as f32 * cw) * k - 2.0 * k, (cy0 + hh) * k, 4.0 * k, (ch_all - hh) * k, GRID);
    }
    for j in 1..3 {
        fill_rect(&mut img, cx0 * k, (cy0 + hh + j as f32 * rh) * k - 2.0 * k, cw_all * k, 4.0 * k, GRID);
    }
    // Round the body's bottom corners back off (grid/cell fills squared them).
    finish(img, outer, inner, out);
}

/// Reader — the light page: grey text lines, the signature yellow highlighter swipe with a dark
/// line riding it.
fn reader(outer: &GrayImage, inner: &GrayImage, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let inl = 116.0;
    let inw = 280.0;
    let line_h = 18.0;
    let rows = [inw, inw, inw, inw, inw * 0.62];
    for (i, &w) in rows.iter().enumerate() {
        let y = 124.0 + i as f32 * 62.0;
        if i == 2 {
            rrect(&mut img, inl - 12.0, y - 14.0, inw + 24.0, 46.0, 14.0, HIGHLIGHT);
            rrect(&mut img, inl, y, inw * 0.86, line_h, line_h / 2.0, INK);
        } else {
            rrect(&mut img, inl, y, w, line_h, line_h / 2.0, TEXT_GREY);
        }
    }
    finish(img, outer, inner, out);
}

/// Kindle — Apple Books, inverted for the white family: an indigo open-book spread — two filled
/// pages meeting at a spine gap, text lines knocked out in white. No highlight (the Reader's).
fn kindle(outer: &GrayImage, inner: &GrayImage, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let (top, ph) = (140.0, 232.0);
    let pw = 128.0;
    let gap = 14.0;
    let cx = 256.0;
    for x in [cx - gap / 2.0 - pw, cx + gap / 2.0] {
        rrect(&mut img, x, top, pw, ph, 22.0, INDIGO);
    }
    for page in 0..2 {
        let inl = if page == 0 { cx - gap / 2.0 - pw + 26.0 } else { cx + gap / 2.0 + 26.0 };
        let inw = pw - 52.0;
        for i in 0..4 {
            let w = if i == 3 { inw * 0.6 } else { inw };
            rrect(&mut img, inl, top + 44.0 + i as f32 * 46.0, w, 12.0, 6.0, WHITE);
        }
    }
    // The spine base: an indigo tab bridging the pages at the bottom (the open book's V).
    rrect(&mut img, cx - gap / 2.0 - 8.0, top + ph - 26.0, gap + 16.0, 26.0, 8.0, INDIGO);
    finish(img, outer, inner, out);
}

/// Habits — Apple Reminders on white: the app's own list rows — sage done-discs, one not-yet
/// ring, grey line marks.
fn habits(outer: &GrayImage, inner: &GrayImage, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let k = SS as f32;
    // Equal-width rows so the block is symmetric and optical centre == geometric centre:
    // discs at cx 156 (left edge 122), lines 212..390 → content spans x 122..390, centre 256.
    let rows = [true, true, false];
    for (i, &done) in rows.iter().enumerate() {
        let cy = (156.0 + i as f32 * 100.0) * k;
        let cx = 156.0 * k;
        if done {
            disc(&mut img, cx, cy, 34.0 * k, SAGE); // filled = done, the app's own mark
        } else {
            disc(&mut img, cx, cy, 34.0 * k, SAGE);
            disc(&mut img, cx, cy, 24.0 * k, WHITE); // ring = not yet
        }
        rrect(&mut img, 212.0, 156.0 + i as f32 * 100.0 - 11.0, 178.0, 22.0, 11.0, TEXT_GREY);
    }
    finish(img, outer, inner, out);
}

/// Recipes — the maroon pot with the app's gold lid knob, on white.
fn recipes(outer: &GrayImage, inner: &GrayImage, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let k = SS as f32;
    rrect(&mut img, 136.0, 226.0, 240.0, 140.0, 34.0, MAROON); // body
    rrect(&mut img, 104.0, 248.0, 40.0, 26.0, 13.0, MAROON); // handles
    rrect(&mut img, 368.0, 248.0, 40.0, 26.0, 13.0, MAROON);
    rrect(&mut img, 148.0, 186.0, 216.0, 34.0, 17.0, MAROON); // lid
    disc(&mut img, 256.0 * k, 168.0 * k, 22.0 * k, GOLD); // knob
    finish(img, outer, inner, out);
}

/// One Thing — the app's timer dial on white: green ring, gold tip at 12 o'clock ON the ring,
/// one green centre dot.
fn onething(outer: &GrayImage, inner: &GrayImage, out: &Path) {
    let mut img = RgbImage::from_pixel(N, N, WHITE);
    let k = SS as f32;
    let cx = 256.0 * k;
    let cy = 262.0 * k; // +6: the gold tip adds mass above — optical centre sits lower
    let r = 128.0;
    disc(&mut img, cx, cy, (r + 14.0) * k, GREEN); // ring outer
    disc(&mut img, cx, cy, (r - 14.0) * k, WHITE); // ring hollow
    disc(&mut img, cx, (262.0 - r) * k, 34.0 * k, WHITE); // tip surround (sits ON the ring)
    disc(&mut img, cx, (262.0 - r) * k, 26.0 * k, GOLD); // the gold tip
    disc(&mut img, cx, cy, 40.0 * k, GREEN); // the one thing
    finish(img, outer, inner, out);
}

/// A filled rounded rectangle in 512-space coordinates (scaled to the 4× working canvas).
fn rrect(img: &mut RgbImage, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgb<u8>) {
    let k = SS as f32;
    let (x, y, w, h) = (x * k, y * k, w * k, h * k);
    let r = (r * k).min(w / 2.0).min(h / 2.0);
    fill_rect(img, x, y + r, w, h - 2.0 * r, c);
    fill_rect(img, x + r, y, w - 2.0 * r, r, c);
    fill_rect(img, x + r, y + h - r, w - 2.0 * r, r, c);
    for (cx, cy) in [(x + r, y + r), (x + w - r, y + r), (x + r, y + h - r), (x + w - r, y + h - r)] {
        disc(img, cx, cy, r, c);
    }
}

/// A supersampled superellipse (iOS "squircle", exponent 5) as an alpha mask. `inset` (in
/// 512-space px) shrinks it — the band between the outer mask and an inset twin is the keyline.
fn squircle_mask(inset: f32) -> GrayImage {
    let n = 5.0_f32;
    let c = N as f32 / 2.0;
    let a = c - (4.0 + inset) * SS as f32;
    let mut m = GrayImage::from_pixel(N, N, Luma([0]));
    for y in 0..N {
        for x in 0..N {
            let dx = ((x as f32 + 0.5) - c) / a;
            let dy = ((y as f32 + 0.5) - c) / a;
            if dx.abs().powf(n) + dy.abs().powf(n) <= 1.0 {
                m.put_pixel(x, y, Luma([255]));
            }
        }
    }
    m
}

/// Paint the keyline (the outer∖inner band) into the colour canvas, then downscale canvas + outer
/// mask with Lanczos3, weld the mask in as alpha, and write the RGBA PNG.
fn finish(mut color: RgbImage, outer: &GrayImage, inner: &GrayImage, out: &Path) {
    for y in 0..N {
        for x in 0..N {
            if outer.get_pixel(x, y).0[0] > 0 && inner.get_pixel(x, y).0[0] == 0 {
                color.put_pixel(x, y, KEYLINE);
            }
        }
    }
    let c = imageops::resize(&color, OUT, OUT, imageops::FilterType::Lanczos3);
    let a = imageops::resize(outer, OUT, OUT, imageops::FilterType::Lanczos3);
    let mut img = RgbaImage::new(OUT, OUT);
    for y in 0..OUT {
        for x in 0..OUT {
            let p = c.get_pixel(x, y).0;
            let alpha = a.get_pixel(x, y).0[0];
            img.put_pixel(x, y, Rgba([p[0], p[1], p[2], alpha]));
        }
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).expect("create app dir");
    }
    img.save(out).unwrap_or_else(|e| panic!("save {}: {e}", out.display()));
    println!("wrote {} ({}×{})", out.display(), OUT, OUT);
}
