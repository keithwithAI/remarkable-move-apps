//! Shared interactive-UI toolkit — the "Reading Desk" design language used by the on-device apps
//! (the Reader, and now the Agenda). Bootstrap-icon chrome, slide-in scrim panels, even-spaced
//! action bars, and chip tap targets, drawn on paper with the embedded [`Fonts`] + `draw` primitives.
//!
//! **Target-agnostic by design:** the draw helpers draw *and return the [`Rect`]s they create*, so
//! each app maps those rects to its own tap-target enum (the `render` crate never knows about a
//! specific app's targets). This is the shared home the Reader's private design helpers migrate to.

use crate::draw::{disc, draw_text, draw_text_tracked, fill_rect, hline, icon, measure, rect_outline, truncate, vline};
use crate::{Fonts, FAINT, INK, MUTED, PAPER};
use ab_glyph::FontRef;
use image::{Rgb, RgbImage};

/// A tap-target rectangle in framebuffer pixels. Shared across apps so lifted draw helpers can hand
/// back hit rects that drop straight into each app's HitMap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// Build from `f32` layout coords (truncated to whole pixels) — the ergonomic constructor for
    /// callers that compute geometry in `f32`.
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x: x as i32, y: y as i32, w: w as i32, h: h as i32 }
    }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
}

/// Warm gold accent for "on" states (favourite, highlights) — reads on the muted Gallery-3 panel.
pub const GOLD: Rgb<u8> = Rgb([230, 170, 40]);
/// Pale button fill so chip tap targets read as tappable on the muted panel.
pub const CHIP: Rgb<u8> = Rgb([236, 236, 236]);
/// Amber "active/selected" tint (shares the reading/highlight hue family). Deepened from the
/// original pale `245,224,140` — the panel's muting made it near-invisible (the reader's
/// "Reading" status dot vanished).
pub const TINT: Rgb<u8> = Rgb([243, 205, 80]);
/// Flat grey wash behind a modal panel — e-ink has no alpha, so we just grey the page behind it.
pub const SCRIM: Rgb<u8> = Rgb([206, 206, 206]);

/// Default on-screen height for a chrome icon (px). Tuned for legibility on the muted e-ink panel.
pub const ICON_PX: f32 = 40.0;
/// Width of a left-docked slide-in panel column.
pub const PANEL_W: i32 = 624;

/// The shared UI icon set — Bootstrap Icons (MIT) codepoints, drawn from `fonts.icons`. A few carry
/// filled twins for "on" states. Matches the Reader's set so the Reader can migrate onto this later.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Menu,
    Search,
    Star,
    StarFill,
    Archive,
    ArchiveFill,
    Tag,
    More,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    /// arrow-counterclockwise — "undo" in the Reader; doubles as "refresh/reload" in the Agenda.
    Undo,
    Sort,
    Check,
    Close,
    /// box-arrow-right — "exit the app" in a bottom bar.
    Exit,
    /// plus — an "add / new" action (the Agenda's quick-add).
    Add,
    /// record-circle — a ring+centre "jump to today / now" control (echoes the NOW-dot).
    Today,
    /// card-text — a note (the reader's tap-a-highlight "Note" action + the noted-highlight indicator).
    Note,
    /// trash3 — delete (the reader's tap-a-highlight "Delete" action).
    Trash,
    /// book / book-fill — the Reader's Kindle-books shelf collection (fill = the active state).
    Book,
    BookFill,
    /// play-fill / pause-fill — the One Thing focus timer's start/pause control.
    PlayFill,
    PauseFill,
    /// arrow-90deg-left / arrow-90deg-right — the browser's rotate-to-landscape controls (bent
    /// arrows, deliberately distinct from the circular Undo/reload glyph).
    RotateLeft,
    RotateRight,
}

