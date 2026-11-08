//! Draw the model into a portrait RGB frame. The [`HitMap`] is produced as a byproduct of
//! layout — every interactive rect is registered at the moment it is drawn, so hit-testing
//! can never drift from the pixels.
//!
//! The day view is **one merged chronological list of calendar items**: Diarized-scheduled
//! task blocks (recognized via `Event::todoist_task`) render as checkable rows washed in
//! their priority color; ordinary events render plain. Tasks Diarized hasn't scheduled are
//! not shown ("calendar only").
//!
//! Visually this is the Agenda's cut of the shared **"Reading Desk"** language (see
//! [`render::ui`]): a serif masthead, an even-spaced Bootstrap-icon action bar, and the task
//! action card as a left-docked slide-in panel. The type mix stays deliberately functional —
//! titles in sans (this is a planner you scan, not prose you read) over mono time figures.
//!
//! Geometry targets the Move's 954×1696 portrait panel at ~266 ppi: every tap target is at
//! least [`MIN_TAP_PX`] (≈9 mm) in both dimensions.

use crate::defer::{defer_label_parts, DeferChoice};
use crate::model::{AppModel, DueChoice, Overlay, Prio, QuickAdd};
use chrono::{DateTime, Local, NaiveDate};
use image::RgbImage;
use render::draw::{
    checkbox, disc, draw_text, draw_text_tracked, fill_rect, hline, line, measure, recur_arrows, truncate,
};
use render::ui::{
    chip, draw_bottom_sheet, draw_keyboard, even_slots, icon_btn, Icon, KeyCap, KB_H, MIN_KEY_PX,
    PANEL_PAD, SHEET_HEADER_H,
};
use render::{priority_color, priority_tint, Canvas, Fonts, FAINT, INK, MUTED, PAPER};
use sb_core::Event;

/// The shared tap-target rect (from [`render::ui`]) — one type across both apps.
pub use render::Rect;

/// Minimum tap-target edge (≈9 mm at 266 ppi).
pub const MIN_TAP_PX: i32 = 94;

/// Type scale: the planner draws ~1.15× larger text — a touch bigger than the original, small enough
/// that a full day still fits. Applied to font sizes and the row/line metrics that carry text;
/// structural widths (checkbox, margins, header and bottom-bar heights) stay put.
pub const TS: f32 = 1.15;
pub const HEADER_H: f32 = 156.0;
pub const BOTTOM_BAR_H: f32 = 176.0;
pub const ROW_H: f32 = 96.0 * TS;
/// Width of the checkbox tap region at the left of a task-block row.
pub const CHECK_W: f32 = 150.0;
const MARGIN: f32 = 24.0;

/// Bottom action-bar geometry: six even cells (‹ · today ● · refresh ↻ · add + · exit ⭲ · ›).
const BAR_MARGIN: i32 = 44;
const ICELL_W: f32 = 112.0;
const ICELL_H: f32 = 100.0;

/// Task action-sheet heights — taller when the three defer rows are shown. Sized to the content
/// stack in [`draw_overlay`] (header + Complete + defers/note + priority chips + Delete + padding).
pub const SHEET_H_DEFER: i32 = 854;
pub const SHEET_H_NODEFER: i32 = 638;
/// The plain-event card: header + one Delete row.
pub const SHEET_H_EVENT: i32 = 360;
/// The quick-add sheet is tall: header + typed line + due chips + priority chips, over the
/// docked keyboard.
pub const QUICK_SHEET_H: i32 = 908;
/// The tallest sheet: the bottom band that open/close partial-refresh must repaint (see
/// [`sheet_region`]). Must cover the task card at its *fullest* — with the defer rows — or the
/// partial refresh leaves a strip of the old frame behind.
const SHEET_H_TASK_MAX: i32 = SHEET_H_DEFER;
const SHEET_H_MAX: i32 =
    if QUICK_SHEET_H > SHEET_H_TASK_MAX { QUICK_SHEET_H } else { SHEET_H_TASK_MAX };

