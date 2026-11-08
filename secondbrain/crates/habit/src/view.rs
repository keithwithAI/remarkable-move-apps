//! Draw the tracker's two screens to a frame + a hit map (what you tap is what you see).
//! Reading Desk language: serif masthead, 56px margins, hairline rules, even-slot icon bar.

use crate::model::{HabitModel, Screen, Target};
use crate::schedule::streak;
use chrono::{Datelike, Duration, NaiveDate};
use image::RgbImage;
use render::draw::{disc, draw_text, fill_rect, hline, measure, truncate};
use render::ui::{even_slots, icon_btn, Icon, Rect, GOLD};
use render::{Canvas, Fonts, FAINT, INK, MUTED, PAPER};

/// Completed days wear Sage (the shared P4 green from `render::priority_color`) — a filled black
/// disc read as mono-e-ink thinking; done days are momentum and deserve colour on this panel.
const SAGE: image::Rgb<u8> = image::Rgb([51, 182, 121]);

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
const ROW_H: i32 = 128;
const BAR_H: i32 = 108;
/// Heatmap cell geometry: 7 columns, Monday-first.
const CELL: i32 = 96;
const GAP: i32 = 12;

pub fn view(model: &HabitModel, fonts: &Fonts, canvas: Canvas) -> ViewOutput {
    let mut img = RgbImage::from_pixel(canvas.width, canvas.height, PAPER);
    let mut hits: HitMap = Vec::new();
    let (w, h) = (canvas.width as i32, canvas.height as i32);

    match &model.screen {
        Screen::Today => draw_today(&mut img, &mut hits, model, fonts, w, h),
        Screen::Detail { ix, month } => draw_detail(&mut img, &mut hits, model, fonts, *ix, *month, w),
    }
    draw_bottom_bar(&mut img, &mut hits, model, fonts, w, h);

    ViewOutput { frame: img, hits }
}

fn draw_today(img: &mut RgbImage, hits: &mut HitMap, model: &HabitModel, fonts: &Fonts, w: i32, h: i32) {
    // Masthead: serif title + the date line.
    draw_text(img, &fonts.serif_bold, 64.0, MARGIN as f32, 108.0, "Habits", INK);
    let date = model.today.format("%A · %-d %B").to_string();
    draw_text(img, &fonts.mono, 28.0, MARGIN as f32, 152.0, &date.to_uppercase(), MUTED);
    hline(img, MARGIN as f32, (w - MARGIN) as f32, 176.0, FAINT, 1);

    if model.habits.is_empty() {
        return draw_empty_state(img, fonts, w);
    }

    let (due, rest): (Vec<usize>, Vec<usize>) =
        (0..model.habits.len()).partition(|&i| model.habits[i].schedule.is_due(model.today));

    let mut y = 208;
    for &ix in &due {
        draw_habit_row(img, hits, model, fonts, ix, w, y, true);
        y += ROW_H;
        hline(img, MARGIN as f32, (w - MARGIN) as f32, (y - 6) as f32, FAINT, 1);
    }
    if !rest.is_empty() && y < h - BAR_H - ROW_H {
        y += 24;
        draw_text(img, &fonts.sans_semibold, 26.0, MARGIN as f32, y as f32, "NOT TODAY", FAINT);
        y += 28;
        for &ix in &rest {
            if y + ROW_H > h - BAR_H {
                break;
            }
            draw_habit_row(img, hits, model, fonts, ix, w, y, false);
            y += ROW_H;
            hline(img, MARGIN as f32, (w - MARGIN) as f32, (y - 6) as f32, FAINT, 1);
        }
    }
}

fn draw_habit_row(img: &mut RgbImage, hits: &mut HitMap, model: &HabitModel, fonts: &Fonts, ix: usize, w: i32, y: i32, due: bool) {
    let habit = &model.habits[ix];
    let cy = (y + ROW_H / 2) as f32;
    let done = habit.done.contains(&model.today);

    if due {
        // The check circle: a Sage-filled disc when done (done = momentum = colour, matching the
        // detail heatmap), a black ring otherwise.
        let cx = (MARGIN + 36) as f32;
        if done {
            disc(img, cx, cy, 34.0, SAGE);
        } else {
            disc(img, cx, cy, 34.0, INK);
            disc(img, cx, cy, 28.0, PAPER);
        }
        hits.push((Rect::new(0.0, y as f32, (MARGIN + 92) as f32, ROW_H as f32), Target::Toggle(ix)));
    }

    // Streak, right-aligned in mono (text only — no new icon glyph needed).
    let n = streak(&habit.done, &habit.schedule, model.today);
    let streak_label = if n > 0 { format!("{n} day{}", if n == 1 { "" } else { "s" }) } else { String::new() };
    let sw = if streak_label.is_empty() { 0.0 } else { measure(&fonts.mono, 28.0, &streak_label) };
    if !streak_label.is_empty() {
        // A real streak (≥2 days) glows gold — momentum gets colour; a 1-day start stays quiet.
        let ink = if n >= 2 { GOLD } else { MUTED };
        draw_text(img, &fonts.mono, 28.0, (w - MARGIN) as f32 - sw, cy + 10.0, &streak_label, ink);
    }

    let tx = (MARGIN + if due { 100 } else { 0 }) as f32;
    let ink = if due { INK } else { MUTED };
    let title = truncate(&fonts.sans_semibold, 40.0, &habit.name, (w - MARGIN) as f32 - tx - sw - 24.0);
    draw_text(img, &fonts.sans_semibold, 40.0, tx, cy + 14.0, &title, ink);
    hits.push((Rect::new(tx, y as f32, (w - MARGIN) as f32 - tx, ROW_H as f32), Target::Open(ix)));
}

