//! Draw the three screens to a frame + a hit map. Reading Desk language: serif masthead, 56px
//! margins, hairline rules, even-slot icon bar, the shared on-screen keyboard.
//!
//! The countdown digits render in **fixed-width cells** (widest-digit advance) so the block never
//! shifts as digits change — the per-second dirty region ([`digits_rect`]) stays pixel-stable.

use crate::model::{Model, Screen, Target, Timer, PRESETS};
use chrono::{Datelike, NaiveDate};
use image::RgbImage;
use render::draw::{disc, draw_text, draw_text_tracked, fill_rect, hline, icon, measure, rect_outline, truncate};
use render::ui::{chip, draw_keyboard, even_slots, icon_btn, wrap, Icon, KeyCap, Rect, CHIP, GOLD, KB_H};
use render::{Canvas, Fonts, FAINT, INK, MUTED, PAPER};

/// The app's brand accent — the icon's forest green, lifted HARD for the muted Gallery 3 panel:
/// the icon's #1F4D3A reads nearly black on e-ink, and the first lift (40,96,70) still read
/// charcoal once the panel muted it. Vivid in software, softened by the panel — same lesson as
/// the browser's ink_px. Flat fields only; gradients band.
const GREEN: image::Rgb<u8> = image::Rgb([32, 138, 92]);

pub type HitMap = Vec<(Rect, Target)>;

/// Topmost hit wins — the map is searched in reverse draw order.
pub fn hit_test(hits: &HitMap, x: i32, y: i32) -> Option<&(Rect, Target)> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y))
}

pub struct ViewOutput {
    pub frame: RgbImage,
    pub hits: HitMap,
}

const MARGIN: i32 = 56;
const BAR_H: i32 = 108;
/// The progress dial — a watch-face ring, the screen's signature. Elapsed arc in INK growing
/// clockwise from 12, remaining track in FAINT, a PAPER/GOLD dot riding the arc tip.
const RING_CX: i32 = 477;
const RING_CY: i32 = 900;
const RING_R: f32 = 350.0; // outer radius
const RING_STROKE: f32 = 28.0;
/// Countdown digit sizes inside the dial (fixed-width cells; both modes fit the ~640px inner circle).
const DIGIT_PX: f32 = 160.0; // MM:SS in live-seconds / final-minute mode
const MINUTES_PX: f32 = 190.0; // the whole-minutes number in calm mode
/// The stable per-second dirty band inside the ring (see [`digits_rect`]).
const DIGITS_TOP: i32 = 790;
const DIGITS_H: i32 = 190;
/// Picker geometry.
const PICKER_HEADER_H: i32 = 168;
const QBAR_H: i32 = 84;
const ROW_H: i32 = 108;

/// The band holding the countdown digits inside the dial — the per-second partial-redraw region
/// (a fixed rect regardless of digits keeps the dirty region stable) and the ToggleSeconds target.
pub fn digits_rect(_canvas: Canvas) -> (i32, i32, i32, i32) {
    (RING_CX - 300, DIGITS_TOP, 600, DIGITS_H)
}

/// Picker rows that fit between the query bar and the docked keyboard.
pub fn rows_per_page(canvas: Canvas) -> usize {
    ((canvas.height as i32 - PICKER_HEADER_H - QBAR_H - 16 - KB_H) / ROW_H).max(1) as usize
}

pub fn view(model: &Model, fonts: &Fonts, canvas: Canvas) -> ViewOutput {
    let mut img = RgbImage::from_pixel(canvas.width, canvas.height, PAPER);
    let mut hits: HitMap = Vec::new();
    let (w, h) = (canvas.width as i32, canvas.height as i32);

    match model.screen {
        Screen::Home => draw_home(&mut img, &mut hits, model, fonts, w, h),
        Screen::Picker => draw_picker(&mut img, &mut hits, model, fonts, w, h),
        Screen::Done => draw_done(&mut img, &mut hits, model, fonts, w, h),
    }
    draw_flash(&mut img, model, fonts, w, h);
    ViewOutput { frame: img, hits }
}

