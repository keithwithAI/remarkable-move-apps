//! The persisted snapshot: `{date, picked, done, timer}` → `cache/onething.json`.
//!
//! Saved (by the runtime) on every state transition, so a battery death or app restart resumes
//! the countdown exactly — a restored `Running{end}` already in the past lands on the Done screen
//! at the first Tick. A snapshot from a previous day is discarded on load (new day, new one thing).

use crate::model::{Model, Picked, Timer};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub date: NaiveDate,
    pub picked: Option<Picked>,
    #[serde(default)]
    pub done: bool,
    pub timer: Timer,
    /// Live-seconds display preference — survives day rollover (it's a setting, not day state).
    #[serde(default)]
    pub seconds: bool,
}

/// What of `model` survives a restart.
pub fn snapshot(model: &Model) -> Snapshot {
    Snapshot {
        date: model.date,
        picked: model.picked.clone(),
        done: model.done,
        timer: model.timer.clone(),
        seconds: model.seconds,
    }
}

/// Seed a fresh model from a loaded snapshot (or none). A stale (previous-day) snapshot resets to
/// an unpicked, idle today — the same rule `update` applies at midnight.
pub fn seed(snapshot: Option<Snapshot>, now: DateTime<Local>) -> Model {
    let mut model = Model::new(now);
    if let Some(s) = snapshot {
        model.seconds = s.seconds; // a preference — kept even when the day state is stale
        if s.date == now.date_naive() {
            model.date = s.date;
            model.picked = s.picked;
            model.done = s.done;
            model.timer = s.timer;
        }
    }
    // A session that expired while the app was closed opens straight onto the Done screen —
    // update's Tick handler does the transition; seeding only restores the state.
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DEFAULT_MINUTES;
    use chrono::{Duration, TimeZone};

    fn at(h: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 7, 14, h, 0, 0).unwrap()
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let now = at(9);
        let mut m = Model::new(now);
        m.picked = Some(Picked { id: "42".into(), title: "Write the report".into() });
        m.timer = Timer::Running { end: now + Duration::minutes(60), duration_min: 60 };
        let s = snapshot(&m);
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        let seeded = seed(Some(back), now);
        assert_eq!(seeded.picked, m.picked);
        assert_eq!(seeded.timer, m.timer);
        assert!(!seeded.done);
    }

    #[test]
    fn stale_snapshot_resets_to_a_fresh_day() {
        let yesterday = at(9) - Duration::days(1);
        let mut m = Model::new(yesterday);
        m.picked = Some(Picked { id: "1".into(), title: "Old thing".into() });
        m.timer = Timer::Paused { remaining_secs: 100, duration_min: 25 };
        let s = snapshot(&m);
        let seeded = seed(Some(s), at(9));
        assert_eq!(seeded.picked, None, "a new day invites a new pick");
        assert_eq!(seeded.timer, Timer::Idle { duration_min: DEFAULT_MINUTES });
        assert_eq!(seeded.date, at(9).date_naive());
    }

    #[test]
    fn seconds_preference_survives_the_day_rollover() {
        let yesterday = at(9) - Duration::days(1);
        let mut m = Model::new(yesterday);
        m.seconds = true;
        let seeded = seed(Some(snapshot(&m)), at(9));
        assert!(seeded.seconds, "display preference is not day state");
        assert_eq!(seeded.picked, None);
    }
}
