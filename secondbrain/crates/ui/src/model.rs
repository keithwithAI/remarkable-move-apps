//! The planner's Elm-style state: a pure model updated by [`crate::update`], drawn by
//! [`crate::view`]. All I/O (fetching, the action queue, the framebuffer) lives in the binary.

use actionqueue::Action;
use chrono::{DateTime, Local, NaiveDate};
use render::ui::KbLayer;
use sb_core::{Event, PlannerState, Task};

#[derive(Debug, Clone)]
pub struct AppModel {
    pub today: NaiveDate,
    pub view_date: NaiveDate,
    /// Wall clock, advanced by [`UiEvent::Tick`] (used for flash expiry and the NOW marker).
    pub now: DateTime<Local>,
    pub planner: PlannerState,
    pub overlay: Option<Overlay>,
    /// Queued offline actions (status badge).
    pub pending: usize,
    pub flash: Option<Flash>,
    pub fetching: bool,
    /// How many of the viewed day's events are scrolled off the top ("… N more" paging).
    /// Reset on day navigation; clamped on refetch.
    pub day_scroll: usize,
    /// Monotonic client counter for quick-add `temp_id`s (so distinct creates never dedupe).
    pub quickadd_seq: u64,
}

impl AppModel {
    pub fn new(planner: PlannerState, now: DateTime<Local>) -> Self {
        AppModel {
            today: now.date_naive(),
            view_date: now.date_naive(),
            now,
            planner,
            overlay: None,
            pending: 0,
            flash: None,
            fetching: false,
            day_scroll: 0,
            quickadd_seq: 0,
        }
    }

    /// Todoist metadata for a task id (recurring flag, priority) — tasks are fetched purely
    /// as this index; the visible items are the calendar blocks.
    pub(crate) fn find_task(&self, task_id: &str) -> Option<&Task> {
        self.planner
            .days
            .values()
            .flat_map(|d| d.tasks.iter())
            .find(|t| t.id == task_id)
    }

    /// The Diarized calendar block for a Todoist task id.
    pub(crate) fn find_block(&self, task_id: &str) -> Option<&Event> {
        self.planner
            .days
            .values()
            .flat_map(|d| d.events.iter())
            .find(|e| e.todoist_task.as_deref() == Some(task_id))
    }

    /// `(calendar_id, event_id)` for **every** calendar block belonging to a task — the handles a
    /// queued `Action::DeleteEvent` needs.
    ///
    /// A `Vec` because a task can own more than one block (Diarized splits long work across
    /// sittings), which is why [`Self::remove_task`] retains rather than removing one. Blocks whose
    /// Google event id is missing are skipped: that only happens on pre-v0.14 cached state, and
    /// there is nothing to address a delete to.
    pub(crate) fn block_refs(&self, task_id: &str) -> Vec<(String, String)> {
        self.planner
            .days
            .values()
            .flat_map(|d| d.events.iter())
            .filter(|e| e.todoist_task.as_deref() == Some(task_id))
            .filter_map(|e| e.id.clone().map(|id| (e.calendar_id.clone(), id)))
            .collect()
    }

    /// Remove a task everywhere it appears — metadata AND its calendar blocks
    /// (optimistic complete; Diarized deletes the event server-side after its next sync).
    pub(crate) fn remove_task(&mut self, task_id: &str) {
        for day in self.planner.days.values_mut() {
            day.tasks.retain(|t| t.id != task_id);
            day.events.retain(|e| e.todoist_task.as_deref() != Some(task_id));
        }
    }

    /// Rewrite a task's priority everywhere its metadata appears (optimistic reprioritise).
    /// Mirrors `actionqueue::Queue::apply_overlay`'s SetPriority arm. A Diarized block with an
    /// explicit colorId keeps its colour until Diarized re-syncs.
    pub(crate) fn set_task_priority(&mut self, task_id: &str, priority: u8) {
        for day in self.planner.days.values_mut() {
            for t in day.tasks.iter_mut().filter(|t| t.id == task_id) {
                t.priority = priority;
            }
        }
    }

    /// Drop a plain calendar event everywhere it appears, by Google event id (optimistic
    /// delete; `actionqueue::Queue::apply_overlay` repeats this on refetched server truth
    /// until the queued delete lands).
    pub(crate) fn remove_event(&mut self, event_id: &str) {
        for day in self.planner.days.values_mut() {
            day.events.retain(|e| e.id.as_deref() != Some(event_id));
        }
    }