/// What a tap on a region means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    PrevDay,
    NextDay,
    JumpToday,
    Refresh,
    /// Leave the app, returning to the AppLoad launcher.
    Exit,
    /// Complete directly (checkbox region of a task-block row). Ids are Todoist task ids.
    Checkbox { task_id: String },
    /// Open the action card for a task block.
    TaskRow { task_id: String },
    /// Open the action card for a plain calendar event (only pushed when the event has a
    /// Google id — pre-v0.14 cached events regain tappability on the next fetch).
    EventRow { calendar_id: String, event_id: String, title: String },
    /// The event card's Delete button (removes the event from Google Calendar).
    CardDeleteEvent,
    /// The "… N more" overflow row: page the day list down (`skip` = the new scroll offset).
    MoreEvents { skip: usize },
    /// The "… N earlier" row shown while scrolled: jump back to the top of the day.
    EarlierEvents,
    /// The card's Complete button.
    CardComplete,
    /// The card's Delete button (removes the task from Todoist entirely).
    CardDelete,
    DeferChoice(DeferChoice),
    /// Open the quick-add sheet (the "+" in the bottom bar).
    QuickAdd,
    /// A keyboard character key (space included).
    Key(char),
    KbBackspace,
    KbShift,
    /// Toggle the keyboard between letters and numbers/symbols.
    KbLayerToggle,
    /// Pick where a quick-added task lands.
    QuickAddDue(DueChoice),
    /// Pick a quick-added task's priority.
    QuickAddPrio(Prio),
    /// Reprioritise the task on the open action card.
    CardPriority(Prio),
    /// Commit the quick-add (the keyboard's Add key).
    QuickAddSubmit,
    /// The overlay panel body — swallows taps so they don't fall through to dismiss.
    CardBody,
    DismissOverlay,
}

impl Target {
    /// Targets that trigger an action (used for tap-feedback inversion and size checks).
    pub fn is_interactive(&self) -> bool {
        !matches!(self, Target::CardBody | Target::DismissOverlay)
    }
    /// Minimum finger size for this target. Keyboard keys form a dense correctable grid, so they
    /// clear the smaller [`MIN_KEY_PX`]; every other control clears the full [`MIN_TAP_PX`].
    pub fn min_tap(&self) -> i32 {
        match self {
            Target::Key(_)
            | Target::KbBackspace
            | Target::KbShift
            | Target::KbLayerToggle
            | Target::QuickAddSubmit => MIN_KEY_PX,
            _ => MIN_TAP_PX,
        }
    }
}

/// Interactive regions in draw order (later entries are on top; see [`hit_test`]).
pub type HitMap = Vec<(Rect, Target)>;

/// Topmost hit wins — the map is searched in reverse draw order.
pub fn hit_test(hits: &HitMap, x: i32, y: i32) -> Option<&(Rect, Target)> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y))
}

pub struct ViewOutput {
    pub frame: RgbImage,
    pub hits: HitMap,
}

/// The bottom status/action bar (partial-refresh hint for badge/flash updates).
pub fn bottom_bar_rect(canvas: Canvas) -> Rect {
    Rect::new(0.0, canvas.height as f32 - BOTTOM_BAR_H, canvas.width as f32, BOTTOM_BAR_H)
}

/// The bottom band a task action sheet can occupy — the partial-refresh region when the sheet
/// opens/closes. The day list above it is untouched (the sheet has no scrim), so only this band
/// changes; redrawing it avoids a full-screen e-ink flash.
pub fn sheet_region(canvas: Canvas) -> Rect {
    Rect::new(0.0, (canvas.height as i32 - SHEET_H_MAX) as f32, canvas.width as f32, SHEET_H_MAX as f32)
}

pub fn view(model: &AppModel, fonts: &Fonts, canvas: Canvas, now: DateTime<Local>) -> ViewOutput {
    let mut img = RgbImage::from_pixel(canvas.width, canvas.height, PAPER);
    let mut hits: HitMap = Vec::new();
    let w = canvas.width as f32;
    let h = canvas.height as f32;

    draw_header(&mut img, fonts, model, w);
    draw_day(&mut img, &mut hits, model, fonts, w, h, now);
    draw_bottom_bar(&mut img, &mut hits, model, fonts, w, h);

    if let Some(overlay) = &model.overlay {
        // Modality is structural: day-level hits stay in the map but are fully covered by the
        // screen-wide dismiss rect pushed *after* them; only overlay rects sit on top of it.
        hits.push((Rect::new(0.0, 0.0, w, h), Target::DismissOverlay));
        draw_overlay(&mut img, &mut hits, model, overlay, fonts, canvas);
    }

    ViewOutput { frame: img, hits }
}

