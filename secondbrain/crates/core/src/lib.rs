//! Shared types for the second-brain device.
//!
//! The [`AgendaState`] is the **neutral timeline model**: it carries both calendar events and
//! Todoist tasks from the start, so Phase 4 (Todoist) adds a data source without changing the
//! renderer (Phase 1 simply leaves `tasks` empty).

pub mod cache;
pub mod fm;
pub mod creds;

use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};

/// A calendar event (timed; all-day events are skipped upstream).
///
/// Diarized schedules Todoist tasks into the calendar as events; those carry `todoist_task`
/// (parsed from the "Open in Todoist" link in the description) and become checkable blocks in
/// the planner. `color_id` is the Google Calendar event palette id when set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub title: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub calendar_id: String,
    /// The Google Calendar event id (an *instance* id for recurring events — the fetch expands with
    /// singleEvents, so deleting removes just that occurrence). The handle a device-side delete
    /// needs; `None` on pre-v0.14 cached state until the next fetch refills it.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color_id: Option<u8>,
    /// The Todoist task this event represents (Diarized auto-scheduled blocks only). This is the
    /// **only** thing the device takes out of a Diarized block besides its colour — see
    /// `sources::gcal::diarize_todoist_id`. Nothing is ever sent to Diarized, so no host, action
    /// URL, or capability token is parsed or stored.
    #[serde(default)]
    pub todoist_task: Option<String>,
}

impl Event {
    pub fn duration_min(&self) -> i64 {
        (self.end - self.start).num_minutes().max(1)
    }
    pub fn is_active(&self, now: DateTime<Local>) -> bool {
        self.start <= now && now < self.end
    }
    /// Todoist priority (4 = P1 … 1 = P4) for an auto-scheduled task block, from the event's
    /// colour first, else from the matched task's metadata.
    ///
    /// Diarized inherited Focustask's palette unchanged — verified against 26 live blocks on
    /// 2026-08-11 and re-confirmed against Diarized's own `RANK_COLOR = {0:"11", 1:"6", 2:"5",
    /// 3:"2"}` (`adapters/gcal.py`): colorId 11 = "Highest priority", 6 = "High", 5 = "Medium",
    /// 2 = "Low".
    pub fn priority(&self, matched: Option<&Task>) -> Option<u8> {
        match self.color_id {
            Some(11) => Some(4), // Tomato — "Highest priority"
            Some(6) => Some(3),  // Tangerine — "High priority"
            Some(5) => Some(2),  // Banana — "Medium priority"
            Some(2) => Some(1),  // Sage — "Low priority" (Todoist's default, so the common case)
            _ => matched.map(|t| t.priority),
        }
    }
}

/// A Todoist task. `priority` is the **Todoist API** value (4 = p1 highest … 1 = p4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub priority: u8,
    #[serde(default)]
    pub due: Option<NaiveDate>,
    #[serde(default)]
    pub duration_min: u32,
    #[serde(default)]
    pub done: bool,
    /// Recurring per Todoist (`due.is_recurring`). Deferring by rewriting `due_date` would
    /// destroy the recurrence, so the planner UI disables defer for these.
    #[serde(default)]
    pub is_recurring: bool,
}

impl Task {
    /// Sort key: highest Todoist priority first, then earliest due date.
    pub fn sort_key(&self) -> (u8, NaiveDate) {
        (u8::MAX - self.priority, self.due.unwrap_or(NaiveDate::MAX))
    }
}

/// The full day view rendered to the suspend screen. Neutral over events + tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgendaState {
    pub date: NaiveDate,
    /// Currently-active event, if any (only meaningful for "today").
    pub now: Option<Event>,
    /// Remaining/upcoming events for the day, sorted by start.
    pub upcoming: Vec<Event>,
    /// Today's tasks (empty in Phase 1; filled in Phase 4). Sorted by priority then due.
    pub tasks: Vec<Task>,
    pub generated_at: DateTime<Local>,
    /// True when this state was served from the offline cache rather than a fresh fetch.
    #[serde(default)]
    pub stale: bool,
}

impl AgendaState {
    /// Build from raw events + tasks: sort events, split out the active one, sort tasks.
    pub fn build(mut events: Vec<Event>, mut tasks: Vec<Task>, now: DateTime<Local>) -> Self {
        events.sort_by_key(|e| e.start);
        let mut now_ev = None;
        let mut upcoming = Vec::new();
        for e in events {
            if now_ev.is_none() && e.is_active(now) {
                now_ev = Some(e);
            } else if e.end > now {
                upcoming.push(e); // drop events already fully in the past
            }
        }
        tasks.retain(|t| !t.done);
        tasks.sort_by_key(|t| t.sort_key());
        AgendaState {
            date: now.date_naive(),
            now: now_ev,
            upcoming,
            tasks,
            generated_at: now,
            stale: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.now.is_none() && self.upcoming.is_empty() && self.tasks.is_empty()
    }
}

/// One day's merged agenda inside a [`PlannerState`] window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DayAgenda {
    pub date: NaiveDate,
    /// Timed events on this date, sorted by start.
    pub events: Vec<Event>,
    /// Tasks due this date (today additionally holds overdue + undated), sorted by priority/due.
    pub tasks: Vec<Task>,
}

impl DayAgenda {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.tasks.is_empty()
    }
}

/// Multi-day model backing the interactive planner app. Every date in the fetched window has an
/// entry (possibly empty), so day paging works offline without refetching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerState {
    pub days: std::collections::BTreeMap<NaiveDate, DayAgenda>,
    /// Inclusive fetched range; paging past it needs a refetch.
    pub window: (NaiveDate, NaiveDate),
    pub fetched_at: DateTime<Local>,
    /// True when served from the offline cache rather than a fresh fetch.
    #[serde(default)]
    pub stale: bool,
}