fn draw_home(img: &mut RgbImage, hits: &mut HitMap, model: &Model, fonts: &Fonts, w: i32, h: i32) {
    // Masthead: serif title + the date line (matches Habits/Recipes).
    draw_text(img, &fonts.serif_bold, 64.0, MARGIN as f32, 108.0, "One Thing", INK);
    let date = model.date.format("%A · %-d %B").to_string();
    draw_text(img, &fonts.mono, 28.0, MARGIN as f32, 152.0, &date.to_uppercase(), MUTED);
    // Offline-queue badge, right-aligned on the date line.
    if model.pending > 0 {
        let s = format!("{} queued", model.pending);
        let sw = measure(&fonts.mono, 28.0, &s);
        draw_text(img, &fonts.mono, 28.0, (w - MARGIN) as f32 - sw, 152.0, &s, MUTED);
    }
    hline(img, MARGIN as f32, (w - MARGIN) as f32, 176.0, FAINT, 1);

    // The one thing — the day's chosen task, large serif; or the invitation to choose one.
    let eyebrow_y = 260.0;
    match &model.picked {
        Some(p) => {
            draw_text_tracked(img, &fonts.sans_semibold, 26.0, MARGIN as f32, eyebrow_y, "TODAY'S ONE THING", MUTED, 1.5);
            let lines = wrap(&fonts.serif_bold, 56.0, &p.title, (w - 2 * MARGIN) as f32, 3);
            let mut y = eyebrow_y + 76.0;
            for line in &lines {
                draw_text(img, &fonts.serif_bold, 56.0, MARGIN as f32, y, line, if model.done { MUTED } else { INK });
                if model.done {
                    // Struck through: the day's one thing is already done.
                    let lw = measure(&fonts.serif_bold, 56.0, line);
                    hline(img, MARGIN as f32, MARGIN as f32 + lw, y - 18.0, MUTED, 4);
                }
                y += 72.0;
            }
            if model.done {
                draw_text(img, &fonts.sans, 30.0, MARGIN as f32, y + 6.0, "Done for today ✓", GREEN);
            }
            // Tapping the task area reopens the picker (change your mind).
            hits.push((Rect::new(0.0, eyebrow_y - 40.0, w as f32, y - eyebrow_y + 40.0), Target::OpenPicker));
        }
        None => {
            draw_text_tracked(img, &fonts.sans_semibold, 26.0, MARGIN as f32, eyebrow_y, "NO TASK CHOSEN", MUTED, 1.5);
            draw_text(img, &fonts.serif_bold, 52.0, MARGIN as f32, eyebrow_y + 74.0, "What's the one thing", INK);
            draw_text(img, &fonts.serif_bold, 52.0, MARGIN as f32, eyebrow_y + 140.0, "you'll finish today?", INK);
            // Sized to clear the dial's 12-o'clock dot below (dot top ≈ y 534).
            let r = green_chip(img, fonts, "Choose your one thing", MARGIN, (eyebrow_y + 172.0) as i32, 520, 76);
            hits.push((r, Target::OpenPicker));
        }
    }

    // The progress dial — the screen's signature. Elapsed arc (INK; GOLD once the task is done)
    // grows clockwise from 12 o'clock over a FAINT track, a PAPER/GOLD dot riding its tip;
    // twelve faint 5-minute ticks. Advances once a minute, riding the minute-tick Full refresh.
    let secs = model.timer.remaining_secs(model.now);
    let total = model.timer.duration_min() as i64 * 60;
    let frac = if matches!(model.timer, Timer::Idle { .. }) || total == 0 {
        0.0
    } else {
        (1.0 - secs as f32 / total as f32).clamp(0.0, 1.0)
    };
    draw_dial(img, frac, if model.done { GOLD } else { GREEN });

    // The countdown inside the dial. Tapping the digits flips calm minutes ↔ live MM:SS.
    draw_digits(img, fonts, w, secs, model.seconds);
    let (dx, dy, dw, dh) = digits_rect(Canvas { width: w as u32, height: h as u32 });
    hits.push((Rect::new(dx as f32, dy as f32, dw as f32, dh as f32), Target::ToggleSeconds));
    // Session state, small, under the digits.
    let state = match &model.timer {
        Timer::Idle { duration_min } => format!("{duration_min}-minute session"),
        Timer::Running { .. } => "focusing".into(),
        Timer::Paused { .. } => "paused".into(),
        Timer::Finished { .. } => "finished".into(),
    };
    let sw = measure(&fonts.mono, 30.0, &state);
    draw_text(img, &fonts.mono, 30.0, (w as f32 - sw) / 2.0, (DIGITS_TOP + DIGITS_H + 34) as f32, &state, MUTED);

    // Play/pause lives INSIDE the dial's lower half — the primary control (per the reference).
    let play_ic = if matches!(model.timer, Timer::Running { .. }) { Icon::PauseFill } else { Icon::PlayFill };
    let (pcx, pcy) = (RING_CX as f32, (RING_CY + 232) as f32);
    icon(img, &fonts.icons, play_ic.ch(), pcx, pcy, 76.0, GREEN);
    hits.push((Rect::new(pcx - 70.0, pcy - 70.0, 140.0, 140.0), Target::PlayPause));

    // Session length — bare numbers under a quiet eyebrow (the unit lives in the state line, so
    // "min" isn't stamped four times across the row). Active length filled; inert mid-session.
    let idle = matches!(model.timer, Timer::Idle { .. });
    let chips_y = RING_CY + RING_R as i32 + 64;
    draw_text_tracked(img, &fonts.sans_semibold, 24.0, MARGIN as f32, (chips_y - 22) as f32, "SESSION LENGTH", if idle { MUTED } else { FAINT }, 1.5);
    let chip_w = 120;
    let gap = (w - 2 * MARGIN - 4 * chip_w) / 3;
    let mut cx = MARGIN;
    for &m in &PRESETS {
        let label = format!("{m}");
        let active = model.timer.duration_min() == m;
        let r = if active && idle {
            green_chip(img, fonts, &label, cx, chips_y, chip_w, 76)
        } else {
            chip(img, fonts, &label, cx, chips_y, chip_w, 76, false)
        };
        if !idle {
            // Fade the inert chips by re-outlining in FAINT (labels stay readable).
            rect_outline(img, r.x as f32, r.y as f32, r.w as f32, r.h as f32, FAINT, 2);
        } else {
            hits.push((r, Target::Preset(m)));
        }
        cx += chip_w + gap;
    }

    // Bottom bar: reset · pick task · exit (play/pause lives in the dial).
    let fy = h - BAR_H;
    hline(img, MARGIN as f32, (w - MARGIN) as f32, fy as f32, FAINT, 1);
    let icon_y = fy + 20;
    let slots = even_slots(w, MARGIN, 60, 3);
    hits.push((icon_btn(img, fonts, Icon::Undo, slots[0], icon_y, 60, 60, INK), Target::Reset));
    hits.push((icon_btn(img, fonts, Icon::Search, slots[1], icon_y, 60, 60, INK), Target::OpenPicker));
    hits.push((icon_btn(img, fonts, Icon::Exit, slots[2], icon_y, 60, 60, INK), Target::Exit));
}

