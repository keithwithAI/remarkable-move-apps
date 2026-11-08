//! The habit tracker's Elm-style state: a pure model updated by [`crate::update`], drawn by
//! [`crate::view`]. All I/O (scanning, file writes, sync, the framebuffer) lives in the binary.

use crate::file::HabitFile;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct HabitModel {
    pub habits: Vec<HabitFile>,
    pub screen: Screen,
    pub today: NaiveDate,
    /// Wall clock (flash expiry + midnight rollover), advanced by [`HabitEvent::Tick`].
    pub now: DateTime<Local>,
    pub flash: Option<Flash>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Today,
    /// A habit's history: `month` is `(year, month0-less 1-based month)` being viewed.
    Detail { ix: usize, month: (i32, u32) },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub expires_at: DateTime<Local>,
}

#[derive(Debug, Clone)]
pub enum HabitEvent {
    Tap { x: i32, y: i32 },
    Tick { now: DateTime<Local> },
    /// The binary rescanned the notes dir (startup, refresh, resume, post-write heal).
    Rescanned(Vec<HabitFile>),
}

/// What a tap region means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The check circle: toggle today's log line for `habits[ix]`.
    Toggle(usize),
    /// The rest of the row: open the habit's detail/heatmap.
    Open(usize),
    Back,
    MonthPrev,
    MonthNext,
    /// Rescan the dir + nudge a vault sync.
    Refresh,
    Exit,
}

/// What the update step asks the runtime to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HabitEffect {
    /// Toggle `date`'s `- [x]` line in the note at `path` (read fresh, edit, write atomically).
    WriteToggle { path: PathBuf, date: NaiveDate, on: bool },
    /// Spawn a detached vaultsync (fire-and-forget git push).
    Sync,
    /// Rescan the dir and feed back [`HabitEvent::Rescanned`].
    Rescan,
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

impl HabitModel {
    pub fn new(habits: Vec<HabitFile>, now: DateTime<Local>) -> Self {
        HabitModel { habits, screen: Screen::Today, today: now.date_naive(), now, flash: None }
    }

    pub(crate) fn flash(&mut self, text: &str) {
        self.flash = Some(Flash { text: text.to_string(), expires_at: self.now + chrono::Duration::seconds(3) });
    }

    /// This month's `(year, month)` — the Detail screen's initial view.
    pub(crate) fn this_month(&self) -> (i32, u32) {
        (self.today.year(), self.today.month())
    }
}