/// The masthead: a serif weekday headline over a tracked-caps date deck, with the relative
/// marker (TODAY / TOMORROW / YESTERDAY) accented. Not itself a tap target — day paging and
/// the jump-to-today control live in the bottom bar.
fn draw_header(img: &mut RgbImage, fonts: &Fonts, model: &AppModel, w: f32) {
    let weekday = model.view_date.format("%A").to_string();
    draw_text(img, &fonts.serif_bold, TS * 52.0, MARGIN, 88.0, &weekday, INK);

    let date = model.view_date.format("%-d %B %Y").to_string().to_uppercase();
    let mut x = MARGIN + draw_text_tracked(img, &fonts.sans_semibold, TS * 22.0, MARGIN, 130.0, &date, MUTED, 2.0);
    if let Some(marker) = relative_marker(model.view_date, model.today) {
        x += draw_text_tracked(img, &fonts.sans_semibold, TS * 22.0, x, 130.0, "  ·  ", FAINT, 2.0);
        draw_text_tracked(img, &fonts.sans_semibold, TS * 22.0, x, 130.0, marker, INK, 2.0);
    }

    hline(img, MARGIN, w - MARGIN, HEADER_H - 2.0, FAINT, 2);
}

/// The relative-day marker for the deck, or `None` for days more than a day out (the weekday
/// headline already carries those).
fn relative_marker(view: NaiveDate, today: NaiveDate) -> Option<&'static str> {
    match (view - today).num_days() {
        0 => Some("TODAY"),
        1 => Some("TOMORROW"),
        -1 => Some("YESTERDAY"),
        _ => None,
    }
}

/// Height of the NOW rule row inserted between past and future items on today.
const NOW_RULE_H: f32 = 48.0 * TS;

fn draw_day(
    img: &mut RgbImage,
    hits: &mut HitMap,
    model: &AppModel,
    fonts: &Fonts,
    w: f32,
    h: f32,
    now: DateTime<Local>,
) {
    let top = HEADER_H + 20.0;
    let bottom = h - BOTTOM_BAR_H - 12.0;
    // The NOW rule only makes sense on the unscrolled first screenful of today.
    let is_today = model.view_date == model.today && model.day_scroll == 0;
    let marker_h = if is_today { NOW_RULE_H } else { 0.0 };
    let cap = (((bottom - top - marker_h) / ROW_H).floor() as usize).max(2);

    let day = match model.planner.day(model.view_date) {
        Some(d) => d,
        None => {
            draw_text(img, &fonts.sans, TS * 32.0, MARGIN, top + 60.0, "Day not fetched — tap refresh below.", MUTED);
            return;
        }
    };

    if day.events.is_empty() {
        let msg = "Nothing scheduled.";
        let mw = measure(&fonts.sans, TS * 34.0, msg);
        draw_text(img, &fonts.sans, TS * 34.0, (w - mw) / 2.0, top + 120.0, msg, MUTED);
        return;
    }

    // "… N more" paging: `day_scroll` events are skipped at the top. While scrolled, the first
    // slot is an "… N earlier" row back to the top; the overflow row at the bottom pages onward.
    let scroll = model.day_scroll.min(day.events.len().saturating_sub(1));
    let rest = &day.events[scroll..];
    let mut y = top;
    if scroll > 0 {
        let label = format!("… {scroll} earlier");
        draw_text(img, &fonts.mono, TS * 28.0, title_x(fonts), y + ROW_H / 2.0 + 10.0, &label, MUTED);
        hits.push((Rect::new(0.0, y, w, ROW_H), Target::EarlierEvents));
        y += ROW_H;
        hline(img, MARGIN, w - MARGIN, y - 6.0, FAINT, 1);
    }
    let cap = cap - usize::from(scroll > 0);
    let shown = if rest.len() > cap { cap.saturating_sub(1) } else { rest.len() };
    let mut now_drawn = !is_today;
    for e in rest.iter().take(shown) {
        if !now_drawn && e.start > now {
            y = draw_now_rule(img, fonts, w, y, now);
            now_drawn = true;
        }
        match &e.todoist_task {
            Some(task_id) => draw_task_block(img, hits, e, task_id, model, fonts, w, y, now),
            None => draw_plain_event(img, hits, e, fonts, w, y, now),
        }
        y += ROW_H;
        hline(img, MARGIN, w - MARGIN, y - 6.0, FAINT, 1);
    }
    if !now_drawn && scroll + shown == day.events.len() {
        y = draw_now_rule(img, fonts, w, y, now); // everything is already behind you
    }
    if scroll + shown < day.events.len() {
        let label = format!("… {} more", day.events.len() - scroll - shown);
        draw_text(img, &fonts.mono, TS * 28.0, title_x(fonts), y + ROW_H / 2.0 + 10.0, &label, MUTED);
        hits.push((Rect::new(0.0, y, w, ROW_H), Target::MoreEvents { skip: scroll + shown }));
    }
}