/// A brand-green filled chip with a PAPER label — the app's primary-action button (the shared
/// `render::ui::chip` only knows INK; other apps keep their monochrome actives).
fn green_chip(img: &mut RgbImage, fonts: &Fonts, label: &str, x: i32, y: i32, wdt: i32, hgt: i32) -> Rect {
    fill_rect(img, x as f32, y as f32, wdt as f32, hgt as f32, GREEN);
    let tw = measure(&fonts.sans_semibold, 30.0, label);
    draw_text(img, &fonts.sans_semibold, 30.0, x as f32 + (wdt as f32 - tw) / 2.0, y as f32 + hgt as f32 * 0.62, label, PAPER);
    Rect::new(x as f32, y as f32, wdt as f32, hgt as f32)
}

/// The watch-face progress dial: a FAINT annulus track with the elapsed arc over it in `arc_ink`,
/// twelve faint 5-minute ticks on the midline, and a PAPER dot with a GOLD core riding the arc
/// tip. One per-pixel scan of the ring's bounding box does track + arc (angles measured clockwise
/// from 12 o'clock); at ~680px square once per redraw this is comfortably cheap.
fn draw_dial(img: &mut RgbImage, frac: f32, arc_ink: image::Rgb<u8>) {
    let (cx, cy) = (RING_CX as f32, RING_CY as f32);
    let (r_out, r_in) = (RING_R, RING_R - RING_STROKE);
    let sweep = frac.clamp(0.0, 1.0) * std::f32::consts::TAU;
    let (x0, x1) = ((cx - r_out) as i32, (cx + r_out).ceil() as i32);
    let (y0, y1) = ((cy - r_out) as i32, (cy + r_out).ceil() as i32);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let d2 = dx * dx + dy * dy;
            if d2 < r_in * r_in || d2 > r_out * r_out {
                continue;
            }
            // Angle clockwise from 12 o'clock, in [0, τ).
            let a = dx.atan2(-dy).rem_euclid(std::f32::consts::TAU);
            let ink = if frac > 0.0 && a <= sweep { arc_ink } else { FAINT };
            if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
                img.put_pixel(x as u32, y as u32, ink);
            }
        }
    }
    // 5-minute ticks: faint dots on the ring midline every 30°, skipping under the arc tip's dot.
    let r_mid = RING_R - RING_STROKE / 2.0;
    for k in 0..12 {
        let a = k as f32 / 12.0 * std::f32::consts::TAU;
        disc(img, cx + a.sin() * r_mid, cy - a.cos() * r_mid, 5.0, PAPER);
    }
    // The leading-edge marker: INK ring, PAPER fill, GOLD core — the family's one spark. Always
    // drawn (at 12 o'clock while idle: the start point, so an idle dial reads as ready, not dead).
    let (mx, my) = (cx + sweep.sin() * r_mid, cy - sweep.cos() * r_mid);
    disc(img, mx, my, 30.0, INK);
    disc(img, mx, my, 24.0, PAPER);
    disc(img, mx, my, 11.0, GOLD);
}

