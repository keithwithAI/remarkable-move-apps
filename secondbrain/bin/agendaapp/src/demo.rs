//! Sample multi-day data for `--demo` (no network, no credentials): dogfood the layout and
//! interactions in the simulator, or dump headless frames for review.
//!
//! Mirrors real Diarized output: ordinary calendar events plus Diarized-scheduled task blocks
//! (priority `color_id` + `todoist_task` link), with Todoist metadata.

use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone};
use sb_core::{Event, PlannerState, Task};

pub fn demo_planner(now: DateTime<Local>) -> PlannerState {
    let today = now.date_naive();
    let at = |d: NaiveDate, h: u32, m: u32| -> DateTime<Local> {
        Local
            .from_local_datetime(&d.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap_or(now)
    };
    // Every demo event carries a Google event id. Without one a row cannot be deleted, and a task
    // block's completion cannot queue the calendar delete that keeps it from reappearing — so an
    // id-less demo would exercise neither path.
    let ev = |d: NaiveDate, t: &str, sh: u32, sm: u32, eh: u32, em: u32| Event {
        title: t.into(),
        start: at(d, sh, sm),
        end: at(d, eh, em),
        calendar_id: "demo".into(),
        id: Some(format!("gid-{t}")),
        description: String::new(),
        color_id: None,
        todoist_task: None,
    };
    // A Diarized-scheduled task block: priority colour plus the parsed Todoist link.
    let block = |d: NaiveDate, id: &str, t: &str, color: u8, sh: u32, sm: u32, eh: u32, em: u32| Event {
        color_id: Some(color),
        todoist_task: Some(id.into()),
        ..ev(d, t, sh, sm, eh, em)
    };
    let task = |id: &str, t: &str, p: u8, due: NaiveDate, recurring: bool| Task {
        id: id.into(),
        title: t.into(),
        priority: p,
        due: Some(due),
        duration_min: 30,
        done: false,
        is_recurring: recurring,
    };

    let tomorrow = today + Duration::days(1);
    let plus3 = today + Duration::days(3);
    let events = vec![
        ev(today, "Morning pages", 7, 30, 8, 0),
        block(today, "d1", "Finish the planner event loop", 11, 8, 0, 9, 0), // P1 Tomato
        ev(today, "Team standup", 9, 30, 9, 45),
        block(today, "d2", "Draft the spike checklist", 6, 10, 0, 10, 45), // P2 Tangerine
        ev(today, "Design review — planner app", 11, 0, 12, 0),
        ev(today, "Lunch with Sam", 12, 30, 13, 30),
        block(today, "d3", "Water the plants", 5, 14, 0, 14, 15), // P3 Banana, recurring
        block(today, "d4", "Reply to the forum thread", 2, 14, 30, 15, 0), // P4 Sage
        block(today, "d5", "Mystery block from an old sync", 6, 16, 0, 16, 30), // no metadata
        ev(today, "Evening run", 18, 0, 19, 0),
        block(tomorrow, "d6", "Prep 1:1 notes", 6, 10, 0, 10, 30),
        ev(tomorrow, "1:1 with Alex", 11, 0, 11, 30),
        ev(tomorrow, "Dentist", 15, 0, 16, 0),
        ev(plus3, "Flight to Hong Kong", 18, 45, 22, 30),
    ];
    let tasks = vec![
        task("d1", "Finish the planner event loop", 4, today, false),
        task("d2", "Draft the spike checklist", 3, today, false),
        task("d3", "Water the plants", 2, today, true),
        task("d4", "Reply to the forum thread", 1, today, false),
        // d5 deliberately has no metadata (defer disabled, complete still works)
        task("d6", "Prep 1:1 notes", 3, tomorrow, false),
    ];
    PlannerState::build(
        events,
        tasks,
        (today - Duration::days(1), today + Duration::days(7)),
        today,
        now,
    )
}