fn draw_detail(img: &mut RgbImage, hits: &mut HitMap, model: &HabitModel, fonts: &Fonts, ix: usize, month: (i32, u32), w: i32) {
    let Some(habit) = model.habits.get(ix) else {
        return;
    };
    // Back chevron + masthead.
    hits.push((icon_btn(img, fonts, Icon::ChevronLeft, MARGIN, 48, 72, 72, INK), Target::Back));
    draw_text(img, &fonts.serif_bold, 56.0, (MARGIN + 96) as f32, 100.0, &truncate(&fonts.serif_bold, 56.0, &habit.name, (w - 2 * MARGIN - 96) as f32), INK);
    let n = streak(&habit.done, &habit.schedule, model.today);
    let sub = format!("{} · {} day streak", habit.schedule.describe(), n);
    draw_text(img, &fonts.mono, 28.0, (MARGIN + 96) as f32, 144.0, &sub.to_uppercase(), MUTED);
    hline(img, MARGIN as f32, (w - MARGIN) as f32, 176.0, FAINT, 1);

    // Month title row with prev/next.
    let first = NaiveDate::from_ymd_opt(month.0, month.1, 1).expect("valid month");
    let title = first.format("%B %Y").to_string();
    let tw = measure(&fonts.sans_semibold, 40.0, &title);
    draw_text(img, &fonts.sans_semibold, 40.0, (w as f32 - tw) / 2.0, 268.0, &title, INK);
    hits.push((icon_btn(img, fonts, Icon::ChevronLeft, MARGIN, 216, 84, 84, INK), Target::MonthPrev));
    hits.push((icon_btn(img, fonts, Icon::ChevronRight, w - MARGIN - 84, 216, 84, 84, INK), Target::MonthNext));

    // Weekday header (Mon-first) + the grid.
    let grid_w = 7 * CELL + 6 * GAP;
    let grid_x = (w - grid_w) / 2;
    let head_y = 356.0;
    for (i, d) in ["M", "T", "W", "T", "F", "S", "S"].iter().enumerate() {
        let cx = grid_x + i as i32 * (CELL + GAP) + CELL / 2;
        let dw = measure(&fonts.mono, 26.0, d);
        draw_text(img, &fonts.mono, 26.0, cx as f32 - dw / 2.0, head_y, d, FAINT);
    }
    let grid_top = 392;
    let leading = first.weekday().num_days_from_monday() as i32;
    let days = days_in_month(month.0, month.1);
    for d0 in 0..days {
        let date = first + Duration::days(d0 as i64);
        let col = (leading + d0) % 7;
        let row = (leading + d0) / 7;
        let x = grid_x + col * (CELL + GAP);
        let y = grid_top + row * (CELL + GAP);
        let (cx, cy) = ((x + CELL / 2) as f32, (y + CELL / 2) as f32);
        let done = habit.done.contains(&date);
        let due = habit.schedule.is_due(date);
        let past = date <= model.today;

        if date == model.today {
            // Today wears an outer ring — GOLD once done, faint until then.
            disc(img, cx, cy, 46.0, if done { GOLD } else { FAINT });
            disc(img, cx, cy, 40.0, PAPER);
        }
        let label = format!("{}", d0 + 1);
        let lw = measure(&fonts.mono, 28.0, &label);
        if done {
            disc(img, cx, cy, 34.0, SAGE);
            draw_text(img, &fonts.mono, 28.0, cx - lw / 2.0, cy + 10.0, &label, PAPER);
        } else if due && past {
            disc(img, cx, cy, 34.0, INK);
            disc(img, cx, cy, 30.0, PAPER);
            draw_text(img, &fonts.mono, 28.0, cx - lw / 2.0, cy + 10.0, &label, INK);
        } else {
            draw_text(img, &fonts.mono, 28.0, cx - lw / 2.0, cy + 10.0, &label, FAINT);
        }
    }
}