/// Draw the countdown centered in its stable band. Above the final minute: whole minutes left
/// ("47" giant + a small "min"), updating once a minute — e-ink kindness. Inside the final
/// minute: MM:SS with live seconds. Digits sit in fixed-width cells (the widest digit's advance)
/// so the block is pixel-stable tick to tick.
fn draw_digits(img: &mut RgbImage, fonts: &Fonts, w: i32, secs: i64, live_seconds: bool) {
    let cell = (b'0'..=b'9').map(|d| measure(&fonts.serif_bold, DIGIT_PX, &(d as char).to_string())).fold(0.0, f32::max);
    let base = (DIGITS_TOP + 150) as f32;
    if !live_seconds && secs > 60 {
        // Whole minutes, rounded up ("46:30 left" reads as 47 min) + a small unit label.
        let mins = format!("{}", (secs + 59) / 60);
        let unit = "min";
        let unit_px = 44.0;
        let mcell = (b'0'..=b'9').map(|d| measure(&fonts.serif_bold, MINUTES_PX, &(d as char).to_string())).fold(0.0, f32::max);
        let digits_w = mcell * mins.len() as f32;
        let unit_w = measure(&fonts.sans_semibold, unit_px, unit);
        let mut x = (w as f32 - (digits_w + 20.0 + unit_w)) / 2.0;
        for c in mins.chars() {
            let gw = measure(&fonts.serif_bold, MINUTES_PX, &c.to_string());
            draw_text(img, &fonts.serif_bold, MINUTES_PX, x + (mcell - gw) / 2.0, base, &c.to_string(), INK);
            x += mcell;
        }
        draw_text(img, &fonts.sans_semibold, unit_px, x + 20.0, base, unit, MUTED);
    } else {
        // The final minute: live seconds, colon at 60% of a cell.
        let text: Vec<char> = format!("{:02}:{:02}", secs / 60, secs % 60).chars().collect();
        let colon_w = cell * 0.6;
        let total: f32 = text.iter().map(|&c| if c == ':' { colon_w } else { cell }).sum();
        let mut x = (w as f32 - total) / 2.0;
        for &c in &text {
            let cw = if c == ':' { colon_w } else { cell };
            let gw = measure(&fonts.serif_bold, DIGIT_PX, &c.to_string());
            draw_text(img, &fonts.serif_bold, DIGIT_PX, x + (cw - gw) / 2.0, base, &c.to_string(), INK);
            x += cw;
        }
    }
}