/// The shared signature with the sleep screen: a filled dot, the clock, a hairline across.
fn draw_now_rule(img: &mut RgbImage, fonts: &Fonts, w: f32, y: f32, now: DateTime<Local>) -> f32 {
    let cy = y + NOW_RULE_H / 2.0;
    disc(img, MARGIN + 12.0, cy, 10.0, INK);
    let clock = now.format("%H:%M").to_string();
    draw_text(img, &fonts.mono, TS * 28.0, MARGIN + 36.0, cy + 10.0, &clock, INK);
    let cw = measure(&fonts.mono, TS * 28.0, &clock);
    hline(img, MARGIN + 36.0 + cw + 16.0, w - MARGIN, cy, INK, 2);
    y + NOW_RULE_H
}

/// One x for every title (tasks and plain events), one x for every time — timetable columns.
fn time_x() -> f32 {
    CHECK_W
}
fn title_x(fonts: &Fonts) -> f32 {
    time_x() + measure(&fonts.mono, TS * 32.0, "00:00") + measure(&fonts.mono, TS * 24.0, "P1") + 40.0
}

/// A Diarized-scheduled task block: priority wash + saturated edge bar + checkbox + time.
fn draw_task_block(
    img: &mut RgbImage,
    hits: &mut HitMap,
    e: &Event,
    task_id: &str,
    model: &AppModel,
    fonts: &Fonts,
    w: f32,
    y: f32,
    now: DateTime<Local>,
) {
    let pri = e.priority(model.find_task(task_id)).unwrap_or(1);
    fill_rect(img, 0.0, y - 6.0, w, ROW_H, priority_tint(pri));
    fill_rect(img, 0.0, y - 6.0, 10.0, ROW_H, priority_color(pri));

    let box_sz = TS * 48.0;
    let box_x = 10.0 + (CHECK_W - 10.0 - box_sz) / 2.0;
    let box_y = y + (ROW_H - box_sz) / 2.0 - 6.0;
    checkbox(img, box_x, box_y, box_sz, INK);
    hits.push((Rect::new(0.0, y, CHECK_W, ROW_H), Target::Checkbox { task_id: task_id.to_string() }));

    let baseline = y + ROW_H / 2.0 + 10.0;
    let time = e.start.format("%H:%M").to_string();
    draw_text(img, &fonts.mono, TS * 32.0, time_x(), baseline, &time, INK);

    // Priority tag in grey ON PURPOSE, even with the vivid TINT_WASH=0.30 bands: a tag in its own
    // quadrant colour sits on a band of the same hue and disappears; grey reads on all four.
    let tag = format!("P{}", 5u8.saturating_sub(pri).max(1));
    let tag_x = time_x() + measure(&fonts.mono, TS * 32.0, "00:00") + 20.0;
    draw_text(img, &fonts.mono, TS * 24.0, tag_x, baseline, &tag, MUTED);

    let dur = format!("{}m", e.duration_min());
    let dur_w = measure(&fonts.mono, TS * 24.0, &dur);
    draw_text(img, &fonts.mono, TS * 24.0, w - MARGIN - dur_w, baseline, &dur, MUTED);

    let mut right = dur_w + 20.0;
    if model.find_task(task_id).map(|t| t.is_recurring).unwrap_or(false) {
        recur_arrows(img, w - MARGIN - right - 12.0, baseline - 10.0, 11.0, MUTED);
        right += 36.0;
    }

    let tx = title_x(fonts);
    let px = if e.is_active(now) { TS * 38.0 } else { TS * 36.0 };
    let title = truncate(&fonts.sans_semibold, px, &e.title, w - MARGIN - tx - right);
    draw_text(img, &fonts.sans_semibold, px, tx, baseline, &title, INK);
    hits.push((Rect::new(CHECK_W, y, w - CHECK_W, ROW_H), Target::TaskRow { task_id: task_id.to_string() }));
}