impl PlannerState {
    /// Bucket raw events + tasks into per-day agendas over `window` (inclusive).
    ///
    /// Tasks that are overdue or undated land on `today`; done tasks and items outside the
    /// window are dropped. Events bucket by their (local) start date.
    pub fn build(
        events: Vec<Event>,
        tasks: Vec<Task>,
        window: (NaiveDate, NaiveDate),
        today: NaiveDate,
        fetched_at: DateTime<Local>,
    ) -> Self {
        let (from, to) = window;
        let mut days = std::collections::BTreeMap::new();
        let mut d = from;
        while d <= to {
            days.insert(d, DayAgenda { date: d, events: Vec::new(), tasks: Vec::new() });
            d = d.succ_opt().unwrap_or(d);
        }
        for e in events {
            let date = e.start.date_naive();
            if let Some(day) = days.get_mut(&date) {
                day.events.push(e);
            }
        }
        for t in tasks {
            if t.done {
                continue;
            }
            let date = match t.due {
                Some(due) if due > today => due,
                _ => today, // overdue or undated → today
            };
            if let Some(day) = days.get_mut(&date) {
                day.tasks.push(t);
            }
        }
        for day in days.values_mut() {
            day.events.sort_by_key(|e| e.start);
            day.tasks.sort_by_key(|t| t.sort_key());
        }
        PlannerState { days, window, fetched_at, stale: false }
    }

    pub fn day(&self, date: NaiveDate) -> Option<&DayAgenda> {
        self.days.get(&date)
    }

    pub fn in_window(&self, date: NaiveDate) -> bool {
        self.window.0 <= date && date <= self.window.1
    }

    /// Placeholder used before the first fetch/cache load completes.
    pub fn empty(today: NaiveDate, now: DateTime<Local>) -> Self {
        PlannerState::build(Vec::new(), Vec::new(), (today, today), today, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 29, h, m, 0).unwrap()
    }
    fn ev(title: &str, sh: u32, eh: u32) -> Event {
        Event {
            title: title.into(),
            start: at(sh, 0),
            end: at(eh, 0),
            calendar_id: "primary".into(),
            id: None,
            description: String::new(),
            color_id: None,
            todoist_task: None,
        }
    }

    #[test]
    fn event_priority_from_color_id_then_task() {
        let mut e = ev("block", 9, 10);
        let meta = Task { id: "t".into(), title: "t".into(), priority: 3, due: None, duration_min: 30, done: false, is_recurring: false };
        assert_eq!(e.priority(None), None);
        assert_eq!(e.priority(Some(&meta)), Some(3), "falls back to matched task");
        e.color_id = Some(11);
        assert_eq!(e.priority(Some(&meta)), Some(4), "Tomato wins as q1/P1");
        e.color_id = Some(2);
        assert_eq!(e.priority(None), Some(1), "Sage = q4/P4");
        e.color_id = Some(7); // some other calendar color — not a quadrant color
        assert_eq!(e.priority(None), None);
    }

    #[test]
    fn build_splits_active_event_and_drops_past() {
        let now = at(10, 30);
        let st = AgendaState::build(vec![ev("past", 8, 9), ev("nowmtg", 10, 11), ev("later", 14, 15)], vec![], now);
        assert_eq!(st.now.as_ref().unwrap().title, "nowmtg");
        assert_eq!(st.upcoming.len(), 1);
        assert_eq!(st.upcoming[0].title, "later");
    }

    #[test]
    fn tasks_sorted_priority_then_due() {
        let now = at(9, 0);
        let mk = |id: &str, p: u8| Task { id: id.into(), title: id.into(), priority: p, due: None, duration_min: 30, done: false, is_recurring: false };
        let st = AgendaState::build(vec![], vec![mk("low", 1), mk("high", 4), mk("mid", 3)], now);
        let order: Vec<_> = st.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, ["high", "mid", "low"]);
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, d).unwrap()
    }
    fn task_due(id: &str, due: Option<NaiveDate>) -> Task {
        Task { id: id.into(), title: id.into(), priority: 2, due, duration_min: 30, done: false, is_recurring: false }
    }

    #[test]
    fn planner_buckets_events_by_day_and_prefills_window() {
        let now = at(10, 0);
        let today = date(29);
        let st = PlannerState::build(
            vec![ev("today-mtg", 9, 10)],
            vec![],
            (date(28), date(30)),
            today,
            now,
        );
        assert_eq!(st.days.len(), 3, "every window date gets an entry");
        assert_eq!(st.day(today).unwrap().events.len(), 1);
        assert!(st.day(date(28)).unwrap().is_empty());
        let july1 = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(st.in_window(date(30)) && !st.in_window(july1));
    }

    #[test]
    fn planner_overdue_and_undated_tasks_land_on_today() {
        let now = at(10, 0);
        let today = date(29);
        let tasks = vec![
            task_due("overdue", Some(date(25))),
            task_due("undated", None),
            task_due("tomorrow", Some(date(30))),
            task_due("beyond-window", Some(NaiveDate::from_ymd_opt(2026, 7, 20).unwrap())),
            Task { done: true, ..task_due("done", Some(date(29))) },
        ];
        let st = PlannerState::build(vec![], tasks, (date(28), date(30)), today, now);
        let ids = |d: NaiveDate| -> Vec<String> {
            st.day(d).unwrap().tasks.iter().map(|t| t.id.clone()).collect()
        };
        assert_eq!(ids(today), ["overdue", "undated"]);
        assert_eq!(ids(date(30)), ["tomorrow"]);
        assert!(st.day(date(28)).unwrap().tasks.is_empty());
    }
}
