//! Pure image preparation for the reader: decode fetched bytes, contain-fit them into the content
//! box, lift saturation to offset the Gallery 3 panel's colour muting, and Floyd–Steinberg dither
//! **each RGB channel** to a small ramp so photo gradients don't band on the panel's limited colour
//! depth. No network — the app crate fetches the bytes; this stays host-testable.

use image::{GenericImageView, Rgb, RgbImage};

/// Levels **per channel** the Gallery 3 panel renders cleanly (4-bit → 16 per channel = 4096 colours).
/// Dithering to this ramp (rather than hard 8-bit) avoids visible banding across photo gradients. This
/// is the colour analog of the old 16-gray path: a grayscale image (R=G=B) lands on the same 16 levels
/// it always did.
const LEVELS_PER_CHANNEL: u32 = 16;

/// Saturation multiplier applied before dithering, to offset how much the Gallery 3 panel mutes colour
/// on-device (mirrors `render::wash` / `TINT_WASH`, tuned the other direction). `1.0` = faithful; `>1`
/// = punchier. 1.8 matches the browser's device-proven `ink_px` boost (1.25 was timid — photos still
/// washed out on the panel). Judge on the physical panel — a headless PNG can't show the muting.
const SATURATION: f32 = 1.8;

/// Contain-fit `(nw, nh)` inside `(max_w, max_h)` **without upscaling** (small images stay their
/// native size). Preserves aspect ratio; never returns a zero dimension.
pub fn fit(nw: u32, nh: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if nw == 0 || nh == 0 {
        return (1, 1);
    }
    let s = (max_w as f32 / nw as f32).min(max_h as f32 / nh as f32).min(1.0);
    let w = ((nw as f32 * s).round() as u32).max(1);
    let h = ((nh as f32 * s).round() as u32).max(1);
    (w, h)
}

/// Decode `bytes` (PNG/JPEG/WebP/GIF — format sniffed from magic bytes, so extensionless CDN URLs
/// work), contain-fit into `(max_w, max_h)`, lift saturation, and Floyd–Steinberg dither each channel
/// to [`LEVELS_PER_CHANNEL`]. Returns `None` for bytes that aren't a supported image (HTML error
/// pages, svg/avif, truncated downloads) so the caller can fall back to a placeholder.
pub fn prepare(bytes: &[u8], max_w: u32, max_h: u32) -> Option<RgbImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let (nw, nh) = img.dimensions();
    let (w, h) = fit(nw, nh, max_w, max_h);
    let mut scaled = image::imageops::resize(&img.to_rgb8(), w, h, image::imageops::FilterType::Triangle);
    saturate(&mut scaled, SATURATION);
    Some(dither_rgb(&scaled))
}

/// Push each pixel's channels away from its luma by `factor`, in place. `1.0` is a no-op; grayscale
/// pixels (R=G=B, so every channel equals the luma) are unchanged.
fn saturate(img: &mut RgbImage, factor: f32) {
    if (factor - 1.0).abs() < f32::EPSILON {
        return;
    }
    for p in img.pixels_mut() {
        let g = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        for c in 0..3 {
            p[c] = (g + (p[c] as f32 - g) * factor).clamp(0.0, 255.0) as u8;
        }
    }
}

/// Floyd–Steinberg error diffusion on an RGB image, quantizing **each channel** to
/// [`LEVELS_PER_CHANNEL`]. Error is accumulated per channel in f32 so it doesn't clip prematurely.
fn dither_rgb(rgb: &RgbImage) -> RgbImage {
    let (w, h) = rgb.dimensions();
    let mut buf: Vec<[f32; 3]> = rgb.pixels().map(|p| [p[0] as f32, p[1] as f32, p[2] as f32]).collect();
    let step = 255.0 / (LEVELS_PER_CHANNEL - 1) as f32;
    let idx = |x: u32, y: u32| (y * w + x) as usize;
    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let old = buf[idx(x, y)];
            let mut q = [0u8; 3];
            let mut err = [0.0f32; 3];
            for c in 0..3 {
                let qc = ((old[c] / step).round() * step).clamp(0.0, 255.0);
                err[c] = old[c] - qc;
                q[c] = qc as u8;
            }
            out.put_pixel(x, y, Rgb(q));
            // Distribute each channel's quantization error to the not-yet-visited neighbours (7/3/5/1 · 1/16).
            if x + 1 < w {
                diffuse(&mut buf[idx(x + 1, y)], &err, 7.0 / 16.0);
            }
            if y + 1 < h {
                if x > 0 {
                    diffuse(&mut buf[idx(x - 1, y + 1)], &err, 3.0 / 16.0);
                }
                diffuse(&mut buf[idx(x, y + 1)], &err, 5.0 / 16.0);
                if x + 1 < w {
                    diffuse(&mut buf[idx(x + 1, y + 1)], &err, 1.0 / 16.0);
                }
            }
        }
    }
    out
}

/// Add `err · frac` into a neighbour's per-channel accumulator.
fn diffuse(cell: &mut [f32; 3], err: &[f32; 3], frac: f32) {
    for c in 0..3 {
        cell[c] += err[c] * frac;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn png_bytes(w: u32, h: u32, px: Rgb<u8>) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for p in img.pixels_mut() {
            *p = px;
        }
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn fit_contains_without_upscaling() {
        assert_eq!(fit(40, 20, 800, 1400), (40, 20), "small image keeps native size");
        assert_eq!(fit(1600, 800, 800, 1400), (800, 400), "wide image is width-bound");
        assert_eq!(fit(800, 3000, 800, 1400), (373, 1400), "tall image is height-bound");
        assert_eq!(fit(0, 10, 800, 1400), (1, 1), "degenerate size is safe");
    }

    #[test]
    fn prepare_scales_and_keeps_colour() {
        let out = prepare(&png_bytes(100, 50, Rgb([200, 30, 30])), 50, 1400).unwrap();
        assert_eq!(out.dimensions(), (50, 25), "contain-fit to max width, aspect preserved");
        // A red input stays red (not flattened to gray): the red channel dominates on average.
        let (mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64);
        for p in out.pixels() {
            sr += p[0] as u64;
            sg += p[1] as u64;
            sb += p[2] as u64;
        }
        assert!(sr > sg && sr > sb, "red stays dominant (r={sr} g={sg} b={sb})");
        // Every channel lands on the per-channel ramp (step 17 is exact for 16 levels).
        let step = (255.0 / (LEVELS_PER_CHANNEL - 1) as f32).round() as i32;
        assert!(
            out.pixels().all(|p| (0..3).all(|c| p[c] as i32 % step.max(1) == 0)),
            "every channel is on the ramp",
        );
    }

    #[test]
    fn prepare_rejects_non_images() {
        assert!(prepare(b"<html>not an image</html>", 800, 1400).is_none());
        assert!(prepare(&[], 800, 1400).is_none());
    }

    #[test]
    fn dither_quantizes_each_channel_to_the_ramp() {
        // Opposing horizontal gradients per channel must each land only on the ramp.
        let mut g = RgbImage::new(64, 8);
        for (x, _y, p) in g.enumerate_pixels_mut() {
            *p = Rgb([(x * 4) as u8, (252 - x * 4) as u8, 120]);
        }
        let out = dither_rgb(&g);
        let step = (255.0 / (LEVELS_PER_CHANNEL - 1) as f32).round() as i32;
        for p in out.pixels() {
            for c in 0..3 {
                assert_eq!(p[c] as i32 % step.max(1), 0, "channel {c} on the ramp");
            }
        }
    }
}