fn draw_plain_event(img: &mut RgbImage, hits: &mut HitMap, e: &Event, fonts: &Fonts, w: f32, y: f32, now: DateTime<Local>) {
    let baseline = y + ROW_H / 2.0 + 10.0;
    let active = e.is_active(now);
    if active {
        fill_rect(img, MARGIN, y + 8.0, 8.0, ROW_H - 20.0, INK);
    }
    let time = e.start.format("%H:%M").to_string();
    draw_text(img, &fonts.mono, TS * 30.0, time_x(), baseline, &time, INK);

    let dur = format!("{}m", e.duration_min());
    let dur_w = measure(&fonts.mono, TS * 24.0, &dur);
    draw_text(img, &fonts.mono, TS * 24.0, w - MARGIN - dur_w, baseline, &dur, MUTED);

    let tx = title_x(fonts);
    let px = if active { TS * 38.0 } else { TS * 36.0 };
    let title = truncate(&fonts.sans_semibold, px, &e.title, w - tx - dur_w - MARGIN - 16.0);
    draw_text(img, &fonts.sans_semibold, px, tx, baseline, &title, INK);
    // Tappable only with a Google id to act on (pre-v0.14 cached events get ids on the next fetch).
    if let Some(id) = &e.id {
        hits.push((
            Rect::new(0.0, y, w, ROW_H),
            Target::EventRow { calendar_id: e.calendar_id.clone(), event_id: id.clone(), title: e.title.clone() },
        ));
    }
}

/// The bottom action bar: a slim status/flash line over seven even cells —
/// ‹ prev · ● today · ↻ refresh · ⇅ reschedule · + add · ⭲ exit · next ›. Chevrons and the today dot
/// go faint + inert at their edges (window ends, already-today), mirroring the sleep screen's NOW
/// dot; the "+" opens the quick-add sheet. Everything here is a local read or a local sheet —
/// re-placing a block is Diarized's job and lives on the task card, one task at a time.
fn draw_bottom_bar(img: &mut RgbImage, hits: &mut HitMap, model: &AppModel, fonts: &Fonts, w: f32, h: f32) {
    let bar_y = h - BOTTOM_BAR_H;
    hline(img, MARGIN, w - MARGIN, bar_y, FAINT, 2);

    // Status / flash line (mono: figures + machine state), centred above the icon row.
    let text = if let Some(flash) = &model.flash {
        flash.text.clone()
    } else {
        let mut s = if model.planner.stale {
            format!("stale · fetched {}", model.planner.fetched_at.format("%H:%M"))
        } else {
            format!("updated {}", model.planner.fetched_at.format("%H:%M"))
        };
        if model.fetching {
            s.push_str(" · refreshing…");
        }
        if model.pending > 0 {
            s.push_str(&format!(" · {} queued", model.pending));
        }
        s
    };
    let ink = if model.flash.is_some() { INK } else { MUTED };
    let text = truncate(&fonts.mono, TS * 26.0, &text, w - 2.0 * MARGIN);
    let tw = measure(&fonts.mono, TS * 26.0, &text);
    draw_text(img, &fonts.mono, TS * 26.0, (w - tw) / 2.0, bar_y + 44.0, &text, ink);

    // Six even icon cells.
    let slots = even_slots(w as i32, BAR_MARGIN, ICELL_W as i32, 6);
    let cy = (bar_y + 64.0) as i32;
    let (cw, ch) = (ICELL_W as i32, ICELL_H as i32);
    let is_today = model.view_date == model.today;
    let prev_ok = model.planner.in_window(model.view_date.pred_opt().unwrap_or(model.view_date));
    let next_ok = model.planner.in_window(model.view_date.succ_opt().unwrap_or(model.view_date));

    let r = icon_btn(img, fonts, Icon::ChevronLeft, slots[0], cy, cw, ch, if prev_ok { INK } else { FAINT });
    if prev_ok {
        hits.push((r, Target::PrevDay));
    }

    // Today: a record-circle echoing the NOW-dot as a control — inert + faint when already on today.
    let r = icon_btn(img, fonts, Icon::Today, slots[1], cy, cw, ch, if is_today { FAINT } else { INK });
    if !is_today {
        hits.push((r, Target::JumpToday));
    }

    let r = icon_btn(img, fonts, Icon::Undo, slots[2], cy, cw, ch, if model.fetching { MUTED } else { INK });
    hits.push((r, Target::Refresh));

    // (The global ⇅ reschedule used to sit here, and later a per-task Recalculate on the task card.
    // Both are gone: Diarized's action routes now need a login session, so the device cannot ask it
    // to re-place anything. It re-plans on its own when Todoist tells it a task changed, which is
    // what completing or deferring here does.)

    // "+" quick-add — a plus glyph from the icon font, matching the others' size + weight.
    let r = icon_btn(img, fonts, Icon::Add, slots[3], cy, cw, ch, INK);
    hits.push((r, Target::QuickAdd));

    let r = icon_btn(img, fonts, Icon::Exit, slots[4], cy, cw, ch, INK);
    hits.push((r, Target::Exit));

    let r = icon_btn(img, fonts, Icon::ChevronRight, slots[5], cy, cw, ch, if next_ok { INK } else { FAINT });
    if next_ok {
        hits.push((r, Target::NextDay));
    }
}

