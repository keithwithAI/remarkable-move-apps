//! Render an [`AgendaState`] to a PNG for the suspend screen: a portrait "day rail".
//!
//! This is print design, not UI — a glanceable daily timetable answering "where am I in
//! today, and what's left?" from arm's length. A letterpress-scale date block sits above a
//! vertical timeline rail; auto-scheduled task blocks hang their priority color on the rail; a
//! filled NOW dot + hairline + clock marks the current moment. Nothing draws a checkbox —
//! nothing here is tappable.
//!
//! Type system (IBM Plex, OFL, embedded): serif bold for the date numeral only, sans for
//! titles and labels, mono for every time figure so colons align down the page.

pub mod draw;
pub mod ui;

/// The shared tap-target rectangle — re-exported at the crate root so apps can `use render::Rect`.
pub use ui::Rect;

use ab_glyph::FontRef;
use draw::{
    disc, draw_text, draw_text_tracked, fill_rect, hline, measure, recur_arrows, truncate, vline,
};
use image::{Rgb, RgbImage};
use sb_core::{AgendaState, Event};

pub const INK: Rgb<u8> = Rgb([0, 0, 0]); // black
pub const MUTED: Rgb<u8> = Rgb([51, 51, 51]); // dark grey for secondary text — kept dark so bylines/dates/meta stay legible on the Gallery-3 panel (mid-greys wash out on e-ink)
pub const FAINT: Rgb<u8> = Rgb([170, 170, 170]); // light grey (rules)
pub const PAPER: Rgb<u8> = Rgb([255, 255, 255]); // white

/// The scheduler's quadrant palette (Google Calendar event colors), keyed by **Todoist priority**
/// (4 = P1 … 1 = P4): Tomato, Tangerine, Banana, Sage. Originated with Focustask; Diarized emits
/// the same colorIds, so the mapping outlived the backend swap (see `sb_core::Event::priority`).
pub fn priority_color(priority: u8) -> Rgb<u8> {
    match priority {
        4 => Rgb([213, 0, 0]),    // P1 Tomato
        3 => Rgb([244, 81, 30]),  // P2 Tangerine
        2 => Rgb([246, 191, 38]), // P3 Banana
        1 => Rgb([51, 182, 121]), // P4 Sage
        _ => MUTED,
    }
}

/// How far the row-tint wash sits toward white (0 = full color, 1 = white). The Gallery 3 panel
/// mutes colour HARD on its own — washing 55% toward white on top of that read washed-out, so the
/// software wash is now mild (0.30): rows carry a genuinely vivid band and the panel does the
/// softening. (Same lesson as the browser's ink_px: pre-muting for e-ink double-mutes.)
pub const TINT_WASH: f32 = 0.30;

/// The pale row-background wash for a priority.
pub fn priority_tint(priority: u8) -> Rgb<u8> {
    let c = priority_color(priority);
    Rgb([wash(c[0]), wash(c[1]), wash(c[2])])
}

fn wash(channel: u8) -> u8 {
    (255.0 * TINT_WASH + channel as f32 * (1.0 - TINT_WASH)).round() as u8
}

/// Output geometry. Default is the Move's portrait panel; the suspend orientation is still
/// `[verify on device]`, hence configurable.
#[derive(Debug, Clone, Copy)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}
impl Default for Canvas {
    fn default() -> Self {
        Canvas { width: 954, height: 1696 }
    }
}

/// The embedded IBM Plex faces, by role.
pub struct Fonts {
    /// Titles and body copy.
    pub sans: FontRef<'static>,
    /// Emphasis: headers, active items, labels.
    pub sans_semibold: FontRef<'static>,
    /// Reader titles (list rows, article, panel headers) + the sleep screen's date numeral.
    pub serif_bold: FontRef<'static>,
    /// Every time, duration, and status figure (timetable voice).
    pub mono: FontRef<'static>,
    /// Bootstrap Icons (MIT) subset — the reader's UI glyphs (star, archive, tag, search, chevrons,
    /// …). Drawn via `draw::icon`; see `assets/fonts/bootstrap-icons-LICENSE.txt`.
    pub icons: FontRef<'static>,
}

static SANS: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-Regular.ttf");
static SANS_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSans-SemiBold.ttf");
static SERIF_BOLD: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexSerif-Bold.ttf");
static MONO: &[u8] = include_bytes!("../../../assets/fonts/IBMPlexMono-Regular.ttf");
static ICONS: &[u8] = include_bytes!("../../../assets/fonts/bootstrap-icons.ttf");