fn draw_empty_state(img: &mut RgbImage, fonts: &Fonts, w: i32) {
    let x = MARGIN as f32;
    draw_text(img, &fonts.sans, 34.0, x, 320.0, "No habit notes yet.", INK);
    draw_text(img, &fonts.sans, 30.0, x, 380.0, "Create notes in Obsidian under Clippings/Habits/ like:", MUTED);
    let template = [
        "---",
        "type: habit",
        "schedule: weekdays      # daily | weekdays | [mon, wed, fri]",
        "tags: [health]",
        "---",
        "## Log",
    ];
    let mut y = 452.0;
    fill_rect(img, x - 16.0, y - 40.0, (w - 2 * MARGIN + 32) as f32, 6.0 + 48.0 * template.len() as f32, render::ui::CHIP);
    for line in template {
        draw_text(img, &fonts.mono, 28.0, x, y, line, INK);
        y += 48.0;
    }
    draw_text(img, &fonts.sans, 30.0, x, y + 40.0, "They sync to the device within a few minutes.", MUTED);
}

fn draw_bottom_bar(img: &mut RgbImage, hits: &mut HitMap, model: &HabitModel, fonts: &Fonts, w: i32, h: i32) {
    let bar_y = h - BAR_H;
    hline(img, MARGIN as f32, (w - MARGIN) as f32, bar_y as f32, FAINT, 2);
    if let Some(f) = &model.flash {
        let fw = measure(&fonts.mono, 26.0, &f.text);
        draw_text(img, &fonts.mono, 26.0, (w as f32 - fw) / 2.0, (bar_y - 14) as f32, &f.text, MUTED);
    }
    let icell = 84;
    let slots = even_slots(w, MARGIN, icell, 2);
    let icon_y = bar_y + 14;
    hits.push((icon_btn(img, fonts, Icon::Undo, slots[0], icon_y, icell, 72, INK), Target::Refresh));
    hits.push((icon_btn(img, fonts, Icon::Exit, slots[1], icon_y, icell, 72, INK), Target::Exit));
}

fn days_in_month(year: i32, month: u32) -> i32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    (next.unwrap() - NaiveDate::from_ymd_opt(year, month, 1).unwrap()).num_days() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::HabitFile;
    use crate::model::HabitModel;
    use crate::schedule::Schedule;
    use chrono::TimeZone;
    use std::path::PathBuf;

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap() // a Friday
    }
    fn habit(name: &str, sched: Schedule) -> HabitFile {
        HabitFile {
            path: PathBuf::from(format!("/h/{name}.md")),
            name: name.into(),
            schedule: sched,
            tags: vec![],
            done: [NaiveDate::from_ymd_opt(2026, 7, 9).unwrap()].into_iter().collect(),
            source: String::new(),
        }
    }

    #[test]
    fn today_lists_due_habits_with_toggle_and_open_targets() {
        let model = HabitModel::new(
            vec![habit("Stretch", Schedule::Daily), habit("Weekend walk", Schedule::Days(vec![chrono::Weekday::Sat]))],
            now(),
        );
        let out = view(&model, &Fonts::embedded(), CANVAS);
        assert_eq!(out.frame.dimensions(), (954, 1696));
        // The daily habit (due on a Friday) gets both targets; the Saturday-only one gets Open only.
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Toggle(0)), "due habit is toggleable");
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Open(0)));
        assert!(!out.hits.iter().any(|(_, t)| *t == Target::Toggle(1)), "not-due habit has no circle");
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Open(1)), "but is still openable");
        // Finger-sized targets.
        for (r, t) in &out.hits {
            assert!(r.h >= 72, "{t:?} too short: {r:?}");
        }
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Refresh));
        assert!(out.hits.iter().any(|(_, t)| *t == Target::Exit));
    }

    #[test]
    fn detail_shows_the_month_grid_and_nav() {
        let mut model = HabitModel::new(vec![habit("Stretch", Schedule::Daily)], now());
        model.screen = Screen::Detail { ix: 0, month: (2026, 7) };
        let out = view(&model, &Fonts::embedded(), CANVAS);
        for t in [Target::Back, Target::MonthPrev, Target::MonthNext] {
            assert!(out.hits.iter().any(|(_, tt)| *tt == t), "{t:?} present");
        }
        // Substantial grid ink (31 numerals + rings + the done disc).
        let ink = out.frame.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(ink > 3000, "expected a drawn heatmap, got {ink}");
    }

    #[test]
    fn empty_dir_renders_the_template() {
        let model = HabitModel::new(vec![], now());
        let out = view(&model, &Fonts::embedded(), CANVAS);
        let ink = out.frame.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(ink > 1500, "template text drawn");
        assert!(!out.hits.iter().any(|(_, t)| matches!(t, Target::Toggle(_))), "no rows");
    }
}