/// Draw whichever overlay is open as a bottom sheet and register its hit targets. Both sheets rise
/// from the bottom with the day's events still visible above; a tap on that visible list, or the
/// sheet's Close (X), dismisses (the full-screen `DismissOverlay` is pushed in `view()`).
fn draw_overlay(
    img: &mut RgbImage,
    hits: &mut HitMap,
    model: &AppModel,
    overlay: &Overlay,
    fonts: &Fonts,
    canvas: Canvas,
) {
    match overlay {
        Overlay::TaskCard { title, can_defer, priority, .. } => {
            draw_task_sheet(img, hits, model, title, *can_defer, *priority, fonts, canvas)
        }
        Overlay::QuickAdd(qa) => draw_quickadd_sheet(img, hits, qa, fonts, canvas),
        Overlay::EventCard { title, .. } => draw_event_sheet(img, hits, title, fonts, canvas),
    }
}

/// The task action card: a filled **Complete**, quick-pick defer rows (when safe), priority chips,
/// and a Tomato **Delete**. The sheet body swallows taps (`CardBody`).
#[allow(clippy::too_many_arguments)]
fn draw_task_sheet(
    img: &mut RgbImage,
    hits: &mut HitMap,
    model: &AppModel,
    title: &str,
    can_defer: bool,
    priority: u8,
    fonts: &Fonts,
    canvas: Canvas,
) {
    let (sw, sh) = (canvas.width as i32, canvas.height as i32);
    let sheet_h = if can_defer { SHEET_H_DEFER } else { SHEET_H_NODEFER };

    let (body, close) = draw_bottom_sheet(img, fonts, title, sheet_h, sw, sh);
    hits.push((body, Target::CardBody));
    hits.push((close, Target::DismissOverlay));

    let pad = PANEL_PAD as f32;
    let bw = sw as f32 - 2.0 * pad;
    let row_h = 108.0;
    let base = model.today.max(model.view_date);
    let mut y = (body.y + SHEET_HEADER_H) as f32;

    // Complete — the primary action: a filled bar, label + check in paper.
    fill_rect(img, pad, y, bw, row_h, INK);
    let label = "Complete";
    let lw = measure(&fonts.sans_semibold, TS * 34.0, label);
    let lx = pad + (bw - lw - 52.0) / 2.0;
    draw_text(img, &fonts.sans_semibold, TS * 34.0, lx, y + row_h / 2.0 + 12.0, label, PAPER);
    icon_btn(img, fonts, Icon::Check, (lx + lw + 12.0) as i32, y as i32, 44, row_h as i32, PAPER);
    hits.push((Rect::new(pad, y, bw, row_h), Target::CardComplete));
    y += row_h + 14.0;

    if can_defer {
        for choice in [DeferChoice::Tomorrow, DeferChoice::PlusTwoDays, DeferChoice::NextWeek] {
            hline(img, pad, pad + bw, y - 7.0, FAINT, 1);
            let (word, date) = defer_label_parts(base, choice);
            let ly = y + row_h / 2.0 + 11.0;
            let mut lx = pad;
            lx += draw_text(img, &fonts.sans, TS * 32.0, lx, ly, word, INK);
            lx += draw_text(img, &fonts.sans, TS * 32.0, lx, ly, "  ·  ", FAINT);
            draw_text(img, &fonts.mono, TS * 30.0, lx, ly, &date, MUTED);
            icon_btn(img, fonts, Icon::ChevronRight, (pad + bw - 44.0) as i32, y as i32, 44, row_h as i32, FAINT);
            hits.push((Rect::new(pad, y, bw, row_h), Target::DeferChoice(choice)));
            y += row_h;
        }
    } else {
        draw_text(img, &fonts.sans, TS * 28.0, pad, y + 40.0, "Defer is off for this task", MUTED);
        draw_text(img, &fonts.sans, TS * 28.0, pad, y + 78.0, "(recurring or unlinked).", MUTED);
        y += 108.0;
    }

    // Priority chips: P1..P4, current one active — reprioritise in place.
    hline(img, pad, pad + bw, y - 7.0, FAINT, 1);
    draw_text_tracked(img, &fonts.sans_semibold, TS * 24.0, pad, y + 30.0, "PRIORITY", MUTED, 3.0);
    let chips_y = (y + 46.0) as i32;
    let chip_gap = 16;
    for (i, p) in Prio::ALL.iter().enumerate() {
        let x = PANEL_PAD + i as i32 * (prio_chip_w(sw) + chip_gap);
        let r = prio_chip(img, fonts, *p, x, chips_y, prio_chip_w(sw), p.api() == priority);
        hits.push((r, Target::CardPriority(*p)));
    }
    y += 46.0 + 96.0 + 16.0;

    // Delete — destructive, so it wears the P1 Tomato ink and sits last, away from Complete.
    hline(img, pad, pad + bw, y - 7.0, FAINT, 1);
    let dl = "Delete task";
    draw_text(img, &fonts.sans_semibold, TS * 32.0, pad, y + row_h / 2.0 + 11.0, dl, priority_color(4));
    hits.push((Rect::new(pad, y, bw, row_h), Target::CardDelete));
}