impl Fonts {
    pub fn embedded() -> Self {
        Fonts {
            sans: FontRef::try_from_slice(SANS).expect("embedded sans is valid"),
            sans_semibold: FontRef::try_from_slice(SANS_SEMIBOLD).expect("embedded semibold is valid"),
            serif_bold: FontRef::try_from_slice(SERIF_BOLD).expect("embedded serif is valid"),
            mono: FontRef::try_from_slice(MONO).expect("embedded mono is valid"),
            icons: FontRef::try_from_slice(ICONS).expect("embedded icons are valid"),
        }
    }
}

/// Type scale: the sleep-screen agenda draws ~1.15× larger text — a touch bigger than the original,
/// small enough that a full day still fits. Applied to every font size and the row metrics that carry
/// text; the letterpress date numeral and the absolute date-block anchors are handled explicitly.
const TS: f32 = 1.15;
const M: f32 = 64.0; // page margin
const RAIL_X: f32 = 76.0;
const TIME_X: f32 = 112.0;
const TITLE_X: f32 = 260.0; // clears the time column at this scale
const ROW_H: f32 = 84.0 * TS;
const NOW_H: f32 = 48.0 * TS;
const FOOTER_H: f32 = 120.0;

/// Render the agenda. Returns an RGB image ready to encode as PNG.
pub fn render_agenda(state: &AgendaState, canvas: Canvas, fonts: &Fonts) -> RgbImage {
    let mut img = RgbImage::from_pixel(canvas.width, canvas.height, PAPER);
    let w = canvas.width as f32;
    let h = canvas.height as f32;

    let rule_y = draw_date_block(&mut img, state, fonts, w);
    draw_footer(&mut img, state, fonts, w, h);

    let has_schedule = state.now.is_some() || !state.upcoming.is_empty();

    if !has_schedule {
        let msg = "Nothing scheduled — enjoy it.";
        let px = fit_px(&fonts.serif_bold, msg, w - 2.0 * M, TS * 60.0);
        let mw = measure(&fonts.serif_bold, px, msg);
        draw_text(&mut img, &fonts.serif_bold, px, (w - mw) / 2.0, h * 0.45, msg, INK);
        return img;
    }

    // The schedule owns the whole page between the date block and the footer — the full agenda.
    draw_schedule(&mut img, state, fonts, w, rule_y + 46.0, h - FOOTER_H);

    img
}

/// Letterpress date block: tracked weekday eyebrow + status, serif numeral + month, heavy rule.
/// Returns the rule's y.
fn draw_date_block(img: &mut RgbImage, state: &AgendaState, fonts: &Fonts, w: f32) -> f32 {
    let weekday = state.date.format("%A").to_string().to_uppercase();
    draw_text_tracked(img, &fonts.sans_semibold, TS * 36.0, M, 136.0, &weekday, MUTED, TS * 10.0);

    let status = if state.stale {
        format!("offline · {}", state.generated_at.format("%H:%M"))
    } else {
        format!("updated {}", state.generated_at.format("%H:%M"))
    };
    let ink = if state.stale { INK } else { MUTED };
    let sw = measure(&fonts.mono, TS * 26.0, &status);
    draw_text(img, &fonts.mono, TS * 26.0, w - M - sw, 136.0, &status, ink);

    // The day numeral stays the letterpress hero (300 px); only the words around it grow.
    let numeral = state.date.format("%-d").to_string();
    let nw = draw_text(img, &fonts.serif_bold, 300.0, M - 10.0, 430.0, &numeral, INK);
    let month = state.date.format("%B %Y").to_string();
    draw_text(img, &fonts.serif_bold, TS * 52.0, M + nw + 34.0, 430.0, &month, INK);

    let rule_y = 486.0;
    hline(img, M, w - M, rule_y, INK, 3);
    rule_y
}