    /// Move a task to a new due date's bucket (optimistic defer). Mirrors
    /// `actionqueue::Queue::apply_overlay`, which repeats this on refetched server truth.
    pub(crate) fn move_task(&mut self, task_id: &str, due: NaiveDate) {
        let mut moved = None;
        for day in self.planner.days.values_mut() {
            if let Some(pos) = day.tasks.iter().position(|t| t.id == task_id) {
                moved = Some(day.tasks.remove(pos));
                break;
            }
        }
        if let Some(mut task) = moved {
            task.due = Some(due);
            if let Some(day) = self.planner.days.get_mut(&due) {
                day.tasks.push(task);
                day.tasks.sort_by_key(|t| t.sort_key());
            }
        }
    }
}

/// A modal layer over the day view. While present, only overlay targets are tappable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// Action card for a Diarized block: Complete, quick-pick defer (when safe), reprioritise,
    /// Delete, Cancel. `can_defer` is false for recurring tasks (a due-date rewrite would break the
    /// recurrence) and when the Todoist metadata wasn't found (can't verify → play safe).
    /// `priority` is the task's current raw Todoist value (4 = P1 … 1 = P4) so the card can show
    /// the active chip.
    TaskCard { task_id: String, title: String, can_defer: bool, priority: u8 },
    /// Keyboard-driven quick-add of a new Todoist task (bottom sheet + on-screen keyboard).
    QuickAdd(QuickAdd),
    /// Action card for a plain Google Calendar event: Delete (the one event action), Cancel.
    EventCard { calendar_id: String, event_id: String, title: String },
}

/// State of the quick-add sheet: the typed text, the keyboard mode, the chosen due, and the
/// chosen priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickAdd {
    pub text: String,
    pub layer: KbLayer,
    pub shift: bool,
    pub due: DueChoice,
    pub priority: Prio,
}

impl QuickAdd {
    pub fn new() -> Self {
        QuickAdd {
            text: String::new(),
            layer: KbLayer::Letters,
            shift: false,
            due: DueChoice::Inbox,
            priority: Prio::P4, // Todoist's own default
        }
    }
}

impl Default for QuickAdd {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a quick-added task lands — its Todoist due. `Inbox` = no date (the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueChoice {
    Inbox,
    Today,
    Tomorrow,
}

impl DueChoice {
    pub const ALL: [DueChoice; 3] = [DueChoice::Inbox, DueChoice::Today, DueChoice::Tomorrow];
    /// The Todoist natural-language due string (`None` for the inbox).
    pub fn due_string(self) -> Option<&'static str> {
        match self {
            DueChoice::Inbox => None,
            DueChoice::Today => Some("today"),
            DueChoice::Tomorrow => Some("tomorrow"),
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            DueChoice::Inbox => "Inbox",
            DueChoice::Today => "Today",
            DueChoice::Tomorrow => "Tomorrow",
        }
    }
}

/// A quick-add / reprioritise priority pick, in DISPLAY order (P1 = most urgent). The raw
/// Todoist value inverts: P1 ↔ 4 … P4 ↔ 1 — `api()` owns that mapping, and the raw value is
/// what the rest of the codebase stores and colours by (`render::priority_color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prio {
    P1,
    P2,
    P3,
    P4,
}

impl Prio {
    pub const ALL: [Prio; 4] = [Prio::P1, Prio::P2, Prio::P3, Prio::P4];
    /// The raw Todoist API value (4 = P1 urgent … 1 = P4 default).
    pub fn api(self) -> u8 {
        match self {
            Prio::P1 => 4,
            Prio::P2 => 3,
            Prio::P3 => 2,
            Prio::P4 => 1,
        }
    }
    /// From a raw Todoist value (clamped).
    pub fn from_api(v: u8) -> Prio {
        match v.clamp(1, 4) {
            4 => Prio::P1,
            3 => Prio::P2,
            2 => Prio::P3,
            _ => Prio::P4,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Prio::P1 => "P1",
            Prio::P2 => "P2",
            Prio::P3 => "P3",
            Prio::P4 => "P4",
        }
    }
}

/// Transient one-line notice ("Deferred to Mon 6 Jul").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub expires_at: DateTime<Local>,
}

/// Everything the outside world can tell the model.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Tap { x: i32, y: i32 },
    /// Fresh data (the caller has already applied the queue overlay to it).
    DataRefreshed(PlannerState),
    FetchFailed(String),
    /// A status line from the background self-update check (fed by the runtime; shown as a flash):
    /// "Checking for updates…" / "Updating…" / "Up to date (vX)" / "Updated to vX".
    UpdateChecked(String),
    /// The queue length changed (after an enqueue or a background flush).
    QueueChanged(usize),
    /// Periodic clock tick: expires the flash, detects midnight rollover.
    Tick { now: DateTime<Local> },
}

/// What the update step asks the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Enqueue(Action),
    RequestFetch,
    Redraw(Dirty),
    Exit,
}

/// Redraw hint for the e-ink refresh strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Full,
    /// x, y, w, h in framebuffer pixels.
    Region(i32, i32, i32, i32),
}