/// Chip width for a 4-across priority row inside the sheet padding.
fn prio_chip_w(sw: i32) -> i32 {
    (sw - 2 * PANEL_PAD - 3 * 16) / 4
}

/// A priority chip: the shared [`chip`] base plus the quadrant colour as a leading dot — the
/// colour survives both the active (ink) and inactive (pale) states.
fn prio_chip(img: &mut RgbImage, fonts: &Fonts, p: Prio, x: i32, y: i32, w: i32, active: bool) -> Rect {
    let r = chip(img, fonts, p.label(), x, y, w, 96, active);
    disc(img, (x + 34) as f32, (y + 48) as f32, 12.0, priority_color(p.api()));
    r
}

/// The plain-event card: the event title in the sheet header and a single destructive action —
/// Delete (in P1 Tomato, mirroring the task card's delete row). X / tap-outside dismisses.
fn draw_event_sheet(img: &mut RgbImage, hits: &mut HitMap, title: &str, fonts: &Fonts, canvas: Canvas) {
    let (sw, sh) = (canvas.width as i32, canvas.height as i32);
    let (body, close) = draw_bottom_sheet(img, fonts, title, SHEET_H_EVENT, sw, sh);
    hits.push((body, Target::CardBody));
    hits.push((close, Target::DismissOverlay));

    let pad = PANEL_PAD as f32;
    let bw = sw as f32 - 2.0 * pad;
    let row_h = 108.0;
    let y = (body.y + SHEET_HEADER_H) as f32 + 14.0;
    let dl = "Delete event";
    draw_text(img, &fonts.sans_semibold, TS * 32.0, pad, y + row_h / 2.0 + 11.0, dl, priority_color(4));
    icon_btn(img, fonts, Icon::ChevronRight, (pad + bw - 44.0) as i32, y as i32, 44, row_h as i32, FAINT);
    hits.push((Rect::new(pad, y, bw, row_h), Target::CardDeleteEvent));
}