impl Icon {
    pub fn ch(self) -> char {
        match self {
            Icon::Menu => '\u{F479}',
            Icon::Search => '\u{F52A}',
            Icon::Star => '\u{F588}',
            Icon::StarFill => '\u{F586}',
            Icon::Archive => '\u{F10D}',
            Icon::ArchiveFill => '\u{F10C}',
            Icon::Tag => '\u{F5B0}',
            Icon::More => '\u{F5D4}',
            Icon::ChevronLeft => '\u{F284}',
            Icon::ChevronRight => '\u{F285}',
            Icon::ChevronDown => '\u{F282}',
            Icon::Undo => '\u{F117}',
            Icon::Sort => '\u{F127}',
            Icon::Check => '\u{F633}',
            Icon::Close => '\u{F659}',
            Icon::Exit => '\u{F1C3}',
            Icon::Add => '\u{F4FE}',
            Icon::Today => '\u{F518}',
            Icon::Note => '\u{F228}',
            Icon::Trash => '\u{F78B}',
            Icon::Book => '\u{F194}',
            Icon::BookFill => '\u{F192}',
            Icon::PlayFill => '\u{F4F4}',
            Icon::PauseFill => '\u{F4C3}',
            Icon::RotateLeft => '\u{F10F}',
            Icon::RotateRight => '\u{F110}',
        }
    }
}

/// Draw icon `ic` centred in the `w×h` tap cell at `(x, y)` in `ink`, and return the cell's rect for
/// the hit map. Chrome sits directly on paper (no button fill) — pass a filled glyph + accent for
/// "on" states (e.g. `StarFill` in [`GOLD`]).
pub fn icon_btn(img: &mut RgbImage, fonts: &Fonts, ic: Icon, x: i32, y: i32, w: i32, h: i32, ink: Rgb<u8>) -> Rect {
    icon(img, &fonts.icons, ic.ch(), (x + w / 2) as f32, (y + h / 2) as f32, ICON_PX, ink);
    Rect { x, y, w, h }
}

/// The left x of each of `n` cells evenly spread across `[margin, iw - margin]`, each `icell` wide, so
/// slot 0 sits on the left margin and slot `n-1` on the right, with equal gaps between. `k*span` is
/// multiplied before the divide so the last slot lands exactly on `iw - margin - icell`.
pub fn even_slots(iw: i32, margin: i32, icell: i32, n: i32) -> Vec<i32> {
    if n <= 1 {
        return vec![margin];
    }
    (0..n).map(|k| margin + k * (iw - 2 * margin - icell) / (n - 1)).collect()
}

/// A filled tap chip with a centred label. `active` inverts it to INK-on-PAPER (a selected pill);
/// otherwise it's a pale [`CHIP`] fill. Returns its rect for the hit map.
pub fn chip(img: &mut RgbImage, fonts: &Fonts, label: &str, x: i32, y: i32, w: i32, h: i32, active: bool) -> Rect {
    let (bg, fg) = if active { (INK, PAPER) } else { (CHIP, INK) };
    fill_rect(img, x as f32, y as f32, w as f32, h as f32, bg);
    let px = 30.0;
    let t = truncate(&fonts.sans_semibold, px, label, (w - 24) as f32);
    let tw = measure(&fonts.sans_semibold, px, &t);
    draw_text(img, &fonts.sans_semibold, px, x as f32 + (w as f32 - tw) / 2.0, y as f32 + h as f32 / 2.0 + px * 0.35, &t, fg);
    Rect { x, y, w, h }
}

/// A tracked-caps section eyebrow with its baseline at `(x, baseline)` — the panel/section voice.
pub fn panel_eyebrow(img: &mut RgbImage, fonts: &Fonts, label: &str, x: i32, baseline: i32) {
    draw_text_tracked(img, &fonts.sans_semibold, 24.0, x as f32, baseline as f32, &label.to_uppercase(), MUTED, 3.0);
}

/// Greedy word-wrap `text` to at most `max_lines` lines within `width` px; the final line is
/// truncated with an ellipsis if words remain. Returns `1..=max_lines` lines (a single, possibly
/// empty, line for empty input). The editorial title voice shared by both apps' rows/panels.
pub fn wrap(font: &FontRef, px: f32, text: &str, width: f32, max_lines: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < words.len() {
        let trial = if cur.is_empty() { words[i].to_string() } else { format!("{cur} {}", words[i]) };
        if measure(font, px, &trial) <= width || cur.is_empty() {
            cur = trial;
            i += 1;
        } else if lines.len() + 1 < max_lines {
            lines.push(std::mem::take(&mut cur));
        } else {
            break; // on the last allowed line — remaining words fold in + truncate below
        }
    }
    let mut last = cur;
    if i < words.len() {
        if !last.is_empty() {
            last.push(' ');
        }
        last.push_str(&words[i..].join(" "));
    }
    lines.push(truncate(font, px, &last, width));
    lines
}

