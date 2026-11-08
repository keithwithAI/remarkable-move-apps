//! The One Thing app's Elm-style state: a pure [`Model`] updated by [`crate::update`], drawn by
//! [`crate::view`]. All I/O (Todoist fetch/complete, persistence, the framebuffer) lives in the
//! binary.
//!
//! The timer is **wall-clock math, never counted**: `Running` stores an absolute `end` instant and
//! every redraw derives `remaining = end − now`. The device suspends aggressively mid-session and
//! that's fine — waking shows the correct remaining time (or the finished screen). The flip side,
//! accepted by design: while suspended the app is frozen, so "time's up" only appears on wake.

use chrono::{DateTime, Duration, Local, NaiveDate};
use render::ui::KbLayer;
use sb_core::Task;
use serde::{Deserialize, Serialize};

/// The Todoist task chosen as today's one thing. Only the identity is kept — the full task list
/// is transient picker state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Picked {
    pub id: String,
    pub title: String,
}

/// The focus timer. Every variant carries the session length so pause/reset/"go again" know what
/// they return to; `Running.end` is the wall-clock anchor that survives suspend and restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Timer {
    Idle { duration_min: u32 },
    Running { end: DateTime<Local>, duration_min: u32 },
    Paused { remaining_secs: i64, duration_min: u32 },
    Finished { duration_min: u32 },
}

/// The session-length presets offered on the Home screen (minutes); 60 is the default.
pub const PRESETS: [u32; 4] = [25, 45, 60, 90];
pub const DEFAULT_MINUTES: u32 = 60;

impl Timer {
    pub fn duration_min(&self) -> u32 {
        match *self {
            Timer::Idle { duration_min }
            | Timer::Running { duration_min, .. }
            | Timer::Paused { duration_min, .. }
            | Timer::Finished { duration_min } => duration_min,
        }
    }

    /// Seconds left on the clock at `now` (never negative). Idle shows the full session;
    /// Finished shows zero.
    pub fn remaining_secs(&self, now: DateTime<Local>) -> i64 {
        match self {
            Timer::Idle { duration_min } => *duration_min as i64 * 60,
            Timer::Running { end, .. } => (*end - now).num_seconds().max(0),
            Timer::Paused { remaining_secs, .. } => (*remaining_secs).max(0),
            Timer::Finished { .. } => 0,
        }
    }

    /// Start (or restart) a session of the current length, anchored at `now`.
    pub fn started(&self, now: DateTime<Local>) -> Timer {
        let m = self.duration_min();
        Timer::Running { end: now + Duration::minutes(m as i64), duration_min: m }
    }
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Picker,
    /// The full-screen "Time's up" state.
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub expires_at: DateTime<Local>,
}

#[derive(Debug, Clone)]
pub struct Model {
    pub screen: Screen,
    /// The day the pick + timer belong to; a date change resets both (a new day, a new one thing).
    pub date: NaiveDate,
    pub picked: Option<Picked>,
    /// Today's one thing was completed (checked off) — Home shows it struck through.
    pub done: bool,
    pub timer: Timer,
    /// The full open-task list for the picker (fetched by the runtime's worker; cached offline).
    pub tasks: Vec<Task>,
    /// Picker search query + on-screen keyboard state (shared render::ui keyboard).
    pub query: String,
    pub layer: KbLayer,
    pub shift: bool,
    /// Picker list scroll: index of the first visible (filtered) row.
    pub scroll: usize,
    /// A task fetch is in flight (picker header shows "fetching…").
    pub fetching: bool,
    /// Offline actions queued (completions waiting for WiFi) — badged on Home when > 0.
    pub pending: usize,
    pub flash: Option<Flash>,
    /// Wall clock, advanced by [`Event::Tick`]. Drives the countdown, flash expiry, day rollover.
    pub now: DateTime<Local>,
    /// Live-seconds display: `false` (default) shows whole minutes until the final minute —
    /// e-ink kindness, ~120 updates per hour session instead of 3,600 hammering the digit band;
    /// `true` shows MM:SS throughout. Toggled by tapping the clock; persisted across days.
    pub seconds: bool,
    /// The display key last drawn — the redraw-cadence diff (see [`crate::update::display_key`]).
    pub last_shown_key: i64,
}

#[derive(Debug, Clone)]
pub enum Event {
    Tap { x: i32, y: i32 },
    Tick { now: DateTime<Local> },
    /// The worker fetched the open-task list (or loaded the offline cache at startup).
    TasksFetched(Vec<Task>),
    FetchFailed(String),
    /// The offline queue length changed (enqueue or flush).
    QueueChanged(usize),
    /// A status line from the background self-update check.
    UpdateChecked(String),
}

/// What a tap region means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Home: set the session length (Idle only).
    Preset(u32),
    PlayPause,
    Reset,
    /// Home/Done → open the task picker.
    OpenPicker,
    /// Picker: leave without changing the pick.
    Back,
    /// Picker: re-fetch the task list.
    Refresh,
    /// Picker: choose `tasks[ix]` as the one thing.
    Row(usize),
    PagePrev,
    PageNext,
    /// Picker keyboard (shared render::ui::draw_keyboard).
    Key(char),
    Backspace,
    Shift,
    Layer,
    /// Keyboard Enter ("Pick"): choose the top filtered row.
    Submit,
    ClearQuery,
    /// Done screen: complete the task in Todoist (via the offline queue).
    MarkDone,
    /// Done screen: start another session on the same task.
    GoAgain,
    /// Done/Home: back to Home without acting.
    Dismiss,
    /// Home: tap the clock — flip between calm whole-minutes display and live MM:SS.
    ToggleSeconds,
    Exit,
}

/// What the update step asks the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Enqueue an offline Todoist write (Complete) and nudge the worker to flush.
    Enqueue(actionqueue::Action),
    /// Ask the worker for a fresh task list.
    RequestFetch,
    /// Save the `{date, picked, done, timer}` snapshot to onething.json.
    Persist,
    Redraw(Dirty),
    Exit,
}

/// Redraw hint for the e-ink refresh strategy (same shape as the other apps').
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Full,
    /// x, y, w, h in framebuffer pixels.
    Region(i32, i32, i32, i32),
}

impl Model {
    pub fn new(now: DateTime<Local>) -> Self {
        Model {
            screen: Screen::Home,
            date: now.date_naive(),
            picked: None,
            done: false,
            timer: Timer::Idle { duration_min: DEFAULT_MINUTES },
            tasks: Vec::new(),
            query: String::new(),
            layer: KbLayer::Letters,
            shift: false,
            scroll: 0,
            fetching: false,
            pending: 0,
            flash: None,
            now,
            seconds: false,
            last_shown_key: i64::MIN,
        }
    }

    pub(crate) fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash { text: text.into(), expires_at: self.now + Duration::seconds(3) });
    }

    /// Indices into `tasks` the picker shows: case-insensitive substring of the query over the
    /// title, sorted priority-high-first, then due date (soonest first, undated last), then title.
    pub fn filtered(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        let mut ix: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| !self.tasks[i].done)
            .filter(|&i| q.is_empty() || self.tasks[i].title.to_lowercase().contains(&q))
            .collect();
        ix.sort_by(|&a, &b| {
            let (ta, tb) = (&self.tasks[a], &self.tasks[b]);
            tb.priority
                .cmp(&ta.priority)
                .then_with(|| match (ta.due, tb.due) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| ta.title.to_lowercase().cmp(&tb.title.to_lowercase()))
        });
        ix
    }
}