/// The quick-add sheet: the typed line (with a caret), the due chips, and a docked on-screen
/// keyboard. The sheet body swallows stray taps; each key maps to its own [`Target`].
fn draw_quickadd_sheet(img: &mut RgbImage, hits: &mut HitMap, qa: &QuickAdd, fonts: &Fonts, canvas: Canvas) {
    let (sw, sh) = (canvas.width as i32, canvas.height as i32);
    let (body, close) = draw_bottom_sheet(img, fonts, "New task", QUICK_SHEET_H, sw, sh);
    hits.push((body, Target::CardBody));
    hits.push((close, Target::DismissOverlay));

    let pad = PANEL_PAD as f32;
    let top = (body.y + SHEET_HEADER_H) as f32;

    // The typed text with a caret, or a faint placeholder.
    let baseline = top + 56.0;
    if qa.text.is_empty() {
        draw_text(img, &fonts.sans, TS * 34.0, pad, baseline, "Type a task…", FAINT);
    } else {
        let shown = truncate(&fonts.sans_semibold, TS * 34.0, &qa.text, sw as f32 - 2.0 * pad - 20.0);
        let tw = draw_text(img, &fonts.sans_semibold, TS * 34.0, pad, baseline, &shown, INK);
        line(img, pad + tw + 4.0, baseline - 34.0, pad + tw + 4.0, baseline + 6.0, INK, 3);
    }
    hline(img, pad, sw as f32 - pad, top + 84.0, FAINT, 1);

    // Due chips: Inbox / Today / Tomorrow.
    let chips_y = (top + 104.0) as i32;
    let chip_gap = 16;
    let chip_w = (sw - 2 * PANEL_PAD - 2 * chip_gap) / 3;
    for (i, due) in DueChoice::ALL.iter().enumerate() {
        let x = PANEL_PAD + i as i32 * (chip_w + chip_gap);
        let r = chip(img, fonts, due.label(), x, chips_y, chip_w, 96, qa.due == *due);
        hits.push((r, Target::QuickAddDue(*due)));
    }

    // Priority chips: P1..P4, each wearing its quadrant colour as a dot.
    let chips2_y = chips_y + 96 + 16;
    for (i, p) in Prio::ALL.iter().enumerate() {
        let x = PANEL_PAD + i as i32 * (prio_chip_w(sw) + chip_gap);
        let r = prio_chip(img, fonts, *p, x, chips2_y, prio_chip_w(sw), qa.priority == *p);
        hits.push((r, Target::QuickAddPrio(*p)));
    }

    // Docked keyboard.
    for (r, cap) in draw_keyboard(img, fonts, sh - KB_H, sw, qa.layer, qa.shift, "Add") {
        let target = match cap {
            KeyCap::Char(c) => Target::Key(c),
            KeyCap::Space => Target::Key(' '),
            KeyCap::Backspace => Target::KbBackspace,
            KeyCap::Shift => Target::KbShift,
            KeyCap::Layer => Target::KbLayerToggle,
            KeyCap::Enter => Target::QuickAddSubmit,
        };
        hits.push((r, target));
    }
}

/// The quick-add text + due-chips strip (above the keyboard) — the partial-refresh region for
/// typing and due changes, which leave the keyboard and the list above untouched.
pub fn quickadd_input_region(canvas: Canvas) -> Rect {
    let top = canvas.height as i32 - QUICK_SHEET_H + SHEET_HEADER_H;
    let kb_top = canvas.height as i32 - KB_H;
    Rect::new(0.0, top as f32, canvas.width as f32, (kb_top - top) as f32)
}
