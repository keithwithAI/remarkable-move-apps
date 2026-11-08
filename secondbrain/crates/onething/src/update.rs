//! `(model, event) → effects`, pure. The countdown is derived (`end − now`) on every Tick — no
//! elapsed accumulation — so a suspend of any length lands on the correct remaining time, and a
//! session that expired while asleep transitions to the Done screen on the wake-up Tick.

use crate::model::{Dirty, Effect, Event, Model, Picked, Screen, Target, Timer, DEFAULT_MINUTES, PRESETS};
use crate::view::{digits_rect, hit_test, rows_per_page, HitMap};
use actionqueue::Action;
use render::ui::KbLayer;
use render::Canvas;

pub fn update(model: &mut Model, event: Event, hits: &HitMap, canvas: Canvas) -> Vec<Effect> {
    match event {
        Event::Tap { x, y } => match hit_test(hits, x, y) {
            Some((_, t)) => on_tap(model, t.clone(), canvas),
            None => Vec::new(),
        },
        Event::Tick { now } => on_tick(model, now, canvas),
        Event::TasksFetched(tasks) => {
            model.tasks = tasks;
            model.fetching = false;
            model.scroll = 0;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Event::FetchFailed(msg) => {
            model.fetching = false;
            model.flash(format!("Fetch failed: {msg}"));
            vec![Effect::Redraw(Dirty::Full)]
        }
        Event::QueueChanged(n) => {
            let changed = model.pending != n;
            model.pending = n;
            if changed { vec![Effect::Redraw(Dirty::Full)] } else { Vec::new() }
        }
        Event::UpdateChecked(msg) => {
            model.flash(msg);
            vec![Effect::Redraw(Dirty::Full)]
        }
    }
}

fn on_tick(model: &mut Model, now: chrono::DateTime<chrono::Local>, canvas: Canvas) -> Vec<Effect> {
    model.now = now;
    let mut effects = Vec::new();

    // Day rollover: a new day invites a new one thing. (Deliberately also fires when a session
    // runs across midnight — the pick belongs to the day it was made.)
    if now.date_naive() != model.date {
        model.date = now.date_naive();
        model.picked = None;
        model.done = false;
        model.timer = Timer::Idle { duration_min: DEFAULT_MINUTES };
        model.screen = Screen::Home;
        model.query.clear();
        return vec![Effect::Persist, Effect::Redraw(Dirty::Full)];
    }

    if model.flash.as_ref().is_some_and(|f| now >= f.expires_at) {
        model.flash = None;
        effects.push(Effect::Redraw(Dirty::Full));
    }

    // Finish detection — pure wall-clock: also catches sessions that expired during a suspend or
    // while the app was closed (a restored past `end`).
    if matches!(model.timer, Timer::Running { .. }) && model.timer.remaining_secs(now) == 0 {
        model.timer = Timer::Finished { duration_min: model.timer.duration_min() };
        model.screen = Screen::Done;
        effects.push(Effect::Persist);
        effects.push(Effect::Redraw(Dirty::Full));
        return effects;
    }

    // Countdown cadence — e-ink kindness: the display shows whole MINUTES for most of the session
    // (one clean Full refresh per minute tick, ~60 per hour) and live seconds only inside the
    // final minute (Region partials on the digit band, ≤59 of them, wiped by the finishing Full).
    // With a 1-hour sleep timeout, a watched session really would emit
    // ~3,600 partials into the same strip at per-second cadence — that flirts with the documented
    // localized-contrast-loss range within a few years of daily sessions.
    if model.screen == Screen::Home {
        let secs = model.timer.remaining_secs(now);
        let key = display_key(secs, model.seconds);
        if key != model.last_shown_key {
            model.last_shown_key = key;
            if matches!(model.timer, Timer::Running { .. }) {
                // Full refresh on minute ticks (the deghosting pass); Region partials for the
                // per-second updates (calm mode: only the final minute; live mode: throughout).
                let full = if model.seconds { secs % 60 == 0 } else { key > 60 };
                effects.push(if full {
                    Effect::Redraw(Dirty::Full)
                } else {
                    let (x, y, w, h) = digits_rect(canvas);
                    Effect::Redraw(Dirty::Region(x, y, w, h))
                });
            }
        }
    }
    effects
}

/// The unit of countdown display — a redraw happens exactly when this changes. Live-seconds mode:
/// the second count. Calm mode: the second count inside the final minute, else whole minutes-left
/// (ceil) offset by 1000 so the two domains can't collide.
pub fn display_key(secs: i64, live_seconds: bool) -> i64 {
    if live_seconds || secs <= 60 {
        secs
    } else {
        1000 + (secs + 59) / 60
    }
}

fn on_tap(model: &mut Model, target: Target, canvas: Canvas) -> Vec<Effect> {
    match target {
        Target::Preset(min) => {
            // Session length is only adjustable before a session starts.
            if let Timer::Idle { .. } = model.timer {
                if PRESETS.contains(&min) {
                    model.timer = Timer::Idle { duration_min: min };
                    return vec![Effect::Persist, Effect::Redraw(Dirty::Full)];
                }
            }
            Vec::new()
        }
        Target::PlayPause => {
            model.timer = match &model.timer {
                Timer::Idle { .. } | Timer::Finished { .. } => model.timer.started(model.now),
                Timer::Running { end, duration_min } => {
                    Timer::Paused { remaining_secs: (*end - model.now).num_seconds().max(0), duration_min: *duration_min }
                }
                Timer::Paused { remaining_secs, duration_min } => Timer::Running {
                    end: model.now + chrono::Duration::seconds((*remaining_secs).max(0)),
                    duration_min: *duration_min,
                },
            };
            model.screen = Screen::Home;
            vec![Effect::Persist, Effect::Redraw(Dirty::Full)]
        }
        Target::Reset => {
            model.timer = Timer::Idle { duration_min: model.timer.duration_min() };
            vec![Effect::Persist, Effect::Redraw(Dirty::Full)]
        }
        Target::OpenPicker => {
            model.screen = Screen::Picker;
            model.query.clear();
            model.layer = KbLayer::Letters;
            model.shift = false;
            model.scroll = 0;
            let mut effects = vec![Effect::Redraw(Dirty::Full)];
            if model.tasks.is_empty() && !model.fetching {
                model.fetching = true;
                effects.push(Effect::RequestFetch);
            }
            effects
        }
        Target::ToggleSeconds => {
            model.seconds = !model.seconds;
            model.flash(if model.seconds { "Live seconds on" } else { "Minutes only (kinder to the e-ink)" });
            vec![Effect::Persist, Effect::Redraw(Dirty::Full)]
        }
        Target::Back | Target::Dismiss => {
            model.screen = Screen::Home;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::Refresh => {
            if !model.fetching {
                model.fetching = true;
                return vec![Effect::RequestFetch, Effect::Redraw(Dirty::Full)];
            }
            Vec::new()
        }
        Target::Row(ix) => pick(model, ix),
        Target::Submit => {
            // Keyboard "Pick": choose the top filtered match (nothing matching → no-op).
            match model.filtered().first().copied() {
                Some(ix) => pick(model, ix),
                None => Vec::new(),
            }
        }
        Target::PagePrev => {
            let per = rows_per_page(canvas);
            model.scroll = model.scroll.saturating_sub(per);
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::PageNext => {
            let per = rows_per_page(canvas);
            if model.scroll + per < model.filtered().len() {
                model.scroll += per;
            }
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::Key(c) => {
            model.query.push(if model.shift { c.to_ascii_uppercase() } else { c });
            model.shift = false;
            model.scroll = 0;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::Backspace => {
            model.query.pop();
            model.scroll = 0;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::Shift => {
            model.shift = !model.shift;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::Layer => {
            model.layer = match model.layer {
                KbLayer::Letters => KbLayer::Symbols,
                KbLayer::Symbols => KbLayer::Letters,
            };
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::ClearQuery => {
            model.query.clear();
            model.scroll = 0;
            vec![Effect::Redraw(Dirty::Full)]
        }
        Target::MarkDone => {
            let Some(p) = model.picked.clone() else {
                return Vec::new();
            };
            model.done = true;
            model.screen = Screen::Home;
            model.flash("Done — queued for Todoist");
            vec![
                Effect::Enqueue(Action::Complete { task_id: p.id }),
                Effect::Persist,
                Effect::Redraw(Dirty::Full),
            ]
        }
        Target::GoAgain => {
            model.timer = model.timer.started(model.now);
            model.screen = Screen::Home;
            vec![Effect::Persist, Effect::Redraw(Dirty::Full)]
        }
        Target::Exit => vec![Effect::Exit],
    }
}

/// Choose `tasks[ix]` as the one thing and return Home.
fn pick(model: &mut Model, ix: usize) -> Vec<Effect> {
    let Some(t) = model.tasks.get(ix) else {
        return Vec::new();
    };
    model.picked = Some(Picked { id: t.id.clone(), title: t.title.clone() });
    model.done = false;
    model.screen = Screen::Home;
    model.query.clear();
    vec![Effect::Persist, Effect::Redraw(Dirty::Full)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use sb_core::Task;

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn at(h: u32, m: u32, s: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 14, h, m, s).unwrap()
    }
    fn task(id: &str, title: &str, priority: u8, due: Option<(u32, u32)>) -> Task {
        Task {
            id: id.into(),
            title: title.into(),
            priority,
            due: due.map(|(mo, d)| chrono::NaiveDate::from_ymd_opt(2026, mo, d).unwrap()),
            duration_min: 0,
            done: false,
            is_recurring: false,
        }
    }
    fn tick(model: &mut Model, now: chrono::DateTime<chrono::Local>) -> Vec<Effect> {
        update(model, Event::Tick { now }, &Vec::new(), CANVAS)
    }

    #[test]
    fn play_anchors_a_wall_clock_end_and_pause_resume_round_trips() {
        let t0 = at(9, 0, 0);
        let mut m = Model::new(t0);
        on_tap(&mut m, Target::PlayPause, CANVAS);
        assert_eq!(m.timer, Timer::Running { end: t0 + Duration::minutes(60), duration_min: 60 });
        // 10 minutes in, pause: 50 minutes remain.
        m.now = at(9, 10, 0);
        on_tap(&mut m, Target::PlayPause, CANVAS);
        assert_eq!(m.timer, Timer::Paused { remaining_secs: 50 * 60, duration_min: 60 });
        // Resume 5 minutes later: the end re-anchors — the pause cost no session time.
        m.now = at(9, 15, 0);
        on_tap(&mut m, Target::PlayPause, CANVAS);
        assert_eq!(m.timer, Timer::Running { end: at(10, 5, 0), duration_min: 60 });
        assert_eq!(m.timer.remaining_secs(m.now), 50 * 60);
    }

    #[test]
    fn a_suspend_gap_past_the_end_lands_on_done_in_one_tick() {
        let t0 = at(9, 0, 0);
        let mut m = Model::new(t0);
        on_tap(&mut m, Target::PlayPause, CANVAS);
        // Device slept through the end; the wake-up Tick arrives 90 minutes later.
        let eff = tick(&mut m, at(10, 30, 0));
        assert_eq!(m.timer, Timer::Finished { duration_min: 60 });
        assert_eq!(m.screen, Screen::Done);
        assert!(eff.contains(&Effect::Persist));
        assert_eq!(m.timer.remaining_secs(m.now), 0, "remaining never goes negative");
    }

    #[test]
    fn presets_only_apply_while_idle() {
        let mut m = Model::new(at(9, 0, 0));
        on_tap(&mut m, Target::Preset(25), CANVAS);
        assert_eq!(m.timer, Timer::Idle { duration_min: 25 });
        on_tap(&mut m, Target::PlayPause, CANVAS);
        on_tap(&mut m, Target::Preset(90), CANVAS);
        assert_eq!(m.timer.duration_min(), 25, "preset taps are inert mid-session");
        // Reset keeps the chosen length.
        on_tap(&mut m, Target::Reset, CANVAS);
        assert_eq!(m.timer, Timer::Idle { duration_min: 25 });
    }

    #[test]
    fn day_rollover_clears_the_pick_and_timer() {
        let mut m = Model::new(at(23, 0, 0));
        m.picked = Some(Picked { id: "1".into(), title: "Ship it".into() });
        on_tap(&mut m, Target::PlayPause, CANVAS);
        let next_day = at(23, 0, 0) + Duration::hours(2);
        let eff = tick(&mut m, next_day);
        assert_eq!(m.picked, None);
        assert_eq!(m.timer, Timer::Idle { duration_min: DEFAULT_MINUTES });
        assert_eq!(m.date, next_day.date_naive());
        assert!(eff.contains(&Effect::Persist));
    }

    #[test]
    fn calm_mode_redraws_once_per_minute_and_per_second_only_in_the_final_minute() {
        let t0 = at(9, 0, 0);
        let mut m = Model::new(t0);
        on_tap(&mut m, Target::PlayPause, CANVAS);
        m.last_shown_key = display_key(m.timer.remaining_secs(t0), false);
        // Mid-minute seconds tick over but the minutes-left display doesn't: NO redraw.
        assert!(tick(&mut m, at(9, 0, 1)).is_empty(), "calm mode: no per-second redraws");
        assert!(tick(&mut m, at(9, 0, 30)).is_empty());
        // The minutes-left display drops 60→59 → one Full (deghosting) redraw.
        let eff = tick(&mut m, at(9, 1, 1));
        assert_eq!(eff, vec![Effect::Redraw(Dirty::Full)]);
        // Inside the final minute: per-second Region partials on the digit band.
        let eff = tick(&mut m, at(9, 59, 30));
        let (x, y, w, h) = digits_rect(CANVAS);
        assert_eq!(eff, vec![Effect::Redraw(Dirty::Region(x, y, w, h))]);
    }

    #[test]
    fn live_seconds_mode_redraws_each_second_with_a_full_at_minute_boundaries() {
        let t0 = at(9, 0, 0);
        let mut m = Model::new(t0);
        m.seconds = true;
        on_tap(&mut m, Target::PlayPause, CANVAS);
        m.last_shown_key = display_key(m.timer.remaining_secs(t0), true);
        let (x, y, w, h) = digits_rect(CANVAS);
        // Every second → Region; same second twice → nothing (the 200ms poll fires ~5×/s).
        assert_eq!(tick(&mut m, at(9, 0, 1)), vec![Effect::Redraw(Dirty::Region(x, y, w, h))]);
        assert!(tick(&mut m, at(9, 0, 1)).is_empty());
        // Minute boundary (remaining divisible by 60) → Full deghost.
        assert_eq!(tick(&mut m, at(9, 1, 0)), vec![Effect::Redraw(Dirty::Full)]);
    }

    #[test]
    fn tapping_the_clock_toggles_seconds_and_persists() {
        let mut m = Model::new(at(9, 0, 0));
        assert!(!m.seconds, "calm minutes display is the default");
        let eff = on_tap(&mut m, Target::ToggleSeconds, CANVAS);
        assert!(m.seconds);
        assert!(eff.contains(&Effect::Persist));
        on_tap(&mut m, Target::ToggleSeconds, CANVAS);
        assert!(!m.seconds);
    }

    #[test]
    fn picker_filters_sorts_and_picks() {
        let mut m = Model::new(at(9, 0, 0));
        m.tasks = vec![
            task("a", "Water the plants", 1, None),
            task("b", "Write the report", 4, Some((7, 20))),
            task("c", "write thank-you note", 2, Some((7, 15))),
            task("d", "Call the bank", 2, Some((7, 14))),
        ];
        // Sort: priority desc, then due asc (None last).
        assert_eq!(m.filtered().iter().map(|&i| m.tasks[i].id.as_str()).collect::<Vec<_>>(), ["b", "d", "c", "a"]);
        // Case-insensitive substring.
        m.query = "write".into();
        assert_eq!(m.filtered().len(), 2);
        // Submit picks the top match.
        let eff = on_tap(&mut m, Target::Submit, CANVAS);
        assert_eq!(m.picked, Some(Picked { id: "b".into(), title: "Write the report".into() }));
        assert_eq!(m.screen, Screen::Home);
        assert!(eff.contains(&Effect::Persist));
    }

    #[test]
    fn mark_done_enqueues_exactly_one_complete_and_go_again_rearms() {
        let mut m = Model::new(at(9, 0, 0));
        m.picked = Some(Picked { id: "42".into(), title: "The thing".into() });
        m.timer = Timer::Finished { duration_min: 45 };
        m.screen = Screen::Done;
        let eff = on_tap(&mut m, Target::MarkDone, CANVAS);
        let enq: Vec<_> = eff.iter().filter(|e| matches!(e, Effect::Enqueue(_))).collect();
        assert_eq!(enq.len(), 1);
        assert!(matches!(&eff[0], Effect::Enqueue(Action::Complete { task_id }) if task_id == "42"));
        assert!(m.done && m.screen == Screen::Home);

        m.timer = Timer::Finished { duration_min: 45 };
        m.screen = Screen::Done;
        on_tap(&mut m, Target::GoAgain, CANVAS);
        assert_eq!(m.timer, Timer::Running { end: m.now + Duration::minutes(45), duration_min: 45 });
        assert_eq!(m.screen, Screen::Home);
    }

    #[test]
    fn opening_the_picker_with_no_tasks_requests_a_fetch() {
        let mut m = Model::new(at(9, 0, 0));
        let eff = on_tap(&mut m, Target::OpenPicker, CANVAS);
        assert!(eff.contains(&Effect::RequestFetch));
        assert!(m.fetching);
        // With tasks already cached it doesn't refetch.
        let mut m2 = Model::new(at(9, 0, 0));
        m2.tasks = vec![task("a", "X", 1, None)];
        assert!(!on_tap(&mut m2, Target::OpenPicker, CANVAS).contains(&Effect::RequestFetch));
    }
}
