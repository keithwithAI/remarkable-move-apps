//! Quick-pick defer date math (pure).

use chrono::{Datelike, Duration, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferChoice {
    Tomorrow,
    PlusTwoDays,
    /// The next Monday strictly after the base date.
    NextWeek,
}

/// The new due date for a defer choice. `base` is the day the user is looking at (clamped to
/// today by the caller, so "tomorrow" never lands in the past).
pub fn defer_target(base: NaiveDate, choice: DeferChoice) -> NaiveDate {
    match choice {
        DeferChoice::Tomorrow => base + Duration::days(1),
        DeferChoice::PlusTwoDays => base + Duration::days(2),
        DeferChoice::NextWeek => {
            let ahead = (7 - base.weekday().num_days_from_monday() as i64) % 7;
            base + Duration::days(if ahead == 0 { 7 } else { ahead })
        }
    }
}

/// Label parts for a defer button: the word in the body face, the concrete date set in mono.
pub fn defer_label_parts(base: NaiveDate, choice: DeferChoice) -> (&'static str, String) {
    let target = defer_target(base, choice);
    let word = match choice {
        DeferChoice::Tomorrow => "Tomorrow",
        DeferChoice::PlusTwoDays => "In 2 days",
        DeferChoice::NextWeek => "Next week",
    };
    (word, target.format("%a %-d %b").to_string())
}

/// Short label with the concrete date, e.g. "Tomorrow · Fri 3 Jul".
pub fn defer_label(base: NaiveDate, choice: DeferChoice) -> String {
    let (word, day) = defer_label_parts(base, choice);
    format!("{word} · {day}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Weekday;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn tomorrow_and_plus_two_cross_month_ends() {
        assert_eq!(defer_target(d(2026, 6, 30), DeferChoice::Tomorrow), d(2026, 7, 1));
        assert_eq!(defer_target(d(2026, 12, 31), DeferChoice::PlusTwoDays), d(2027, 1, 2));
    }

    #[test]
    fn next_week_is_strictly_next_monday() {
        // 2026-07-02 is a Thursday; next Monday is the 6th.
        assert_eq!(defer_target(d(2026, 7, 2), DeferChoice::NextWeek), d(2026, 7, 6));
        // Friday / Saturday / Sunday all land on the same Monday.
        assert_eq!(defer_target(d(2026, 7, 3), DeferChoice::NextWeek), d(2026, 7, 6));
        assert_eq!(defer_target(d(2026, 7, 4), DeferChoice::NextWeek), d(2026, 7, 6));
        assert_eq!(defer_target(d(2026, 7, 5), DeferChoice::NextWeek), d(2026, 7, 6));
        // From a Monday, "next week" means a full week ahead, not today.
        assert_eq!(defer_target(d(2026, 7, 6), DeferChoice::NextWeek), d(2026, 7, 13));
        assert_eq!(defer_target(d(2026, 7, 6), DeferChoice::NextWeek).weekday(), Weekday::Mon);
    }

    #[test]
    fn labels_include_concrete_dates() {
        assert_eq!(defer_label(d(2026, 7, 2), DeferChoice::Tomorrow), "Tomorrow · Fri 3 Jul");
        assert_eq!(defer_label(d(2026, 7, 2), DeferChoice::NextWeek), "Next week · Mon 6 Jul");
    }
}
