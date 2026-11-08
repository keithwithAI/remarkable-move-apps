//! `(model, event) → effects`, pure. Toggles are optimistic: the model's copy of the note is
//! edited immediately (so the circle fills on this very frame) and the runtime persists the same
//! edit; a failed write self-heals through the Rescan the runtime issues on error.

use crate::file::toggle_log;
use crate::model::{Dirty, HabitEffect, HabitEvent, HabitModel, Screen, Target};
use crate::view::{hit_test, HitMap};

pub fn update(model: &mut HabitModel, event: HabitEvent, hits: &HitMap) -> Vec<HabitEffect> {
    match event {
        HabitEvent::Tap { x, y } => match hit_test(hits, x, y) {
            Some((_, t)) => on_tap(model, t.clone()),
            None => Vec::new(),
        },
        HabitEvent::Rescanned(habits) => {
            // Keep the open detail pointing at the same habit across the re-sort (match by path).
            if let Screen::Detail { ix, .. } = &mut model.screen {
                let open = model.habits.get(*ix).map(|h| h.path.clone());
                if let Some(path) = open {
                    match habits.iter().position(|h| h.path == path) {
                        Some(new_ix) => *ix = new_ix,
                        None => model.screen = Screen::Today, // the note vanished
                    }
                }
            }
            model.habits = habits;
            vec![HabitEffect::Redraw(Dirty::Full)]
        }
        HabitEvent::Tick { now } => {
            let mut effects = Vec::new();
            model.now = now;
            if model.flash.as_ref().is_some_and(|f| now >= f.expires_at) {
                model.flash = None;
                effects.push(HabitEffect::Redraw(Dirty::Full));
            }
            let today = now.date_naive();
            if today != model.today {
                // Midnight rollover: due-ness, streaks, and the heatmap's "today" ring all move.
                model.today = today;
                effects.push(HabitEffect::Redraw(Dirty::Full));
            }
            effects
        }
    }
}

fn on_tap(model: &mut HabitModel, target: Target) -> Vec<HabitEffect> {
    match target {
        Target::Toggle(ix) => {
            let today = model.today;
            let Some(h) = model.habits.get_mut(ix) else {
                return Vec::new();
            };
            let on = !h.done.contains(&today);
            // Optimistic: flip the model's copy now; the runtime persists the identical edit.
            h.source = toggle_log(&h.source, today, on);
            if on {
                h.done.insert(today);
            } else {
                h.done.remove(&today);
            }
            let path = h.path.clone();
            vec![
                HabitEffect::WriteToggle { path, date: today, on },
                HabitEffect::Sync,
                HabitEffect::Redraw(Dirty::Full),
            ]
        }
        Target::Open(ix) => {
            if ix < model.habits.len() {
                model.screen = Screen::Detail { ix, month: model.this_month() };
                return vec![HabitEffect::Redraw(Dirty::Full)];
            }
            Vec::new()
        }
        Target::Back => {
            model.screen = Screen::Today;
            vec![HabitEffect::Redraw(Dirty::Full)]
        }
        Target::MonthPrev | Target::MonthNext => {
            let delta: i32 = if target == Target::MonthNext { 1 } else { -1 };
            if let Screen::Detail { month, .. } = &mut model.screen {
                let (mut y, mut m) = (month.0, month.1 as i32 + delta);
                if m < 1 {
                    y -= 1;
                    m = 12;
                } else if m > 12 {
                    y += 1;
                    m = 1;
                }
                *month = (y, m as u32);
                return vec![HabitEffect::Redraw(Dirty::Full)];
            }
            Vec::new()
        }
        Target::Refresh => {
            model.flash("Refreshing…");
            vec![HabitEffect::Rescan, HabitEffect::Sync, HabitEffect::Redraw(Dirty::Full)]
        }
        Target::Exit => vec![HabitEffect::Exit],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::HabitFile;
    use crate::schedule::Schedule;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap()
    }
    fn habit(name: &str) -> HabitFile {
        let source = format!("---\nschedule: daily\n---\n\n## Log\n");
        HabitFile {
            path: PathBuf::from(format!("/h/{name}.md")),
            name: name.into(),
            schedule: Schedule::Daily,
            tags: vec![],
            done: Default::default(),
            source,
        }
    }

    #[test]
    fn toggle_is_optimistic_and_emits_write_plus_sync() {
        let mut model = HabitModel::new(vec![habit("Stretch")], now());
        let eff = on_tap(&mut model, Target::Toggle(0));
        assert!(model.habits[0].done.contains(&model.today), "circle fills immediately");
        assert!(model.habits[0].source.contains("- [x] 2026-07-10"), "model source edited too");
        assert!(matches!(&eff[0], HabitEffect::WriteToggle { date, on: true, .. } if *date == model.today));
        assert!(eff.contains(&HabitEffect::Sync));
        // Toggling again turns it off.
        let eff = on_tap(&mut model, Target::Toggle(0));
        assert!(!model.habits[0].done.contains(&model.today));
        assert!(!model.habits[0].source.contains("- [x] 2026-07-10"));
        assert!(matches!(&eff[0], HabitEffect::WriteToggle { on: false, .. }));
    }

    #[test]
    fn month_navigation_wraps_across_years() {
        let mut model = HabitModel::new(vec![habit("A")], now());
        model.screen = Screen::Detail { ix: 0, month: (2026, 1) };
        on_tap(&mut model, Target::MonthPrev);
        assert_eq!(model.screen, Screen::Detail { ix: 0, month: (2025, 12) });
        on_tap(&mut model, Target::MonthNext);
        assert_eq!(model.screen, Screen::Detail { ix: 0, month: (2026, 1) });
    }

    #[test]
    fn rescan_keeps_the_open_detail_by_path() {
        let mut model = HabitModel::new(vec![habit("A"), habit("B")], now());
        model.screen = Screen::Detail { ix: 1, month: (2026, 7) };
        // A rescan that re-sorts (B first now).
        let rescanned = vec![habit("B"), habit("A")];
        update(&mut model, HabitEvent::Rescanned(rescanned), &Vec::new());
        assert_eq!(model.screen, Screen::Detail { ix: 0, month: (2026, 7) }, "followed B across the re-sort");
        // A rescan that drops the open habit falls back to Today.
        update(&mut model, HabitEvent::Rescanned(vec![habit("A")]), &Vec::new());
        assert_eq!(model.screen, Screen::Today);
    }

    #[test]
    fn midnight_rollover_redraws() {
        let mut model = HabitModel::new(vec![habit("A")], now());
        let tomorrow = chrono::Local.with_ymd_and_hms(2026, 7, 11, 0, 1, 0).unwrap();
        let eff = update(&mut model, HabitEvent::Tick { now: tomorrow }, &Vec::new());
        assert_eq!(model.today, tomorrow.date_naive());
        assert!(eff.contains(&HabitEffect::Redraw(Dirty::Full)));
    }
}