/// The day rail: eyebrow, timeline rows (active + upcoming, task blocks tinted), NOW marker.
fn draw_schedule(img: &mut RgbImage, state: &AgendaState, fonts: &Fonts, w: f32, top: f32, bottom: f32) {
    let has_items = state.now.is_some() || !state.upcoming.is_empty();

    let eyebrow_base = top + 28.0;
    draw_text_tracked(img, &fonts.sans_semibold, TS * 26.0, M, eyebrow_base, "SCHEDULE", MUTED, TS * 8.0);
    let remaining = state.upcoming.len() + state.now.is_some() as usize;
    if has_items {
        let count = format!("{remaining} left");
        let cw = measure(&fonts.mono, TS * 24.0, &count);
        draw_text(img, &fonts.mono, TS * 24.0, w - M - cw, eyebrow_base, &count, MUTED);
    }
    let rows_top = top + 56.0;

    if !has_items {
        draw_text(img, &fonts.serif_bold, TS * 60.0, M, rows_top + 92.0, "Day's clear.", INK);
        return;
    }

    // Fit: active row + NOW marker + as many upcoming as the space allows.
    let mut budget = bottom - rows_top;
    let active_rows = state.now.is_some() as usize;
    budget -= active_rows as f32 * ROW_H + NOW_H;
    let cap = (budget / ROW_H).floor().max(0.0) as usize;
    let shown = if state.upcoming.len() > cap {
        cap.saturating_sub(1) // keep a slot for the "… more" line
    } else {
        state.upcoming.len()
    };

    let rail_bottom = rows_top
        + active_rows as f32 * ROW_H
        + NOW_H
        + shown as f32 * ROW_H
        + if shown < state.upcoming.len() { 44.0 } else { 0.0 };
    vline(img, RAIL_X, rows_top - 4.0, rail_bottom - 12.0, FAINT, 2);

    let mut y = rows_top;
    if let Some(active) = &state.now {
        draw_schedule_row(img, active, state, fonts, w, y, true);
        y += ROW_H;
    }
    y = draw_now_marker(img, state, fonts, w, y);
    for e in state.upcoming.iter().take(shown) {
        draw_schedule_row(img, e, state, fonts, w, y, false);
        y += ROW_H;
    }
    if shown < state.upcoming.len() {
        draw_text(img, &fonts.mono, TS * 26.0, TITLE_X, y + TS * 30.0, &format!("… {} more", state.upcoming.len() - shown), MUTED);
    }
}

fn draw_schedule_row(
    img: &mut RgbImage,
    e: &Event,
    state: &AgendaState,
    fonts: &Fonts,
    w: f32,
    y: f32,
    active: bool,
) {
    let matched = e
        .todoist_task
        .as_deref()
        .and_then(|id| state.tasks.iter().find(|t| t.id == id));
    let pri = e.priority(matched);

    // Auto-scheduled task block: tinted band across the row + a saturated flag on the rail.
    if let Some(p) = pri {
        fill_rect(img, RAIL_X + 12.0, y + 2.0, w - M - RAIL_X - 4.0, ROW_H - 12.0, priority_tint(p));
        fill_rect(img, RAIL_X - 5.0, y + 2.0, 12.0, ROW_H - 12.0, priority_color(p));
    }

    let baseline = y + TS * 52.0;
    let time = e.start.format("%H:%M").to_string();
    draw_text(img, &fonts.mono, TS * 32.0, TIME_X, baseline, &time, INK);

    let dur = format!("{:>4}", format!("{}m", e.duration_min()));
    let dur_w = measure(&fonts.mono, TS * 24.0, &dur);
    draw_text(img, &fonts.mono, TS * 24.0, w - M - dur_w, baseline, &dur, MUTED);

    let mut right = dur_w + 20.0;
    if let Some(p) = pri {
        let tag = format!("P{}", 5u8.saturating_sub(p).max(1));
        let tag_w = measure(&fonts.mono, TS * 24.0, &tag);
        draw_text(img, &fonts.mono, TS * 24.0, w - M - right - tag_w, baseline, &tag, MUTED);
        right += tag_w + 20.0;
        if matched.map(|t| t.is_recurring).unwrap_or(false) {
            recur_arrows(img, w - M - right - 12.0, baseline - 10.0, 11.0, MUTED);
            right += 34.0;
        }
    }

    let px = if active { TS * 38.0 } else { TS * 36.0 };
    let title = truncate(&fonts.sans_semibold, px, &e.title, w - M - right - TITLE_X);
    draw_text(img, &fonts.sans_semibold, px, TITLE_X, baseline, &title, INK);
}

/// The shared signature: filled dot on the rail, the current time, a hairline to the margin.
fn draw_now_marker(img: &mut RgbImage, state: &AgendaState, fonts: &Fonts, w: f32, y: f32) -> f32 {
    let cy = y + NOW_H / 2.0;
    disc(img, RAIL_X + 1.0, cy, 11.0, INK);
    let clock = state.generated_at.format("%H:%M").to_string();
    draw_text(img, &fonts.mono, TS * 28.0, TIME_X, cy + TS * 10.0, &clock, INK);
    let clock_w = measure(&fonts.mono, TS * 28.0, &clock);
    hline(img, TIME_X + clock_w + 18.0, w - M, cy, INK, 2);
    y + NOW_H
}

fn draw_footer(img: &mut RgbImage, state: &AgendaState, fonts: &Fonts, w: f32, h: f32) {
    hline(img, M, w - M, h - 104.0, FAINT, 1);
    let text = if state.stale {
        format!("offline · showing data from {}", state.generated_at.format("%H:%M"))
    } else {
        format!("second brain · fetched {}", state.generated_at.format("%H:%M"))
    };
    let ink = if state.stale { INK } else { MUTED };
    draw_text(img, &fonts.mono, TS * 24.0, M, h - 56.0, &text, ink);
}