/// A panel's inner left/right padding.
pub const PANEL_PAD: i32 = 44;

/// A left-docked modal panel: a [`SCRIM`] over the whole screen (tap-out to dismiss), an opaque white
/// [`PANEL_W`] column on the left, a serif masthead, a Close (X) in the column's top-right, and a
/// hairline right edge. Returns `(scrim_tap_out, close_btn)` rects — the caller pushes them to its own
/// dismiss/close targets and draws the panel body below the masthead inside the column.
pub fn draw_panel(img: &mut RgbImage, fonts: &Fonts, title: &str, screen_w: i32, screen_h: i32) -> (Rect, Rect) {
    fill_rect(img, 0.0, 0.0, screen_w as f32, screen_h as f32, SCRIM);
    let scrim = Rect { x: 0, y: 0, w: screen_w, h: screen_h };
    fill_rect(img, 0.0, 0.0, PANEL_W as f32, screen_h as f32, PAPER);
    vline(img, PANEL_W as f32, 0.0, screen_h as f32, FAINT, 2);
    draw_text(img, &fonts.serif_bold, 44.0, PANEL_PAD as f32, 74.0, title, INK);
    let cell = 60;
    let close = icon_btn(img, fonts, Icon::Close, PANEL_W - PANEL_PAD - cell, 18, cell, cell, MUTED);
    hline(img, PANEL_PAD as f32, (PANEL_W - PANEL_PAD) as f32, 108.0, FAINT, 1);
    (scrim, close)
}

/// The height of a bottom sheet's header block (grabber + title + rule); the caller lays out its
/// action rows from `sheet_body.y + SHEET_HEADER_H` down.
pub const SHEET_HEADER_H: i32 = 140;

/// A **bottom sheet**: a full-width opaque-white panel pinned to the bottom `sheet_h` px of the
/// screen — deliberately *without* a scrim, so the content above it stays visible (a sheet keeps its
/// context on screen, unlike [`draw_panel`]). Draws a crisp top edge, a centred rounded grabber, a
/// single-line `title`, a Close (X) top-right, and a hairline under the header. Returns
/// `(sheet_body, close)`: the caller pushes `sheet_body` as its tap-swallow rect and `close` as a
/// dismiss target, and draws its rows from `sheet_body.y + SHEET_HEADER_H`. Pair it with a
/// full-screen dismiss rect pushed *under* the sheet so a tap on the visible area above it closes it.
pub fn draw_bottom_sheet(img: &mut RgbImage, fonts: &Fonts, title: &str, sheet_h: i32, screen_w: i32, screen_h: i32) -> (Rect, Rect) {
    let top = screen_h - sheet_h;
    fill_rect(img, 0.0, top as f32, screen_w as f32, sheet_h as f32, PAPER);
    hline(img, 0.0, screen_w as f32, top as f32, MUTED, 3); // crisp top edge lifts it off the page

    // Grabber: a short rounded bar, centred near the top edge.
    let gw = 96.0;
    let gx = (screen_w as f32 - gw) / 2.0;
    let gy = (top + 20) as f32;
    fill_rect(img, gx, gy, gw, 8.0, FAINT);
    disc(img, gx, gy + 4.0, 4.0, FAINT);
    disc(img, gx + gw, gy + 4.0, 4.0, FAINT);

    // Title (single line) + Close (X) top-right.
    let cell = 60;
    let close = icon_btn(img, fonts, Icon::Close, screen_w - PANEL_PAD - cell, top + 44, cell, cell, MUTED);
    let title_max = (screen_w - 2 * PANEL_PAD - cell - 20) as f32;
    let t = truncate(&fonts.sans_semibold, 36.0, title, title_max);
    draw_text(img, &fonts.sans_semibold, 36.0, PANEL_PAD as f32, (top + 96) as f32, &t, INK);
    hline(img, PANEL_PAD as f32, (screen_w - PANEL_PAD) as f32, (top + SHEET_HEADER_H - 12) as f32, FAINT, 1);

    (Rect { x: 0, y: top, w: screen_w, h: sheet_h }, close)
}