fn draw_picker(img: &mut RgbImage, hits: &mut HitMap, model: &Model, fonts: &Fonts, w: i32, h: i32) {
    // Header: back chevron + title + fetch state / count + refresh + pager.
    hits.push((icon_btn(img, fonts, Icon::ChevronLeft, MARGIN, 48, 72, 72, INK), Target::Back));
    draw_text(img, &fonts.serif_bold, 56.0, (MARGIN + 96) as f32, 100.0, "Pick one thing", INK);
    let filtered = model.filtered();
    let per = rows_per_page(Canvas { width: w as u32, height: h as u32 });
    let shown_end = (model.scroll + per).min(filtered.len());
    let sub = if model.fetching {
        "Fetching tasks…".to_string()
    } else if filtered.is_empty() {
        "No open tasks match.".to_string()
    } else {
        format!("{}–{} of {}", model.scroll + 1, shown_end, filtered.len())
    };
    draw_text(img, &fonts.mono, 28.0, (MARGIN + 96) as f32, 144.0, &sub, MUTED);
    hits.push((icon_btn(img, fonts, Icon::Undo, w - MARGIN - 216, 48, 72, 72, INK), Target::Refresh));
    if model.scroll > 0 {
        hits.push((icon_btn(img, fonts, Icon::ChevronLeft, w - MARGIN - 144, 48, 72, 72, INK), Target::PagePrev));
    }
    if shown_end < filtered.len() {
        hits.push((icon_btn(img, fonts, Icon::ChevronRight, w - MARGIN - 72, 48, 72, 72, INK), Target::PageNext));
    }
    hline(img, MARGIN as f32, (w - MARGIN) as f32, (PICKER_HEADER_H - 2) as f32, FAINT, 1);

    // Query bar (reader-style: outlined, caret, ✕ clears).
    let (qx, qy, qw) = (MARGIN, PICKER_HEADER_H + 8, w - 2 * MARGIN);
    rect_outline(img, qx as f32, qy as f32, qw as f32, QBAR_H as f32, INK, 2);
    let clear = icon_btn(img, fonts, Icon::Close, qx + qw - QBAR_H, qy, QBAR_H, QBAR_H, MUTED);
    hits.push((clear, Target::ClearQuery));
    let base = (qy + QBAR_H / 2) as f32 + 14.0;
    if model.query.is_empty() {
        draw_text(img, &fonts.sans, 40.0, (qx + 20) as f32, base, "Search tasks…", FAINT);
        fill_rect(img, (qx + 20) as f32, base - 40.0, 3.0, 50.0, INK);
    } else {
        let shown = truncate(&fonts.sans, 40.0, &model.query, (qw - 40 - QBAR_H) as f32);
        let tw = measure(&fonts.sans, 40.0, &shown);
        draw_text(img, &fonts.sans, 40.0, (qx + 20) as f32, base, &shown, INK);
        fill_rect(img, (qx + 20) as f32 + tw + 3.0, base - 40.0, 3.0, 50.0, INK);
    }

    // Task rows.
    let rows_top = PICKER_HEADER_H + 8 + QBAR_H + 8;
    for (row, &ix) in filtered.iter().enumerate().skip(model.scroll).take(per) {
        let t = &model.tasks[ix];
        let y = rows_top + (row - model.scroll) as i32 * ROW_H;
        let cy = (y + ROW_H / 2) as f32;
        // Priority tick: a gold disc for p1/p2 (Todoist priority 4 = urgent).
        let mut tx = MARGIN as f32;
        if t.priority >= 3 {
            disc(img, tx + 10.0, cy, 10.0, GOLD);
        }
        tx += 40.0;
        // Due, right-aligned mono; overdue in gold.
        let due = t.due.map(|d| due_label(d, model.date)).unwrap_or_default();
        let dw = if due.is_empty() { 0.0 } else { measure(&fonts.mono, 26.0, &due) };
        if !due.is_empty() {
            let ink = if t.due.is_some_and(|d| d < model.date) { GOLD } else { MUTED };
            draw_text(img, &fonts.mono, 26.0, (w - MARGIN) as f32 - dw, cy + 9.0, &due, ink);
        }
        let title = truncate(&fonts.sans, 36.0, &t.title, (w - MARGIN) as f32 - tx - dw - 24.0);
        draw_text(img, &fonts.sans, 36.0, tx, cy + 12.0, &title, INK);
        hline(img, MARGIN as f32, (w - MARGIN) as f32, (y + ROW_H) as f32, FAINT, 1);
        hits.push((Rect::new(0.0, y as f32, w as f32, ROW_H as f32), Target::Row(ix)));
    }

    // The shared on-screen keyboard, docked; Enter = "Pick" (top match).
    for (rect, cap) in draw_keyboard(img, fonts, h - KB_H, w, model.layer, model.shift, "Pick") {
        let target = match cap {
            KeyCap::Char(c) => Target::Key(c),
            KeyCap::Space => Target::Key(' '),
            KeyCap::Backspace => Target::Backspace,
            KeyCap::Shift => Target::Shift,
            KeyCap::Layer => Target::Layer,
            KeyCap::Enter => Target::Submit,
        };
        hits.push((rect, target));
    }
}

/// Relative due label: "today" / "tomorrow" / "Mon 20 Jul" (past dates render plainly and gold).
fn due_label(due: NaiveDate, today: NaiveDate) -> String {
    if due == today {
        "today".into()
    } else if due == today + chrono::Duration::days(1) {
        "tomorrow".into()
    } else {
        format!("{} {} {}", ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][due.weekday().num_days_from_monday() as usize], due.day(), ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][due.month0() as usize])
    }
}