/// Shrink `start_px` until `text` fits `max_w`.
fn fit_px(font: &FontRef, text: &str, max_w: f32, start_px: f32) -> f32 {
    let mut px = start_px;
    while px > 24.0 && measure(font, px, text) > max_w {
        px -= 2.0;
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use sb_core::Task;

    fn d(h: u32, mi: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 7, 2, h, mi, 0).unwrap()
    }
    fn plain(title: &str, sh: u32, eh: u32) -> Event {
        Event {
            title: title.into(),
            start: d(sh, 0),
            end: d(eh, 0),
            calendar_id: "primary".into(),
            id: None,
            description: String::new(),
            color_id: None,
            todoist_task: None,
        }
    }
    fn block(id: &str, title: &str, color: u8, sh: u32, eh: u32) -> Event {
        Event { color_id: Some(color), todoist_task: Some(id.into()), ..plain(title, sh, eh) }
    }
    fn task(id: &str, p: u8, recurring: bool) -> Task {
        Task {
            id: id.into(),
            title: format!("task {id}"),
            priority: p,
            due: Some(d(9, 0).date_naive()),
            duration_min: 30,
            done: false,
            is_recurring: recurring,
        }
    }

    fn sample() -> AgendaState {
        let events = vec![
            plain("Standup", 9, 10),
            block("t1", "Ship the planner", 11, 11, 12),
            plain("Lunch with a very long companion name that truncates", 13, 14),
            block("t2", "Water the plants", 5, 15, 16),
        ];
        let tasks = vec![task("t1", 4, false), task("t2", 2, true), task("t9", 3, false)];
        AgendaState::build(events, tasks, d(10, 30))
    }

    #[test]
    fn renders_portrait_nonblank_with_color() {
        let img = render_agenda(&sample(), Canvas::default(), &Fonts::embedded());
        assert_eq!(img.dimensions(), (954, 1696));
        let inked = img.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(inked > 5000, "expected substantial ink, got {inked}");
        let colored = img.pixels().filter(|p| p[0] != p[1] || p[1] != p[2]).count();
        assert!(colored > 3000, "expected tinted blocks, got {colored}");
    }

    #[test]
    fn empty_states_render() {
        let fonts = Fonts::embedded();
        // No schedule at all → the centered "nothing scheduled" message, still substantial ink.
        let none = AgendaState::build(vec![], vec![], d(20, 0));
        let img = render_agenda(&none, Canvas::default(), &fonts);
        assert_eq!(img.dimensions(), (954, 1696));
        let inked = img.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(inked > 2000, "expected the 'nothing scheduled' message, got {inked}");

        // Tasks with no events no longer add an "on your plate" section — the sleep screen shows
        // only the schedule, so a tasks-only state still reads as nothing scheduled (no color).
        let tasks_only = AgendaState::build(vec![], vec![task("t9", 3, false)], d(20, 0));
        let img2 = render_agenda(&tasks_only, Canvas::default(), &fonts);
        assert_eq!(img2.dimensions(), (954, 1696));
        let colored = img2.pixels().filter(|p| p[0] != p[1] || p[1] != p[2]).count();
        assert!(colored < 100, "tasks-only draws no colored task rows now, got {colored}");
    }

    #[test]
    fn all_faces_cover_the_strings_we_set() {
        let fonts = Fonts::embedded();
        for (name, font) in [
            ("sans", &fonts.sans),
            ("semibold", &fonts.sans_semibold),
            ("serif", &fonts.serif_bold),
            ("mono", &fonts.mono),
        ] {
            let mut img = RgbImage::from_pixel(900, 120, PAPER);
            let w = draw_text(&mut img, font, 40.0, 10.0, 80.0, "0123456789 SCHEDULE · … — Day's clear", INK);
            let inked = img.pixels().filter(|p| p[0] < 128).count();
            assert!(inked > 400 && w > 500.0, "{name}: coverage looks wrong (ink {inked}, adv {w})");
        }
    }

    #[test]
    fn priority_palette_and_tint() {
        assert_eq!(priority_color(4), Rgb([213, 0, 0]), "P1 = Tomato");
        assert_eq!(priority_color(1), Rgb([51, 182, 121]), "P4 = Sage");
        let tint = priority_tint(4);
        assert!(tint[0] > tint[1] && tint[0] > tint[2], "P1 tint stays reddish: {tint:?}");
        // A vivid wash (TINT_WASH 0.30): clearly red, but still lifted off full-strength Tomato
        // so black text stays legible on the band. 213*0.7+255*0.3 ≈ 226 red, ~77 green.
        assert!(tint[0] > 220 && tint[0] < 240, "tint keeps a strong red: {tint:?}");
        assert!(tint[1] > 60 && tint[1] < 110, "green lifted but not pale: {tint:?}");
    }
}