/// One key on the [on-screen keyboard](draw_keyboard). The caller maps each to its own tap target;
/// `Char` already carries the correct case/symbol for the active layer + shift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyCap {
    Char(char),
    Backspace,
    Space,
    /// Toggle caps (letters layer only).
    Shift,
    /// Switch between the letters layer and the numbers/symbols layer.
    Layer,
    /// The commit key — the caller decides what it does (Add / Search / Done); labelled by
    /// `enter_label`.
    Enter,
}

/// Which set of keys the [keyboard](draw_keyboard) shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KbLayer {
    Letters,
    Symbols,
}

/// Total on-screen-keyboard height (4 rows + padding). A caller docks it at `screen_h - KB_H`.
pub const KB_H: i32 = 440;
/// A keyboard key can be a touch narrower than [`super`] action buttons — it's a dense grid you can
/// correct typos in. Callers that size-check keyboard targets use this floor for them.
pub const MIN_KEY_PX: i32 = 80;
const KB_PAD: i32 = 16;
const KB_GAP: f32 = 8.0;
const KB_KEY_H: f32 = 94.0;
const KB_ROW_GAP: f32 = 12.0;
const KB_TOP_PAD: f32 = 14.0;

/// The rows of keys for a layer + shift state. Letter chars are pre-cased so `KeyCap::Char` is
/// exactly what to insert. Special keys (Shift/Backspace/Layer/Space/Enter) frame the letter grid.
fn kb_layout(layer: KbLayer, shift: bool) -> Vec<Vec<KeyCap>> {
    let mut rows: Vec<Vec<KeyCap>> = Vec::new();
    match layer {
        KbLayer::Letters => {
            for (i, r) in ["qwertyuiop", "asdfghjkl", "zxcvbnm"].iter().enumerate() {
                let mut row: Vec<KeyCap> = r
                    .chars()
                    .map(|c| KeyCap::Char(if shift { c.to_ascii_uppercase() } else { c }))
                    .collect();
                if i == 2 {
                    row.insert(0, KeyCap::Shift);
                    row.push(KeyCap::Backspace);
                }
                rows.push(row);
            }
        }
        KbLayer::Symbols => {
            rows.push("1234567890".chars().map(KeyCap::Char).collect());
            rows.push("-/:;()$&@\"".chars().map(KeyCap::Char).collect());
            let mut row: Vec<KeyCap> = ".,?!'%".chars().map(KeyCap::Char).collect();
            row.push(KeyCap::Backspace);
            rows.push(row);
        }
    }
    rows.push(vec![KeyCap::Layer, KeyCap::Space, KeyCap::Enter]);
    rows
}

/// Layout weight (relative width) of a key. Space stretches; the framing keys are ~1.6× a letter.
fn key_weight(k: KeyCap) -> f32 {
    match k {
        KeyCap::Space => 6.0,
        KeyCap::Shift | KeyCap::Backspace | KeyCap::Layer | KeyCap::Enter => 1.6,
        KeyCap::Char(_) => 1.0,
    }
}

fn key_label(k: KeyCap, layer: KbLayer, enter_label: &str) -> String {
    match k {
        KeyCap::Char(c) => c.to_string(),
        KeyCap::Space => "space".into(),
        KeyCap::Backspace => "del".into(),
        KeyCap::Shift => "shift".into(),
        KeyCap::Layer => if layer == KbLayer::Letters { "123".into() } else { "abc".into() },
        KeyCap::Enter => enter_label.to_string(),
    }
}

fn draw_key(img: &mut RgbImage, fonts: &Fonts, k: KeyCap, x: f32, y: f32, w: f32, h: f32, shift: bool, layer: KbLayer, enter_label: &str) -> Rect {
    let active = matches!(k, KeyCap::Shift) && shift;
    let commit = matches!(k, KeyCap::Enter);
    let (bg, fg) = if active || commit { (INK, PAPER) } else { (CHIP, INK) };
    fill_rect(img, x, y, w, h, bg);
    rect_outline(img, x, y, w, h, FAINT, 1);
    let label = key_label(k, layer, enter_label);
    let px = if matches!(k, KeyCap::Char(_)) { 44.0 } else { 30.0 };
    let tw = measure(&fonts.sans_semibold, px, &label);
    draw_text(img, &fonts.sans_semibold, px, x + (w - tw) / 2.0, y + h / 2.0 + px * 0.34, &label, fg);
    Rect::new(x, y, w, h)
}