fn draw_done(img: &mut RgbImage, hits: &mut HitMap, model: &Model, fonts: &Fonts, w: i32, h: i32) {
    let center = |img: &mut RgbImage, font, px: f32, y: f32, text: &str, ink| {
        let tw = measure(font, px, text);
        draw_text(img, font, px, (w as f32 - tw) / 2.0, y, text, ink);
    };
    center(img, &fonts.serif_bold, 110.0, 420.0, "Time's up.", INK);
    let mins = model.timer.duration_min();
    center(img, &fonts.mono, 30.0, 500.0, &format!("{mins} minutes focused"), MUTED);
    if let Some(p) = &model.picked {
        let lines = wrap(&fonts.serif_bold, 44.0, &p.title, (w - 2 * MARGIN) as f32, 2);
        let mut y = 620.0;
        for line in &lines {
            center(img, &fonts.serif_bold, 44.0, y, line, INK);
            y += 58.0;
        }
    }

    // Two big actions + a quiet back.
    let bw = 620;
    let bx = (w - bw) / 2;
    let done_btn = green_chip(img, fonts, "Mark done in Todoist", bx, 860, bw, 104);
    hits.push((done_btn, Target::MarkDone));
    let again = chip(img, fonts, "Another session", bx, 1000, bw, 104, false);
    hits.push((again, Target::GoAgain));
    center(img, &fonts.sans, 32.0, 1180.0, "Back", MUTED);
    hits.push((Rect::new((w / 2 - 120) as f32, 1130.0, 240.0, 80.0), Target::Dismiss));
    let _ = h;
}

fn draw_flash(img: &mut RgbImage, model: &Model, fonts: &Fonts, w: i32, h: i32) {
    let Some(f) = &model.flash else { return };
    let y = h - BAR_H - 48;
    let tw = measure(&fonts.sans_semibold, 30.0, &f.text);
    let pad = 24.0;
    fill_rect(img, (w as f32 - tw) / 2.0 - pad, y as f32 - 40.0, tw + 2.0 * pad, 58.0, CHIP);
    draw_text(img, &fonts.sans_semibold, 30.0, (w as f32 - tw) / 2.0, y as f32, &f.text, INK);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Picked;
    use chrono::TimeZone;

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn model() -> Model {
        Model::new(chrono::Local.with_ymd_and_hms(2026, 7, 14, 9, 0, 0).unwrap())
    }

    #[test]
    fn home_has_timer_controls_and_the_picker_invitation_when_unpicked() {
        let m = model();
        let out = view(&m, &Fonts::embedded(), CANVAS);
        assert!(out.hits.iter().any(|(_, t)| *t == Target::PlayPause));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::OpenPicker));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Preset(60)));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Exit));
    }

    #[test]
    fn presets_are_inert_mid_session() {
        let mut m = model();
        m.timer = m.timer.started(m.now);
        let out = view(&m, &Fonts::embedded(), CANVAS);
        assert!(!out.hits.iter().any(|(_, t)| matches!(t, Target::Preset(_))));
    }

    #[test]
    fn digits_rect_is_constant_and_inside_the_canvas() {
        let (x, y, w, h) = digits_rect(CANVAS);
        assert!(x >= 0 && y >= 0 && x + w <= 954 && y + h <= 1696);
    }

    #[test]
    fn picker_shows_keyboard_rows_and_submit() {
        let mut m = model();
        m.screen = Screen::Picker;
        m.tasks = vec![sb_core::Task {
            id: "1".into(),
            title: "A task".into(),
            priority: 1,
            due: None,
            duration_min: 0,
            done: false,
            is_recurring: false,
        }];
        let out = view(&m, &Fonts::embedded(), CANVAS);
        assert!(out.hits.iter().any(|(_, t)| matches!(t, Target::Key(_))));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Submit));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Row(0)));
    }

    #[test]
    fn done_screen_offers_mark_done_and_go_again() {
        let mut m = model();
        m.picked = Some(Picked { id: "1".into(), title: "The thing".into() });
        m.timer = Timer::Finished { duration_min: 60 };
        m.screen = Screen::Done;
        let out = view(&m, &Fonts::embedded(), CANVAS);
        assert!(out.hits.iter().any(|(_, t)| *t == Target::MarkDone));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::GoAgain));
    }
}
