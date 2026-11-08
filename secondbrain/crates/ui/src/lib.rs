//! Pure planner-app logic (Elm-style): [`AppModel`] + [`update`] + [`view`].
//!
//! No I/O lives here — the same code drives the device (qtfb), the Mac simulator (minifb),
//! and headless snapshot tests. The [`view::HitMap`] is produced by layout itself, so what you
//! tap is exactly what you see.

pub mod defer;
pub mod model;
pub mod update;
pub mod view;

pub use defer::{defer_label, defer_target, DeferChoice};
pub use model::{AppModel, Dirty, DueChoice, Effect, Flash, Overlay, Prio, QuickAdd, UiEvent};
pub use update::update;
pub use view::{bottom_bar_rect, hit_test, view, HitMap, Rect, Target, ViewOutput};

#[cfg(test)]
mod tests {
    use super::*;
    use actionqueue::Action;
    use chrono::{DateTime, Local, NaiveDate, TimeZone};
    use render::{Canvas, Fonts};
    use sb_core::{Event, PlannerState, Task};

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap() // a Thursday
    }
    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }
    fn task_meta(id: &str, p: u8, due: NaiveDate, recurring: bool) -> Task {
        Task { id: id.into(), title: id.into(), priority: p, due: Some(due), duration_min: 30, done: false, is_recurring: recurring }
    }
    fn plain_event(title: &str, day: u32, h: u32) -> Event {
        let start = Local.with_ymd_and_hms(2026, 7, day, h, 0, 0).unwrap();
        Event { title: title.into(), start, end: start + chrono::Duration::hours(1), calendar_id: "primary".into(), id: Some(format!("gid-{title}")), description: String::new(), color_id: None, todoist_task: None }
    }
    fn task_block(todoist_id: &str, title: &str, color_id: u8, day: u32, h: u32) -> Event {
        Event {
            color_id: Some(color_id),
            todoist_task: Some(todoist_id.into()),
            ..plain_event(title, day, h)
        }
    }

    /// Today (2 Jul): P1 block 08:00, standup 09:00, recurring P3 block 11:00, orphan block
    /// 14:00 (no Todoist metadata), lunch 13:00. Tomorrow: one block + one event.
    fn sample_model() -> AppModel {
        let events = vec![
            task_block("t-high", "Ship the planner", 11, 2, 8),
            plain_event("Standup", 2, 9),
            task_block("t-rec", "Water the plants", 5, 2, 11),
            plain_event("Lunch with Sam", 2, 13),
            task_block("t-orphan", "Mystery block", 6, 2, 14),
            task_block("t-tomorrow", "Prep meeting", 6, 3, 10),
            plain_event("1:1 with Alex", 3, 15),
        ];
        let tasks = vec![
            task_meta("t-high", 4, d(7, 2), false),
            task_meta("t-rec", 2, d(7, 2), true),
            task_meta("t-tomorrow", 3, d(7, 3), false),
            // t-orphan deliberately has no metadata
        ];
        let planner = PlannerState::build(events, tasks, (d(7, 1), d(7, 9)), d(7, 2), now());
        AppModel::new(planner, now())
    }

    fn render_hits(model: &AppModel) -> HitMap {
        view(model, &Fonts::embedded(), CANVAS, model.now).hits
    }
    fn find_target<'a>(hits: &'a HitMap, pred: impl Fn(&Target) -> bool) -> &'a (Rect, Target) {
        hits.iter().find(|(_, t)| pred(t)).expect("target present")
    }
    fn tap_on(model: &mut AppModel, hits: &HitMap, pred: impl Fn(&Target) -> bool) -> Vec<Effect> {
        let (r, _) = find_target(hits, pred);
        let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
        update(model, UiEvent::Tap { x: cx, y: cy }, hits, CANVAS)
    }
    /// The queued actions, in order — order matters where one write is the source of truth for
    /// another (Todoist close before the calendar delete that mirrors it).
    fn enqueued(effects: &[Effect]) -> Vec<Action> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Enqueue(a) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tapping_a_plain_event_opens_the_event_card_and_delete_enqueues() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        // The plain event row is tappable (it has a Google id in the fixture)…
        let eff = tap_on(&mut model, &hits, |t| matches!(t, Target::EventRow { title, .. } if title == "Standup"));
        assert!(matches!(&model.overlay, Some(Overlay::EventCard { event_id, .. }) if event_id == "gid-Standup"), "event card opens");
        assert_eq!(eff.len(), 1, "sheet redraw only");
        // …its sheet has exactly one action: Delete.
        let hits = render_hits(&model);
        let eff = tap_on(&mut model, &hits, |t| *t == Target::CardDeleteEvent);
        assert!(model.overlay.is_none(), "sheet closes");
        assert_eq!(model.pending, 1, "queued badge ticks");
        assert!(
            eff.iter().any(|e| matches!(e, Effect::Enqueue(Action::DeleteEvent { calendar_id, event_id }) if calendar_id == "primary" && event_id == "gid-Standup")),
            "delete enqueued, got {eff:?}"
        );
        // Optimistically gone from every day + no longer tappable.
        assert!(model.planner.days.values().all(|d| d.events.iter().all(|e| e.id.as_deref() != Some("gid-Standup"))));
        assert!(!render_hits(&model).iter().any(|(_, t)| matches!(t, Target::EventRow { event_id, .. } if event_id == "gid-Standup")));
    }

    #[test]
    fn task_blocks_still_open_the_task_card_not_the_event_card() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        assert!(
            !hits.iter().any(|(_, t)| matches!(t, Target::EventRow { title, .. } if title == "Ship the planner")),
            "task blocks never get an event row target"
        );
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        assert!(matches!(&model.overlay, Some(Overlay::TaskCard { .. })), "task card, not event card");
    }

    #[test]
    fn an_event_without_an_id_is_not_tappable() {
        let mut model = sample_model();
        // Simulate a pre-v0.14 cached event: strip the id.
        for day in model.planner.days.values_mut() {
            for e in day.events.iter_mut().filter(|e| e.title == "Standup") {
                e.id = None;
            }
        }
        let hits = render_hits(&model);
        assert!(!hits.iter().any(|(_, t)| matches!(t, Target::EventRow { title, .. } if title == "Standup")));
    }

    #[test]
    fn more_events_pages_the_day_and_earlier_returns() {
        let mut model = sample_model();
        // Pack today with enough extra events to overflow the row capacity.
        {
            let day = model.planner.days.get_mut(&d(7, 2)).unwrap();
            for i in 0..20 {
                day.events.push(plain_event(&format!("Extra {i:02}"), 2, 15));
            }
            day.events.sort_by_key(|e| e.start);
        }
        let hits = render_hits(&model);
        let (_, more) = find_target(&hits, |t| matches!(t, Target::MoreEvents { .. }));
        let Target::MoreEvents { skip } = more else { unreachable!() };
        let first_page = *skip;
        assert!(first_page > 0, "a full first page is shown");
        // Page down: the scroll advances and an "earlier" row appears.
        tap_on(&mut model, &hits, |t| matches!(t, Target::MoreEvents { .. }));
        assert_eq!(model.day_scroll, first_page);
        let hits = render_hits(&model);
        assert!(hits.iter().any(|(_, t)| *t == Target::EarlierEvents), "back-to-top row present");
        // Hidden events are now visible AND tappable (deletable).
        assert!(hits.iter().any(|(_, t)| matches!(t, Target::EventRow { title, .. } if title.starts_with("Extra"))));
        // Back to the top.
        tap_on(&mut model, &hits, |t| *t == Target::EarlierEvents);
        assert_eq!(model.day_scroll, 0);
        // Day navigation resets the scroll.
        model.day_scroll = 3;
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| *t == Target::NextDay);
        assert_eq!(model.day_scroll, 0, "paging to another day starts at its top");
    }

    #[test]
    fn view_renders_merged_list_with_checkable_blocks_only() {
        let model = sample_model();
        let out = view(&model, &Fonts::embedded(), CANVAS, model.now);
        assert_eq!(out.frame.dimensions(), (954, 1696));
        let inked = out.frame.pixels().filter(|p| (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384).count();
        assert!(inked > 2000, "expected substantial content, got {inked}");
        // exactly the three Diarized blocks today are checkable — plain events are not
        let boxes: Vec<_> = out
            .hits
            .iter()
            .filter_map(|(_, t)| match t {
                Target::Checkbox { task_id } => Some(task_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(boxes, ["t-high", "t-rec", "t-orphan"], "chronological, tasks only");
        // priority tint actually painted (colored pixels exist)
        let colored = out.frame.pixels().filter(|p| p[0] != p[1] || p[1] != p[2]).count();
        assert!(colored > 5000, "expected tinted rows, got {colored}");
    }

    #[test]
    fn tap_targets_are_finger_sized_and_disjoint_per_layer() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        assert_hit_invariants(&hits);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        assert_hit_invariants(&hits);
    }

    fn assert_hit_invariants(hits: &HitMap) {
        for (r, t) in hits.iter().filter(|(_, t)| t.is_interactive()) {
            assert!(r.w >= t.min_tap() && r.h >= t.min_tap(), "{t:?} too small: {r:?}");
        }
        let layer_start = hits
            .iter()
            .rposition(|(_, t)| matches!(t, Target::DismissOverlay) && !t.is_interactive())
            .map(|i| i + 1)
            .unwrap_or(0);
        let layer: Vec<_> = hits[layer_start..].iter().filter(|(_, t)| t.is_interactive()).collect();
        for (i, (ra, ta)) in layer.iter().enumerate() {
            for (rb, tb) in layer.iter().skip(i + 1) {
                assert!(!ra.intersects(rb), "{ta:?} overlaps {tb:?}");
            }
        }
    }

    #[test]
    fn checkbox_tap_completes_and_hides_the_block() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::Checkbox { task_id } if task_id == "t-high"));
        assert!(effects.contains(&Effect::Enqueue(Action::Complete { task_id: "t-high".into() })));
        assert!(model.find_block("t-high").is_none(), "block hidden optimistically");
        assert!(model.find_task("t-high").is_none(), "metadata gone too");
        assert_eq!(model.pending, 2, "the Todoist close plus the block's calendar delete");
        assert!(model.flash.is_some());
    }

    #[test]
    fn card_complete_button_works() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        assert!(matches!(model.overlay, Some(Overlay::TaskCard { ref task_id, can_defer: true, .. }) if task_id == "t-high"));
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::CardComplete));
        assert!(effects.contains(&Effect::Enqueue(Action::Complete { task_id: "t-high".into() })));
        assert!(model.overlay.is_none());
        assert!(model.find_block("t-high").is_none());
    }

    #[test]
    fn card_delete_button_removes_the_task_entirely() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::CardDelete));
        assert!(effects.contains(&Effect::Enqueue(Action::Delete { task_id: "t-high".into() })));
        assert!(model.overlay.is_none());
        assert!(model.find_block("t-high").is_none(), "block gone");
        assert!(model.find_task("t-high").is_none(), "metadata gone too");
        assert_eq!(model.pending, 2, "the Todoist delete plus the block's calendar delete");
        assert!(model.flash.as_ref().unwrap().text.contains("Deleted"));
    }

    #[test]
    fn defer_keeps_the_block_but_moves_metadata() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::DeferChoice(DeferChoice::NextWeek)));
        // 2026-07-02 is a Thursday → next Monday is the 6th.
        assert!(effects.contains(&Effect::Enqueue(Action::Defer { task_id: "t-high".into(), due: d(7, 6) })));
        assert!(model.overlay.is_none());
        assert!(model.find_block("t-high").is_some(), "block STAYS — Diarized moves it");
        assert_eq!(model.find_task("t-high").unwrap().due, Some(d(7, 6)), "metadata moved");
        assert!(model.flash.as_ref().unwrap().text.contains("Diarized"));
    }

    #[test]
    fn recurring_and_orphan_blocks_get_no_defer_buttons() {
        for id in ["t-rec", "t-orphan"] {
            let mut model = sample_model();
            let hits = render_hits(&model);
            tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == id));
            assert!(
                matches!(model.overlay, Some(Overlay::TaskCard { can_defer: false, .. })),
                "{id}: defer must be disabled"
            );
            let hits = render_hits(&model);
            assert!(!hits.iter().any(|(_, t)| matches!(t, Target::DeferChoice(_))), "{id}");
            // Complete and Delete are still offered (both are always safe).
            assert!(hits.iter().any(|(_, t)| matches!(t, Target::CardComplete)), "{id}");
            assert!(hits.iter().any(|(_, t)| matches!(t, Target::CardDelete)), "{id}: delete present");
        }
    }

    #[test]
    fn overlay_is_modal_and_dismisses_outside_card() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        let before = model.view_date;
        // The action card is a bottom sheet; "outside" it is the day list above (t-high shows the
        // taller defer sheet, whose top is well below y=400).
        let effects = update(&mut model, UiEvent::Tap { x: 500, y: 400 }, &hits, CANVAS);
        assert_eq!(model.view_date, before, "overlay must swallow day-layer taps");
        assert!(model.overlay.is_none(), "outside tap dismisses");
        assert!(effects.iter().any(|e| matches!(e, Effect::Redraw(_))));
    }

    #[test]
    fn paging_moves_within_window_and_stops_at_edges() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::NextDay));
        assert_eq!(model.view_date, d(7, 3));

        model.view_date = d(7, 9);
        let hits = render_hits(&model);
        assert!(!hits.iter().any(|(_, t)| matches!(t, Target::NextDay)), "edge arrow is dead");
        model.view_date = d(7, 1);
        let hits = render_hits(&model);
        assert!(!hits.iter().any(|(_, t)| matches!(t, Target::PrevDay)));

        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::JumpToday));
        assert_eq!(model.view_date, model.today);
        assert!(effects.contains(&Effect::Redraw(Dirty::Full)));
    }

    #[test]
    fn refresh_tap_requests_fetch_once() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::Refresh));
        assert!(effects.contains(&Effect::RequestFetch));
        assert!(model.fetching);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::Refresh));
        assert!(effects.is_empty());
    }

    #[test]
    fn every_task_card_row_fits_on_canvas() {
        // The sheet shrank when Recalculate went; make sure nothing (least of all the destructive
        // Delete) hangs off the bottom of the panel where it can't be tapped.
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        for (r, t) in hits.iter().filter(|(_, t)| matches!(t,
            Target::CardComplete | Target::CardDelete
                | Target::DeferChoice(_) | Target::CardPriority(_)))
        {
            assert!(r.y >= 0 && r.y + r.h <= CANVAS.height as i32, "{t:?} off-canvas: {r:?}");
            assert!(r.h >= Target::CardComplete.min_tap(), "{t:?} too short to tap: {r:?}");
        }
    }

    /// Completing must also queue the calendar delete for the task's block. Without it the Todoist
    /// close lands in seconds, the queue empties, `apply_overlay` stops masking the block, and the
    /// next fetch brings it back to sit on the day until Diarized's next sync notices.
    #[test]
    fn completing_also_deletes_the_calendar_block() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        let effects =
            tap_on(&mut model, &hits, |t| matches!(t, Target::Checkbox { task_id } if task_id == "t-high"));
        assert_eq!(
            enqueued(&effects),
            vec![
                // Todoist first: the source of truth before the view of it.
                Action::Complete { task_id: "t-high".into() },
                Action::DeleteEvent {
                    calendar_id: "primary".into(),
                    event_id: "gid-Ship the planner".into()
                },
            ]
        );
        assert!(model.find_block("t-high").is_none());
        assert_eq!(model.pending, 2, "both writes counted");
    }

    #[test]
    fn deleting_a_task_also_deletes_its_calendar_block() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { task_id } if task_id == "t-high"));
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::CardDelete));
        assert_eq!(
            enqueued(&effects),
            vec![
                Action::Delete { task_id: "t-high".into() },
                Action::DeleteEvent {
                    calendar_id: "primary".into(),
                    event_id: "gid-Ship the planner".into()
                },
            ]
        );
    }

    /// A block whose Google event id never made it into the cache (pre-v0.14 state) must still
    /// complete — there is simply no delete to address, and inventing one would park forever.
    #[test]
    fn a_block_without_a_google_event_id_still_completes() {
        let mut model = sample_model();
        for day in model.planner.days.values_mut() {
            for e in day.events.iter_mut() {
                e.id = None;
            }
        }
        let hits = render_hits(&model);
        let effects =
            tap_on(&mut model, &hits, |t| matches!(t, Target::Checkbox { task_id } if task_id == "t-high"));
        assert_eq!(enqueued(&effects), vec![Action::Complete { task_id: "t-high".into() }]);
        assert_eq!(model.pending, 1, "nothing counted for a delete we can't address");
    }

    #[test]
    fn bottom_bar_keeps_all_six_controls_reachable() {
        // Guard that dropping the reschedule cell didn't push anything off-canvas or let two
        // controls overlap once `even_slots` re-spaced the row.
        let mut model = sample_model();
        model.view_date = d(7, 3); // mid-window so both chevrons are live
        let hits = render_hits(&model);
        for want in [
            Target::PrevDay, Target::JumpToday, Target::Refresh,
            Target::QuickAdd, Target::Exit, Target::NextDay,
        ] {
            let r = hits.iter().find(|(_, t)| *t == want).map(|(r, _)| *r);
            let r = r.unwrap_or_else(|| panic!("{want:?} missing from the bottom bar"));
            assert!(r.x >= 0 && r.x + r.w <= CANVAS.width as i32, "{want:?} off-canvas: {r:?}");
        }
        let bar: Vec<_> = hits
            .iter()
            .filter(|(_, t)| {
                matches!(t, Target::PrevDay | Target::JumpToday | Target::Refresh
                    | Target::QuickAdd | Target::Exit | Target::NextDay)
            })
            .map(|(r, _)| *r)
            .collect();
        for (i, a) in bar.iter().enumerate() {
            for b in &bar[i + 1..] {
                let overlap = a.x < b.x + b.w && b.x < a.x + a.w;
                assert!(!overlap, "bottom-bar cells overlap: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn exit_button_quits_the_app() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        assert!(hits.iter().any(|(_, t)| matches!(t, Target::Exit)), "exit control present");
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::Exit));
        assert_eq!(effects, vec![Effect::Exit]);
    }

    #[test]
    fn data_refresh_clamps_view_date_into_new_window() {
        let mut model = sample_model();
        model.view_date = d(7, 9);
        let fresh = PlannerState::build(vec![], vec![], (d(7, 2), d(7, 5)), d(7, 2), now());
        update(&mut model, UiEvent::DataRefreshed(fresh), &HitMap::new(), CANVAS);
        assert_eq!(model.view_date, d(7, 5));
        assert!(!model.fetching);
    }

    #[test]
    fn tick_expires_flash_and_rolls_midnight() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::Checkbox { .. }));
        assert!(model.flash.is_some());
        update(&mut model, UiEvent::Tick { now: now() + chrono::Duration::seconds(6) }, &hits, CANVAS);
        assert!(model.flash.is_none(), "flash expires");

        let effects = update(
            &mut model,
            UiEvent::Tick { now: Local.with_ymd_and_hms(2026, 7, 3, 0, 1, 0).unwrap() },
            &hits,
            CANVAS,
        );
        assert_eq!(model.today, d(7, 3));
        assert_eq!(model.view_date, d(7, 3));
        assert!(effects.contains(&Effect::RequestFetch), "midnight rollover refetches");
    }

    #[test]
    fn day_paging_renders_different_rows() {
        let model = sample_model();
        let fonts = Fonts::embedded();
        let today_frame = view(&model, &fonts, CANVAS, model.now).frame;
        let mut tomorrow = sample_model();
        tomorrow.view_date = d(7, 3);
        let tomorrow_frame = view(&tomorrow, &fonts, CANVAS, tomorrow.now).frame;
        assert_ne!(today_frame.as_raw(), tomorrow_frame.as_raw());
    }

    #[test]
    fn overflow_folds_into_more_row() {
        let events: Vec<Event> = (0..25)
            .map(|i| task_block(&format!("t{i}"), &format!("Task {i}"), 11, 2, 0)) // all at 00:xx
            .collect();
        let planner = PlannerState::build(events, vec![], (d(7, 1), d(7, 9)), d(7, 2), now());
        let model = AppModel::new(planner, now());
        let hits = render_hits(&model);
        let shown = hits.iter().filter(|(_, t)| matches!(t, Target::Checkbox { .. })).count();
        assert!(shown < 25, "must truncate: {shown}");
        // Fewer rows fill a page now that the type scale (TS ~1.15×) makes each row taller (~11 fit).
        assert!(shown >= 6, "should still fill the page: {shown}");
    }

    #[test]
    fn quickadd_opens_types_picks_due_and_submits() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        // open the quick-add sheet from the bottom-bar "+"
        tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAdd));
        assert!(matches!(model.overlay, Some(Overlay::QuickAdd(_))), "quick-add sheet open");
        let hits = render_hits(&model);
        assert_hit_invariants(&hits); // the keyboard keys clear MIN_KEY_PX and don't overlap

        // type "hi"
        for ch in ['h', 'i'] {
            let hits = render_hits(&model);
            tap_on(&mut model, &hits, |t| matches!(t, Target::Key(c) if *c == ch));
        }
        match &model.overlay {
            Some(Overlay::QuickAdd(qa)) => assert_eq!(qa.text, "hi"),
            _ => panic!("sheet should still be open with typed text"),
        }

        // pick Tomorrow + P1, then submit via the keyboard's Add key
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAddDue(DueChoice::Tomorrow)));
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAddPrio(Prio::P1)));
        match &model.overlay {
            Some(Overlay::QuickAdd(qa)) => assert_eq!(qa.priority, Prio::P1),
            _ => panic!("sheet should still be open with P1 picked"),
        }
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAddSubmit));
        assert!(model.overlay.is_none(), "submit closes the sheet");
        assert!(model.flash.as_ref().unwrap().text.contains("Added: hi"));
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::Enqueue(Action::Create { content, due, priority, .. })
                    if content == "hi" && due.as_deref() == Some("tomorrow")
                        && *priority == Some(4))),
            "enqueues a Todoist create carrying the chosen due + P1 (raw 4)"
        );
    }

    #[test]
    fn quickadd_default_priority_is_omitted_from_the_create() {
        // P4 is Todoist's default — the create body should stay minimal when it's unchanged.
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAdd));
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::Key('x')));
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAddSubmit));
        assert!(effects.iter().any(|e| matches!(e,
            Effect::Enqueue(Action::Create { priority: None, .. }))));
    }

    #[test]
    fn task_card_reprioritises_in_place() {
        let mut model = sample_model();
        // Open a task's action card.
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::TaskRow { .. }));
        let (task_id, before) = match &model.overlay {
            Some(Overlay::TaskCard { task_id, priority, .. }) => (task_id.clone(), *priority),
            other => panic!("expected a task card, got {other:?}"),
        };
        // Pick a DIFFERENT priority so the retint is observable.
        let pick = if before == 4 { Prio::P3 } else { Prio::P1 };
        let hits = render_hits(&model);
        let effects = tap_on(&mut model, &hits, |t| matches!(t, Target::CardPriority(p) if *p == pick));
        assert!(model.overlay.is_none(), "reprioritise closes the card");
        assert!(model.flash.as_ref().unwrap().text.contains("Priority"));
        assert!(
            effects.iter().any(|e| matches!(e,
                Effect::Enqueue(Action::SetPriority { task_id: t, priority })
                    if *t == task_id && *priority == pick.api())),
            "enqueues the priority rewrite"
        );
        // Optimistic retint landed in the model's task metadata.
        assert_eq!(model.find_task(&task_id).map(|t| t.priority), Some(pick.api()));
    }

    #[test]
    fn quickadd_shift_capitalises_then_reverts() {
        let mut model = sample_model();
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::QuickAdd));
        // shift on, then a letter → uppercase, and shift auto-releases (one-shot)
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::KbShift));
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::Key('H')));
        let hits = render_hits(&model);
        tap_on(&mut model, &hits, |t| matches!(t, Target::Key('i')));
        match &model.overlay {
            Some(Overlay::QuickAdd(qa)) => {
                assert_eq!(qa.text, "Hi", "one-shot shift capitalises only the next letter");
                assert!(!qa.shift);
            }
            _ => panic!("open"),
        }
    }
}