/// A docked **on-screen keyboard**: a justified QWERTY (or numbers/symbols) grid across the full
/// width from `top` down, `KB_H` tall. `layer`/`shift` pick the keys; `enter_label` names the commit
/// key ("Add", "Search", …). Draws each key and returns `(rect, cap)` for every key — the caller maps
/// each [`KeyCap`] to its own tap target. Keyboard keys clear [`MIN_KEY_PX`] (a touch smaller than a
/// full action button is fine for a correctable grid).
pub fn draw_keyboard(img: &mut RgbImage, fonts: &Fonts, top: i32, screen_w: i32, layer: KbLayer, shift: bool, enter_label: &str) -> Vec<(Rect, KeyCap)> {
    let rows = kb_layout(layer, shift);
    let usable = (screen_w - 2 * KB_PAD) as f32;
    let mut hits = Vec::new();
    for (r, row) in rows.iter().enumerate() {
        let total_gap = (row.len() as f32 - 1.0) * KB_GAP;
        let total_w: f32 = row.iter().map(|k| key_weight(*k)).sum();
        let avail = usable - total_gap;
        let y = top as f32 + KB_TOP_PAD + r as f32 * (KB_KEY_H + KB_ROW_GAP);
        let mut x = KB_PAD as f32;
        for &k in row {
            let w = avail * key_weight(k) / total_w;
            let rect = draw_key(img, fonts, k, x, y, w, KB_KEY_H, shift, layer, enter_label);
            hits.push((rect, k));
            x += w + KB_GAP;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_slots_lands_on_both_edges() {
        let iw = 954;
        let (margin, icell) = (44, 60);
        let s = even_slots(iw, margin, icell, 5);
        assert_eq!(s.len(), 5);
        assert_eq!(s[0], margin, "slot 0 on the left margin");
        assert_eq!(*s.last().unwrap(), iw - margin - icell, "last slot on the right margin");
        // gaps are (near-)equal
        let gaps: Vec<i32> = s.windows(2).map(|w| w[1] - w[0]).collect();
        let (min, max) = (*gaps.iter().min().unwrap(), *gaps.iter().max().unwrap());
        assert!(max - min <= 1, "slots are evenly spread: {gaps:?}");
    }

    #[test]
    fn icon_btn_returns_its_cell_and_inks() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(200, 120, PAPER);
        let r = icon_btn(&mut img, &fonts, Icon::Check, 20, 30, 60, 60, INK);
        assert_eq!(r, Rect { x: 20, y: 30, w: 60, h: 60 });
        let inked = img.pixels().filter(|p| p[0] < 128).count();
        assert!(inked > 20, "the glyph drew some ink, got {inked}");
    }

    #[test]
    fn every_icon_glyph_is_in_the_font_subset() {
        // A codepoint missing from the subset ttf renders as blank/notdef — catch it here rather
        // than as an empty button on-device (the subset is regenerated by hand when icons grow).
        let fonts = Fonts::embedded();
        for ic in [
            Icon::Menu, Icon::Search, Icon::Star, Icon::StarFill, Icon::Archive, Icon::ArchiveFill,
            Icon::Tag, Icon::More, Icon::ChevronLeft, Icon::ChevronRight, Icon::ChevronDown,
            Icon::Undo, Icon::Sort, Icon::Check, Icon::Close, Icon::Exit, Icon::Add, Icon::Today,
            Icon::Note, Icon::Trash, Icon::Book, Icon::BookFill, Icon::PlayFill, Icon::PauseFill,
            Icon::RotateLeft, Icon::RotateRight,
        ] {
            let mut img = RgbImage::from_pixel(100, 100, PAPER);
            icon_btn(&mut img, &fonts, ic, 20, 20, 60, 60, INK);
            let inked = img.pixels().filter(|p| p[0] < 128).count();
            assert!(inked > 15, "glyph U+{:04X} missing/blank ({inked} px)", ic.ch() as u32);
        }
    }

    #[test]
    fn chip_contains_its_center_and_active_inverts() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(400, 120, PAPER);
        let r = chip(&mut img, &fonts, "Today", 10, 10, 180, 72, true);
        assert!(r.contains(r.center().0, r.center().1));
        // active = INK fill; sample just inside the left edge (clear of the centred white label).
        let p = img.get_pixel((r.x + 6) as u32, r.center().1 as u32);
        assert!(p[0] < 128, "active chip fills INK, got {p:?}");
    }

    #[test]
    fn panel_returns_fullscreen_scrim_and_a_close_button() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(954, 1696, PAPER);
        let (scrim, close) = draw_panel(&mut img, &fonts, "Task", 954, 1696);
        assert_eq!(scrim, Rect { x: 0, y: 0, w: 954, h: 1696 });
        assert!(close.w >= 60 && close.h >= 60, "close is finger-sized");
        assert!(close.x < PANEL_W && close.x > PANEL_W / 2, "close sits top-right of the column");
    }

    #[test]
    fn bottom_sheet_pins_full_width_to_the_bottom() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(954, 1696, PAPER);
        let (body, close) = draw_bottom_sheet(&mut img, &fonts, "Finish the planner event loop", 732, 954, 1696);
        assert_eq!(body, Rect { x: 0, y: 1696 - 732, w: 954, h: 732 }, "full width, pinned to the bottom");
        assert!(close.w >= 60 && close.h >= 60, "close is finger-sized");
        assert!(close.y >= body.y && close.y < body.y + SHEET_HEADER_H, "close sits in the sheet header");
        assert!(close.x > 954 / 2, "close is on the right");
        // the content above the sheet is untouched (no scrim): the top row of pixels is still paper
        assert!(img.get_pixel(477, 10)[0] > 250, "no scrim over the area above the sheet");
    }

    #[test]
    fn keyboard_letters_layer_is_complete_finger_sized_and_disjoint() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(954, 1696, PAPER);
        let hits = draw_keyboard(&mut img, &fonts, 1696 - KB_H, 954, KbLayer::Letters, false, "Add");
        let chars: std::collections::HashSet<char> =
            hits.iter().filter_map(|(_, k)| if let KeyCap::Char(c) = k { Some(*c) } else { None }).collect();
        for c in 'a'..='z' {
            assert!(chars.contains(&c), "letters layer missing '{c}'");
        }
        for want in [KeyCap::Shift, KeyCap::Backspace, KeyCap::Space, KeyCap::Layer, KeyCap::Enter] {
            assert!(hits.iter().any(|(_, k)| *k == want), "missing framing key {want:?}");
        }
        for (r, _) in &hits {
            assert!(r.w >= MIN_KEY_PX && r.h >= MIN_KEY_PX, "key too small: {r:?}");
            assert!(r.x >= 0 && r.x + r.w <= 954 && r.y + r.h <= 1696, "key off-screen: {r:?}");
        }
        for (i, (a, _)) in hits.iter().enumerate() {
            for (b, _) in hits.iter().skip(i + 1) {
                assert!(!a.intersects(b), "keys overlap: {a:?} / {b:?}");
            }
        }
    }

    #[test]
    fn keyboard_shift_capitalises_and_symbols_layer_has_digits() {
        let fonts = Fonts::embedded();
        let mut img = RgbImage::from_pixel(954, 1696, PAPER);
        let shifted = draw_keyboard(&mut img, &fonts, 1696 - KB_H, 954, KbLayer::Letters, true, "Add");
        assert!(shifted.iter().any(|(_, k)| *k == KeyCap::Char('Q')), "shift capitalises");
        assert!(!shifted.iter().any(|(_, k)| *k == KeyCap::Char('q')), "no lowercase when shifted");
        let sym = draw_keyboard(&mut img, &fonts, 1696 - KB_H, 954, KbLayer::Symbols, false, "Add");
        let chars: std::collections::HashSet<char> =
            sym.iter().filter_map(|(_, k)| if let KeyCap::Char(c) = k { Some(*c) } else { None }).collect();
        for c in '0'..='9' {
            assert!(chars.contains(&c), "symbols layer missing digit '{c}'");
        }
        assert!(chars.contains(&'@') && chars.contains(&'.'), "symbols layer has punctuation");
    }
}
