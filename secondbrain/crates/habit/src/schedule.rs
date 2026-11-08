//! When a habit is due, and how long its streak is.

use chrono::{Datelike, Duration, NaiveDate, Weekday};
use sb_core::fm::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Daily,
    Weekdays,
    Days(Vec<Weekday>),
}

/// Parse the frontmatter `schedule:` value. Scalar `daily`/`weekdays`, or a day list
/// (`[mon, wed, fri]` — 3-letter prefixes, case-insensitive). Anything unrecognised falls back
/// to `Daily`: a typo must never hide a habit.
pub fn parse_schedule(v: Option<&Value>) -> Schedule {
    match v {
        Some(Value::Scalar(s)) if s.eq_ignore_ascii_case("weekdays") => Schedule::Weekdays,
        Some(Value::Scalar(s)) if s.eq_ignore_ascii_case("daily") => Schedule::Daily,
        Some(Value::List(items)) => {
            let days: Vec<Weekday> = items.iter().filter_map(|s| parse_day(s)).collect();
            if days.is_empty() {
                Schedule::Daily
            } else {
                Schedule::Days(days)
            }
        }
        _ => Schedule::Daily,
    }
}

fn parse_day(s: &str) -> Option<Weekday> {
    match s.trim().to_lowercase().get(..3)? {
        "mon" => Some(Weekday::Mon),
        "tue" => Some(Weekday::Tue),
        "wed" => Some(Weekday::Wed),
        "thu" => Some(Weekday::Thu),
        "fri" => Some(Weekday::Fri),
        "sat" => Some(Weekday::Sat),
        "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

impl Schedule {
    pub fn is_due(&self, d: NaiveDate) -> bool {
        match self {
            Schedule::Daily => true,
            Schedule::Weekdays => !matches!(d.weekday(), Weekday::Sat | Weekday::Sun),
            Schedule::Days(days) => days.contains(&d.weekday()),
        }
    }

    /// Human line for the detail masthead.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Daily => "Every day".into(),
            Schedule::Weekdays => "Weekdays".into(),
            Schedule::Days(days) => days.iter().map(|d| &day_name(*d)[..3]).collect::<Vec<_>>().join(" · "),
        }
    }
}

fn day_name(d: Weekday) -> &'static str {
    match d {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// Consecutive due-days done, counting backwards from `today`. Non-due days are skipped.
/// Grace rule: today being due but not yet checked does NOT break the streak (the day isn't
/// over) — counting simply starts from the previous due day; once today is checked it counts.
pub fn streak(done: &BTreeSet<NaiveDate>, sched: &Schedule, today: NaiveDate) -> u32 {
    let Some(&first_done) = done.iter().next() else {
        return 0;
    };
    let mut n = 0u32;
    let mut day = today;
    let mut graced = false;
    loop {
        if sched.is_due(day) {
            if done.contains(&day) {
                n += 1;
            } else if day == today && !graced {
                graced = true; // today isn't over — look further back
            } else {
                break;
            }
        }
        if day <= first_done {
            break; // nothing earlier can be done
        }
        day -= Duration::days(1);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }
    fn set(days: &[NaiveDate]) -> BTreeSet<NaiveDate> {
        days.iter().copied().collect()
    }

    #[test]
    fn schedules_parse_with_daily_fallback() {
        use sb_core::fm::Value::*;
        assert_eq!(parse_schedule(Some(&Scalar("weekdays".into()))), Schedule::Weekdays);
        assert_eq!(parse_schedule(Some(&Scalar("Daily".into()))), Schedule::Daily);
        assert_eq!(
            parse_schedule(Some(&List(vec!["Mon".into(), "wednesday".into(), "FRI".into()]))),
            Schedule::Days(vec![Weekday::Mon, Weekday::Wed, Weekday::Fri])
        );
        assert_eq!(parse_schedule(Some(&Scalar("fortnightly?".into()))), Schedule::Daily, "typos never hide a habit");
        assert_eq!(parse_schedule(None), Schedule::Daily);
        assert_eq!(parse_schedule(Some(&List(vec!["blursday".into()]))), Schedule::Daily);
    }

    #[test]
    fn due_days() {
        // 2026-07-10 is a Friday; 11 Sat, 12 Sun, 13 Mon.
        assert!(Schedule::Daily.is_due(d(7, 11)));
        assert!(Schedule::Weekdays.is_due(d(7, 10)) && !Schedule::Weekdays.is_due(d(7, 11)));
        let mwf = Schedule::Days(vec![Weekday::Mon, Weekday::Wed, Weekday::Fri]);
        assert!(mwf.is_due(d(7, 10)) && !mwf.is_due(d(7, 11)) && mwf.is_due(d(7, 13)));
    }

    #[test]
    fn streak_counts_consecutive_due_days_with_todays_grace() {
        let today = d(7, 10); // Friday
        let daily = Schedule::Daily;
        // Done through yesterday, today unchecked → grace keeps the streak at 3.
        let done = set(&[d(7, 7), d(7, 8), d(7, 9)]);
        assert_eq!(streak(&done, &daily, today), 3);
        // Check today → 4.
        let done = set(&[d(7, 7), d(7, 8), d(7, 9), d(7, 10)]);
        assert_eq!(streak(&done, &daily, today), 4);
        // A gap two days ago breaks it (only today gets grace).
        let done = set(&[d(7, 6), d(7, 7), d(7, 9)]);
        assert_eq!(streak(&done, &daily, today), 1);
        // Empty log → 0.
        assert_eq!(streak(&set(&[]), &daily, today), 0);
    }

    #[test]
    fn streak_skips_non_due_days() {
        // Weekdays habit checked Thu+Fri, then the weekend passes, Monday is today and unchecked:
        // grace on Monday, weekend skipped, Fri+Thu count → 2.
        let sched = Schedule::Weekdays;
        let done = set(&[d(7, 9), d(7, 10)]); // Thu, Fri
        assert_eq!(streak(&done, &sched, d(7, 13)), 2);
        // Checked Monday too → 3.
        let done = set(&[d(7, 9), d(7, 10), d(7, 13)]);
        assert_eq!(streak(&done, &sched, d(7, 13)), 3);
        // Missed Friday breaks the chain even across the weekend.
        let done = set(&[d(7, 9), d(7, 13)]);
        assert_eq!(streak(&done, &sched, d(7, 13)), 1);
    }
}
