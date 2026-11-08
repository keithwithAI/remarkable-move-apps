//! Offline queue for Todoist writes (complete / defer).
//!
//! Every enqueue persists immediately (atomic rename via `sb_core::cache::save_json`), so a
//! crash or AppLoad SIGTERM never loses an action. `flush` drains against the API and stops on
//! the first transient error (offline → don't hammer); permanently-failed actions are parked in
//! `actions.failed.json` rather than silently dropped. `apply_overlay` re-applies still-pending
//! actions on top of freshly-fetched server truth, so the UI (and the sleep screen) never
//! resurrect a task completed offline.
//!
//! Both the planner app and the `agenda` timer flush this queue — and the timer is NOT an
//! AppLoad app, so it genuinely runs concurrently with the planner. Queue WRITES are therefore
//! serialized by [`try_write_lock`]: the planner holds the lock for its session; the timer takes
//! it per-flush and skips flushing when the planner has it (the planner flushes itself). Without
//! it, two load-mutate-save cycles could drop a freshly-enqueued action (lost update).

use chrono::{DateTime, Local, NaiveDate};
use sb_core::cache::{load_json, save_json};
use sb_core::PlannerState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_ATTEMPTS: u32 = 10;

/// A queued Todoist write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Complete { task_id: String },
    Defer { task_id: String, due: NaiveDate },
    Delete { task_id: String },
    /// Quick-add a brand-new task. `temp_id` is a client-generated dedupe key — a create has no
    /// server id yet, so (unlike the other writes) distinct creates must never collapse together.
    /// `due` is an optional Todoist natural-language due string ("today", "tomorrow"); `None` = inbox.
    /// `priority` is the raw Todoist value (4 = P1 … 1 = P4); `None` = server default (P4).
    /// serde(default) is LOAD-BEARING: queues persisted before the field existed must still load —
    /// a parse failure silently resets the whole queue.
    Create {
        temp_id: String,
        content: String,
        due: Option<String>,
        #[serde(default)]
        priority: Option<u8>,
    },
    /// Rewrite a task's priority (raw Todoist value, 4 = P1 … 1 = P4). Lives on its own dedupe
    /// channel: it must never evict (or be evicted by) a pending Complete/Defer for the same task.
    SetPriority { task_id: String, priority: u8 },
    /// Delete a Google Calendar event (a recurring *instance* when the id is an instance id). The
    /// one non-Todoist write; keyed/deduped by the event id.
    DeleteEvent { calendar_id: String, event_id: String },
    /// **Retired 2026-08-17; retained only so old queue files still load.** Asked Diarized to
    /// re-place one task's block, via the action URL published in the event description. Diarized
    /// put those routes behind a login session and a subscription check, so a request from the
    /// device gets a redirect to `/login` — the action could never succeed again, and nothing
    /// enqueues it any more.
    ///
    /// Keeping the variant is **load-bearing**, not tidiness. `Queue::load` does
    /// `load_json(..).unwrap_or_default()`, so one unrecognised `"kind"` fails the whole-file parse
    /// and silently discards every *other* queued action with it — including completions a user
    /// made offline. Any `recalculate` left in a device's `actions.json` is therefore parsed and
    /// then dropped as a no-op by the flush. Delete this once no device can still be carrying one.
    Recalculate { task_id: String, url: String },
}

impl Action {
    pub fn task_id(&self) -> &str {
        match self {
            Action::Complete { task_id }
            | Action::Defer { task_id, .. }
            | Action::Delete { task_id }
            | Action::SetPriority { task_id, .. }
            | Action::Recalculate { task_id, .. } => task_id,
            // creates key on their client temp_id, so distinct quick-adds never dedupe each other
            Action::Create { temp_id, .. } => temp_id,
            Action::DeleteEvent { event_id, .. } => event_id,
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Action::Complete { task_id } => format!("complete {task_id}"),
            Action::Defer { task_id, due } => format!("defer {task_id} → {due}"),
            Action::Delete { task_id } => format!("delete {task_id}"),
            Action::Create { content, due, .. } => match due {
                Some(d) => format!("create \"{content}\" ({d})"),
                None => format!("create \"{content}\""),
            },
            Action::SetPriority { task_id, priority } => {
                format!("priority {task_id} → P{}", 5u8.saturating_sub(*priority).max(1))
            }
            Action::DeleteEvent { event_id, .. } => format!("delete event {event_id}"),
            Action::Recalculate { task_id, .. } => format!("retired recalculate {task_id}"),
        }
    }

    /// Would enqueueing `self` supersede `old`? Same key AND same CHANNEL: the task-state
    /// channel (Complete/Defer/Delete) collapses to the newest intent, and priority rewrites
    /// collapse among themselves — but a SetPriority must never evict a pending Complete/Defer
    /// (they're orthogonal: "do it tomorrow, and it's a P1"). Creates key on their temp_id and
    /// event deletes on the event id, each their own channel — which is what lets a Complete and
    /// the DeleteEvent for its own block coexist rather than one evicting the other. Retired
    /// Recalculates keep channel 4 so a stale one can't collide with anything live.
    fn supersedes(&self, old: &Action) -> bool {
        if self.task_id() != old.task_id() {
            return false;
        }
        let channel = |a: &Action| match a {
            Action::Complete { .. } | Action::Defer { .. } | Action::Delete { .. } => 0u8,
            Action::SetPriority { .. } => 1,
            Action::Create { .. } => 2,
            Action::DeleteEvent { .. } => 3,
            Action::Recalculate { .. } => 4,
        };
        channel(self) == channel(old)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: u64,
    pub action: Action,
    pub created_at: DateTime<Local>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
}

/// Why a write failed — decides retry vs park.
#[derive(Debug)]
pub enum WriteError {
    /// The request never reached the server (no connection / timeout). Retry forever WITHOUT
    /// counting an attempt — on a mostly-offline device, "offline again" is the normal state,
    /// and counting it toward MAX_ATTEMPTS silently abandoned real completions after ten
    /// sleep-cycle flushes.
    Offline(String),
    /// The server answered but transiently (5xx, 408/409/423/425/429) — retry, counting
    /// attempts so a persistently-failing action is eventually parked, not retried forever.
    Retryable(String),
    /// Other 4xx (task deleted, bad request) — park it; retrying can't succeed.
    Permanent(String),
}

/// The Todoist write surface, as a trait so tests can use a fake.
pub trait TodoistWriter {
    fn complete(&self, task_id: &str) -> Result<(), WriteError>;
    fn defer(&self, task_id: &str, due: NaiveDate) -> Result<(), WriteError>;
    fn delete(&self, task_id: &str) -> Result<(), WriteError>;
    /// Create a task; the server-assigned id is discarded (nothing references it — the day view is
    /// calendar-only, so a new task has no block until Diarized schedules one). `priority` is
    /// the raw Todoist value (4 = P1 … 1 = P4); `None` = server default.
    fn create(&self, content: &str, due: Option<&str>, priority: Option<u8>) -> Result<(), WriteError>;
    /// Rewrite a task's priority (raw Todoist value).
    fn set_priority(&self, task_id: &str, priority: u8) -> Result<(), WriteError>;
    /// Delete a Google Calendar event — a non-Todoist write (the trait carries the whole
    /// device write surface; renaming it would churn every caller for no behavior).
    ///
    /// Also the second half of a completion: [`Action::Complete`] enqueues one of these for each of
    /// the task's blocks, so the block leaves the day view at once instead of lingering until
    /// Diarized's next sync notices the task is done. Diarized reads a user-deleted block as a
    /// completion too, so the two agree rather than fight.
    fn delete_event(&self, calendar_id: &str, event_id: &str) -> Result<(), WriteError>;
}

/// Real implementation over `sources::todoist` with a bearer token, plus the Google service-account
/// key path for the one calendar write ([`Action::DeleteEvent`]).
pub struct TodoistApi {
    pub token: String,
    /// `[google].service_account` key path; `None` = calendar deletes fail permanently (parked).
    pub service_account: Option<std::path::PathBuf>,
}

impl TodoistWriter for TodoistApi {
    fn complete(&self, task_id: &str) -> Result<(), WriteError> {
        sources::todoist::complete_task(&self.token, task_id).map_err(classify)
    }
    fn defer(&self, task_id: &str, due: NaiveDate) -> Result<(), WriteError> {
        sources::todoist::update_due(&self.token, task_id, &sources::todoist::DueUpdate::Date(due))
            .map_err(classify)
    }
    fn delete(&self, task_id: &str) -> Result<(), WriteError> {
        sources::todoist::delete_task(&self.token, task_id).map_err(classify)
    }
    fn create(&self, content: &str, due: Option<&str>, priority: Option<u8>) -> Result<(), WriteError> {
        sources::todoist::create_task(&self.token, content, due, priority).map(|_| ()).map_err(classify)
    }
    fn set_priority(&self, task_id: &str, priority: u8) -> Result<(), WriteError> {
        sources::todoist::update_priority(&self.token, task_id, priority).map_err(classify)
    }
    fn delete_event(&self, calendar_id: &str, event_id: &str) -> Result<(), WriteError> {
        let Some(sa) = &self.service_account else {
            return Err(WriteError::Permanent("no google service account configured".into()));
        };
        sources::gcal::delete_event(sa, calendar_id, event_id, chrono::Local::now()).map_err(classify)
    }
}

fn classify(e: sources::SourceError) -> WriteError {
    match &e {
        // Transport never reached the server: offline, not a failing action.
        sources::SourceError::Http(h) if h.is_connect() || h.is_timeout() => {
            WriteError::Offline(e.to_string())
        }
        // Transient server answers — retryable, but attempts count.
        sources::SourceError::Api(408 | 409 | 423 | 425 | 429, _) => WriteError::Retryable(e.to_string()),
        sources::SourceError::Api(s, _) if (400..500).contains(s) => WriteError::Permanent(e.to_string()),
        _ => WriteError::Retryable(e.to_string()),
    }
}

/// Exclusive inter-process guard for queue writes (flock on `<cache>/actions.lock`; released on
/// drop / process exit, so a crash never leaves a stale lock). See the crate doc for the
/// timer-vs-planner protocol.
pub struct QueueLock {
    _file: std::fs::File,
}

/// Try to take the queue write lock (non-blocking). `Ok(None)` = another process holds it.
pub fn try_write_lock(cache_dir: &Path) -> std::io::Result<Option<QueueLock>> {
    std::fs::create_dir_all(cache_dir).ok();
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(cache_dir.join("actions.lock"))?;
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok(if rc == 0 { Some(QueueLock { _file: f }) } else { None })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct FlushReport {
    pub sent: usize,
    pub parked: usize,
    /// Still queued after this pass (stopped on a transient error).
    pub remaining: usize,
}

/// What [`Queue::resolve`] did with an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Sent,
    Parked,
    WillRetry,
    /// The action was superseded while its send was in flight; outcome discarded.
    Stale,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueFile {
    next_id: u64,
    items: Vec<PendingAction>,
}

/// The persistent queue. All mutating calls persist before returning.
pub struct Queue {
    path: PathBuf,
    file: QueueFile,
}

impl Queue {
    /// Load from `path` (missing or corrupt → empty queue).
    pub fn load(path: &Path) -> Queue {
        let file = load_json::<QueueFile>(path).unwrap_or_default();
        Queue { path: path.to_path_buf(), file }
    }

    pub fn len(&self) -> usize {
        self.file.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.file.items.is_empty()
    }
    pub fn items(&self) -> &[PendingAction] {
        &self.file.items
    }

    /// Queue an action (last action per task PER CHANNEL wins — see [`Action::supersedes`]:
    /// deferring twice keeps the newest date; completing supersedes a defer; a priority rewrite
    /// coexists with a pending defer and only supersedes an earlier priority rewrite).
    pub fn enqueue(&mut self, action: Action, now: DateTime<Local>) -> std::io::Result<()> {
        self.file.items.retain(|p| !action.supersedes(&p.action));
        let id = self.file.next_id;
        self.file.next_id += 1;
        self.file.items.push(PendingAction { id, action, created_at: now, attempts: 0, last_error: None });
        self.persist()
    }

    /// Try to send everything, oldest first. Stops the pass on the first transient error.
    ///
    /// Blocks for the duration of the API calls — fine for the timer binary. The interactive
    /// app uses [`Queue::head`] + [`Queue::resolve`] instead, so no lock is held during
    /// network I/O.
    pub fn flush(&mut self, api: &dyn TodoistWriter) -> FlushReport {
        let mut report = FlushReport::default();
        while let Some(pending) = self.head() {
            let res = match &pending.action {
                Action::Complete { task_id } => api.complete(task_id),
                Action::Defer { task_id, due } => api.defer(task_id, *due),
                Action::Delete { task_id } => api.delete(task_id),
                Action::Create { content, due, priority, .. } => api.create(content, due.as_deref(), *priority),
                Action::SetPriority { task_id, priority } => api.set_priority(task_id, *priority),
                Action::DeleteEvent { calendar_id, event_id } => api.delete_event(calendar_id, event_id),
                // Retired: drain, never send. See `Action::Recalculate`.
                Action::Recalculate { .. } => Ok(()),
            };
            match self.resolve(pending.id, res) {
                Resolved::Sent => report.sent += 1,
                Resolved::Parked => report.parked += 1,
                Resolved::WillRetry => break, // likely offline — stop the whole pass
                Resolved::Stale => {}
            }
        }
        report.remaining = self.file.items.len();
        report
    }

    /// The oldest pending action (send it without holding any lock, then [`Queue::resolve`]).
    pub fn head(&self) -> Option<PendingAction> {
        self.file.items.first().cloned()
    }

    /// Record the outcome of sending action `id`. Persists before returning.
    pub fn resolve(&mut self, id: u64, outcome: Result<(), WriteError>) -> Resolved {
        let Some(idx) = self.file.items.iter().position(|p| p.id == id) else {
            // Superseded by a newer enqueue for the same task while the send was in flight.
            return Resolved::Stale;
        };
        let resolved = match outcome {
            Ok(()) => {
                log::info!("action sent: {}", self.file.items[idx].action.describe());
                self.file.items.remove(idx);
                Resolved::Sent
            }
            Err(WriteError::Permanent(msg)) => {
                log::warn!("action parked ({}): {msg}", self.file.items[idx].action.describe());
                self.file.items[idx].last_error = Some(msg);
                let dead = self.file.items.remove(idx);
                self.park(dead);
                Resolved::Parked
            }
            Err(WriteError::Offline(msg)) => {
                // No attempt counted: offline is the device's normal state, not the action's
                // fault. The queue simply waits for connectivity.
                let pending = &mut self.file.items[idx];
                pending.last_error = Some(msg.clone());
                log::info!("offline — action kept ({}): {msg}", pending.action.describe());
                Resolved::WillRetry
            }
            Err(WriteError::Retryable(msg)) => {
                let pending = &mut self.file.items[idx];
                pending.attempts += 1;
                pending.last_error = Some(msg.clone());
                if pending.attempts >= MAX_ATTEMPTS {
                    log::warn!(
                        "action parked after {MAX_ATTEMPTS} attempts ({}): {msg}",
                        pending.action.describe()
                    );
                    let dead = self.file.items.remove(idx);
                    self.park(dead);
                    Resolved::Parked
                } else {
                    log::info!("action will retry ({}): {msg}", pending.action.describe());
                    Resolved::WillRetry
                }
            }
        };
        if let Err(e) = self.persist() {
            log::warn!("queue persist failed: {e}");
        }
        resolved
    }

    /// Re-apply still-pending actions on top of freshly-fetched server truth, so optimistic
    /// state survives a refetch that raced the flush.
    ///
    /// Complete hides both the task metadata AND any Diarized calendar block for it (the
    /// server-side event deletion is Diarized's job and lags by a sync cycle). Defer leaves
    /// events where they are — Diarized owns block placement and moves them on its own
    /// reschedule cadence.
    pub fn apply_overlay(&self, state: &mut PlannerState) {
        for pending in &self.file.items {
            match &pending.action {
                Action::Complete { task_id } | Action::Delete { task_id } => {
                    for day in state.days.values_mut() {
                        day.tasks.retain(|t| &t.id != task_id);
                        day.events.retain(|e| e.todoist_task.as_deref() != Some(task_id));
                    }
                }
                Action::Defer { task_id, due } => {
                    let mut moved = None;
                    for day in state.days.values_mut() {
                        if let Some(pos) = day.tasks.iter().position(|t| &t.id == task_id) {
                            moved = Some(day.tasks.remove(pos));
                            break;
                        }
                    }
                    if let Some(mut task) = moved {
                        task.due = Some(*due);
                        if let Some(day) = state.days.get_mut(due) {
                            day.tasks.push(task);
                            day.tasks.sort_by_key(|t| t.sort_key());
                        }
                        // due beyond the window: it simply leaves the visible window
                    }
                }
                // A new task has no block/metadata in the current state — it surfaces on the next
                // fetch (and only in the calendar-only day view once Diarized schedules a block).
                Action::Create { .. } => {}
                // Optimistic retint: update the task metadata everywhere so the P-tag / fallback
                // row tint reflect the pending rewrite. A Diarized block carrying an explicit
                // colorId keeps its colour until Diarized re-syncs — expected, not a bug.
                Action::SetPriority { task_id, priority } => {
                    for day in state.days.values_mut() {
                        for t in day.tasks.iter_mut().filter(|t| &t.id == task_id) {
                            t.priority = *priority;
                        }
                    }
                }
                // Keep a deleted event hidden until the delete actually lands server-side.
                Action::DeleteEvent { event_id, .. } => {
                    for day in state.days.values_mut() {
                        day.events.retain(|e| e.id.as_deref() != Some(event_id));
                    }
                }
                Action::Recalculate { .. } => {} // retired; drains without touching state
            }
        }
    }

    /// Overlay pending actions onto a single day's task list (the suspend-screen path):
    /// completed tasks disappear; deferred tasks disappear when their new due date moved
    /// them off this day.
    pub fn apply_to_tasks(&self, tasks: &mut Vec<sb_core::Task>, date: NaiveDate) {
        for pending in &self.file.items {
            match &pending.action {
                Action::Complete { task_id } | Action::Delete { task_id } => tasks.retain(|t| &t.id != task_id),
                Action::Defer { task_id, due } => {
                    if *due != date {
                        tasks.retain(|t| &t.id != task_id);
                    } else {
                        for t in tasks.iter_mut().filter(|t| &t.id == task_id) {
                            t.due = Some(*due);
                        }
                    }
                }
                Action::Create { .. } => {}
                Action::SetPriority { task_id, priority } => {
                    for t in tasks.iter_mut().filter(|t| &t.id == task_id) {
                        t.priority = *priority;
                    }
                }
                Action::DeleteEvent { .. } => {} // events aren't in the task list
                Action::Recalculate { .. } => {} // retired; drains without touching state
            }
        }
    }

    fn park(&self, dead: PendingAction) {
        let failed_path = self.path.with_file_name("actions.failed.json");
        let mut failed: Vec<PendingAction> = load_json(&failed_path).unwrap_or_default();
        failed.push(dead);
        if let Err(e) = save_json(&failed_path, &failed) {
            log::warn!("could not persist {}: {e}", failed_path.display());
        }
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        save_json(&self.path, &self.file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sb_core::{Event, Task};
    use std::cell::RefCell;

    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap()
    }
    fn date(m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, d).unwrap()
    }
    fn tmp_queue(name: &str) -> (PathBuf, Queue) {
        let dir = std::env::temp_dir().join(format!("sbq-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actions.json");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(dir.join("actions.failed.json"));
        let q = Queue::load(&path);
        (path, q)
    }

    /// Scriptable fake: each call pops the next scripted result for that task.
    #[derive(Default)]
    struct FakeApi {
        // (task_id, result) — Ok, "retry", or "perm"
        script: RefCell<Vec<(String, &'static str)>>,
        calls: RefCell<Vec<String>>,
    }
    impl FakeApi {
        fn scripted(script: &[(&str, &'static str)]) -> Self {
            FakeApi {
                script: RefCell::new(script.iter().map(|(a, b)| (a.to_string(), *b)).collect()),
                calls: RefCell::new(vec![]),
            }
        }
        fn next(&self, task_id: &str, what: &str) -> Result<(), WriteError> {
            self.calls.borrow_mut().push(format!("{what}:{task_id}"));
            let mut s = self.script.borrow_mut();
            let pos = s.iter().position(|(id, _)| id == task_id);
            let outcome = pos.map(|p| s.remove(p).1).unwrap_or("ok");
            match outcome {
                "retry" => Err(WriteError::Retryable("503".into())),
                "offline" => Err(WriteError::Offline("connect timeout".into())),
                "perm" => Err(WriteError::Permanent("404".into())),
                _ => Ok(()),
            }
        }
    }
    impl TodoistWriter for FakeApi {
        fn complete(&self, task_id: &str) -> Result<(), WriteError> {
            self.next(task_id, "complete")
        }
        fn defer(&self, task_id: &str, _due: NaiveDate) -> Result<(), WriteError> {
            self.next(task_id, "defer")
        }
        fn delete(&self, task_id: &str) -> Result<(), WriteError> {
            self.next(task_id, "delete")
        }
        fn create(&self, content: &str, _due: Option<&str>, _priority: Option<u8>) -> Result<(), WriteError> {
            self.next(content, "create")
        }
        fn set_priority(&self, task_id: &str, _priority: u8) -> Result<(), WriteError> {
            self.next(task_id, "set_priority")
        }
        fn delete_event(&self, _calendar_id: &str, event_id: &str) -> Result<(), WriteError> {
            self.next(event_id, "delete_event")
        }
    }

    #[test]
    fn enqueue_persists_and_reloads() {
        let (path, mut q) = tmp_queue("persist");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "t2".into(), due: date(7, 3) }, now()).unwrap();
        let q2 = Queue::load(&path);
        assert_eq!(q2.len(), 2);
        assert_eq!(q2.items()[0].action.task_id(), "t1");
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let (path, _) = tmp_queue("corrupt");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(Queue::load(&path).is_empty());
    }

    #[test]
    fn last_action_per_task_wins() {
        let (_, mut q) = tmp_queue("dedupe");
        q.enqueue(Action::Defer { task_id: "t1".into(), due: date(7, 3) }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "t1".into(), due: date(7, 6) }, now()).unwrap();
        q.enqueue(Action::Complete { task_id: "t2".into() }, now()).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q.items()[0].action, Action::Defer { task_id: "t1".into(), due: date(7, 6) });
    }

    #[test]
    fn flush_sends_all_when_online() {
        let (_, mut q) = tmp_queue("online");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "t2".into(), due: date(7, 3) }, now()).unwrap();
        let api = FakeApi::scripted(&[]);
        let report = q.flush(&api);
        assert_eq!(report, FlushReport { sent: 2, parked: 0, remaining: 0 });
        assert_eq!(*api.calls.borrow(), vec!["complete:t1", "defer:t2"]);
        assert!(q.is_empty());
    }

    #[test]
    fn flush_stops_on_transient_error_and_keeps_action() {
        let (_, mut q) = tmp_queue("offline");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        q.enqueue(Action::Complete { task_id: "t2".into() }, now()).unwrap();
        let api = FakeApi::scripted(&[("t1", "retry")]);
        let report = q.flush(&api);
        assert_eq!(report, FlushReport { sent: 0, parked: 0, remaining: 2 });
        assert_eq!(*api.calls.borrow(), vec!["complete:t1"], "pass stops at first transient failure");
        assert_eq!(q.items()[0].attempts, 1);
        // next pass (network back) drains both
        let api2 = FakeApi::scripted(&[]);
        assert_eq!(q.flush(&api2).sent, 2);
    }

    #[test]
    fn permanent_error_parks_and_continues() {
        let (path, mut q) = tmp_queue("park");
        q.enqueue(Action::Complete { task_id: "gone".into() }, now()).unwrap();
        q.enqueue(Action::Complete { task_id: "t2".into() }, now()).unwrap();
        let api = FakeApi::scripted(&[("gone", "perm")]);
        let report = q.flush(&api);
        assert_eq!(report, FlushReport { sent: 1, parked: 1, remaining: 0 });
        let failed: Vec<PendingAction> =
            load_json(&path.with_file_name("actions.failed.json")).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].action.task_id(), "gone");
        assert_eq!(failed[0].last_error.as_deref(), Some("404"));
    }

    #[test]
    fn max_attempts_parks() {
        let (path, mut q) = tmp_queue("maxattempts");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            let api = FakeApi::scripted(&[("t1", "retry")]);
            q.flush(&api);
        }
        assert!(q.is_empty(), "parked after {MAX_ATTEMPTS} transient failures");
        let failed: Vec<PendingAction> =
            load_json(&path.with_file_name("actions.failed.json")).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].attempts, MAX_ATTEMPTS);
    }

    #[test]
    fn priority_rides_its_own_dedupe_channel() {
        let (_, mut q) = tmp_queue("prio-channel");
        // A pending defer must SURVIVE a later priority rewrite (orthogonal intents)…
        q.enqueue(Action::Defer { task_id: "t1".into(), due: date(7, 3) }, now()).unwrap();
        q.enqueue(Action::SetPriority { task_id: "t1".into(), priority: 4 }, now()).unwrap();
        assert_eq!(q.len(), 2, "defer + priority coexist");
        // …two priority rewrites collapse to the newest…
        q.enqueue(Action::SetPriority { task_id: "t1".into(), priority: 2 }, now()).unwrap();
        assert_eq!(q.len(), 2);
        assert!(q.items().iter().any(|p| matches!(&p.action,
            Action::SetPriority { priority: 2, .. })), "newest priority wins");
        // …and the task-state channel still self-supersedes (complete replaces defer).
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        assert_eq!(q.len(), 2, "complete replaced the defer, priority untouched");
        // Flush sends both.
        let api = FakeApi::scripted(&[]);
        assert_eq!(q.flush(&api).sent, 2);
        assert!(api.calls.borrow().contains(&"set_priority:t1".to_string()));
    }

    #[test]
    fn old_format_create_json_still_loads() {
        // Queues persisted before the priority field existed must load unchanged — a parse
        // failure would silently reset the whole queue.
        let json = r#"{"next_id":2,"items":[{"id":1,
            "action":{"kind":"create","temp_id":"tmp-0","content":"Buy milk","due":"today"},
            "created_at":"2026-07-01T09:00:00+01:00"}]}"#;
        let file: QueueFile = serde_json::from_str(json).expect("old-format queue parses");
        assert_eq!(file.items.len(), 1);
        match &file.items[0].action {
            Action::Create { priority, content, .. } => {
                assert_eq!(*priority, None, "absent priority defaults to None");
                assert_eq!(content, "Buy milk");
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn set_priority_overlays_task_metadata() {
        let (_, mut q) = tmp_queue("prio-overlay");
        q.enqueue(Action::SetPriority { task_id: "t1".into(), priority: 4 }, now()).unwrap();
        let mut tasks = vec![Task {
            id: "t1".into(), title: "x".into(), priority: 1, due: None,
            duration_min: 0, done: false, is_recurring: false,
        }];
        q.apply_to_tasks(&mut tasks, now().date_naive());
        assert_eq!(tasks[0].priority, 4, "optimistic retint");
    }

    #[test]
    fn offline_never_parks_a_valid_action() {
        // The mostly-offline-device regression: a completion queued offline must survive ANY
        // number of no-connectivity flush passes — offline is the device's normal state, not the
        // action's fault. Only server-answered failures count toward MAX_ATTEMPTS.
        // (Unique dir tag: sharing one with another test makes parallel runs race on the queue file.)
        let (_path, mut q) = tmp_queue("offline-forever");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        for _ in 0..(MAX_ATTEMPTS * 3) {
            let api = FakeApi::scripted(&[("t1", "offline")]);
            q.flush(&api);
        }
        assert_eq!(q.len(), 1, "still queued after 30 offline passes");
        assert_eq!(q.head().unwrap().attempts, 0, "offline passes never count as attempts");
        // Connectivity returns → it sends.
        let api = FakeApi::scripted(&[("t1", "ok")]);
        let report = q.flush(&api);
        assert_eq!(report.sent, 1);
        assert!(q.is_empty());
    }

    #[test]
    fn write_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("aq-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = try_write_lock(&dir).unwrap();
        assert!(first.is_some(), "first lock succeeds");
        // flock contends on the open file description even within one process — models the
        // agenda timer firing while the planner app holds the session lock.
        let second = try_write_lock(&dir).unwrap();
        assert!(second.is_none(), "second is refused without blocking");
        drop(first);
        assert!(try_write_lock(&dir).unwrap().is_some(), "released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transient_server_4xx_retries_rather_than_parks() {
        // 408/409/423/425/429 answer transiently — retry (counting attempts), don't park.
        for code in [408u16, 409, 423, 425, 429] {
            match classify(sources::SourceError::Api(code, "x".into())) {
                WriteError::Retryable(_) => {}
                other => panic!("{code} should be Retryable, got {other:?}"),
            }
        }
        match classify(sources::SourceError::Api(404, "x".into())) {
            WriteError::Permanent(_) => {}
            other => panic!("404 should be Permanent, got {other:?}"),
        }
    }

    fn planner_with(tasks: Vec<Task>) -> PlannerState {
        PlannerState::build(
            Vec::<Event>::new(),
            tasks,
            (date(7, 1), date(7, 8)),
            date(7, 2),
            now(),
        )
    }
    fn task(id: &str, due: NaiveDate) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            priority: 2,
            due: Some(due),
            duration_min: 30,
            done: false,
            is_recurring: false,
        }
    }

    #[test]
    fn overlay_removes_completed_and_moves_deferred() {
        let (_, mut q) = tmp_queue("overlay");
        let mut state = planner_with(vec![task("done-me", date(7, 2)), task("move-me", date(7, 2))]);
        q.enqueue(Action::Complete { task_id: "done-me".into() }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "move-me".into(), due: date(7, 6) }, now()).unwrap();
        q.apply_overlay(&mut state);
        assert!(state.day(date(7, 2)).unwrap().tasks.is_empty());
        let moved = &state.day(date(7, 6)).unwrap().tasks;
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].id, "move-me");
        assert_eq!(moved[0].due, Some(date(7, 6)));
    }

    /// A queue file written by a device that still knew about Recalculate must load, and must not
    /// take the *other* actions down with it.
    ///
    /// This is why the variant survives its own retirement. `Queue::load` is
    /// `load_json(..).unwrap_or_default()`, so a `"kind"` serde cannot name fails the whole file and
    /// silently yields an empty queue — discarding completions the user made offline, with nothing
    /// logged. The flush then drains the stale entry without dialling out.
    #[test]
    fn a_queue_file_containing_a_retired_recalculate_still_loads_whole() {
        let (path, _) = tmp_queue("legacyrecalc");
        std::fs::write(
            &path,
            r#"{"next_id":3,"items":[
                {"id":0,"action":{"kind":"recalculate","task_id":"t1",
                    "url":"https://getdiarize.com/task/d-t1/recalculate"},
                 "created_at":"2026-08-11T10:00:00+01:00","attempts":0,"last_error":null},
                {"id":1,"action":{"kind":"complete","task_id":"t2"},
                 "created_at":"2026-08-11T10:00:01+01:00","attempts":0,"last_error":null}
            ]}"#,
        )
        .unwrap();

        let mut q = Queue::load(&path);
        assert_eq!(q.len(), 2, "the completion must survive alongside the retired action");

        let api = FakeApi::default();
        let report = q.flush(&api);
        assert_eq!(report.sent, 2, "both drain");
        assert_eq!(report.remaining, 0);
        assert_eq!(
            api.calls.borrow().as_slice(),
            ["complete:t2"],
            "the retired recalculate is dropped without any network call"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn delete_event_flushes_dedupes_and_survives_reload() {
        let (path, mut q) = tmp_queue("delevent");
        let de = |id: &str| Action::DeleteEvent { calendar_id: "primary".into(), event_id: id.into() };
        q.enqueue(de("ev1"), now()).unwrap();
        q.enqueue(de("ev1"), now()).unwrap(); // same event id dedupes (last wins)
        q.enqueue(de("ev2"), now()).unwrap();
        assert_eq!(q.len(), 2, "same-event deletes collapse");
        // Round-trips the file format.
        let mut q = Queue::load(&path);
        assert_eq!(q.len(), 2);
        // Flushes through the writer's delete_event.
        let api = FakeApi::default();
        let report = q.flush(&api);
        assert_eq!(report.sent, 2);
        let calls = api.calls.borrow();
        assert!(calls.iter().any(|c| c == "delete_event:ev1"));
        assert!(calls.iter().any(|c| c == "delete_event:ev2"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pending_event_delete_hides_the_refetched_event() {
        let (path, mut q) = tmp_queue("delevent-overlay");
        q.enqueue(Action::DeleteEvent { calendar_id: "primary".into(), event_id: "gone".into() }, now()).unwrap();
        let day = date(7, 2);
        let mut ev = diarize_block("x", day);
        ev.todoist_task = None;
        ev.id = Some("gone".into());
        let mut keep = diarize_block("y", day);
        keep.todoist_task = None;
        keep.id = Some("keep".into());
        let mut state = PlannerState { days: Default::default(), window: (day, day), fetched_at: now(), stale: false };
        state.days.insert(day, sb_core::DayAgenda { date: day, events: vec![ev, keep], tasks: vec![] });
        q.apply_overlay(&mut state);
        let evs = &state.days[&day].events;
        assert_eq!(evs.len(), 1, "the deleted event stays hidden until the queue drains");
        assert_eq!(evs[0].id.as_deref(), Some("keep"));
        let _ = std::fs::remove_file(&path);
    }

    fn diarize_block(todoist_id: &str, day: NaiveDate) -> Event {
        use chrono::Datelike;
        let start = chrono::Local.with_ymd_and_hms(day.year(), day.month(), day.day(), 9, 0, 0).unwrap();
        Event {
            title: todoist_id.into(),
            start,
            end: start + chrono::Duration::minutes(30),
            calendar_id: "primary".into(),
            id: Some(format!("gid-{todoist_id}")),
            description: String::new(),
            color_id: Some(11),
            todoist_task: Some(todoist_id.into()),
        }
    }

    /// The completed block must stay hidden for the *whole* handover, not just while the Todoist
    /// close is queued. A tap enqueues Complete **and** a DeleteEvent for the block; the Complete
    /// drains first, and if the mask depended on it alone the block would reappear on the next
    /// fetch and sit there until Diarized's next sync — which is exactly the lag this pairing
    /// exists to remove.
    #[test]
    fn a_completed_block_stays_hidden_after_the_complete_drains() {
        let (_, mut q) = tmp_queue("overlayhandover");
        let mut state = PlannerState::build(
            vec![diarize_block("done-me", date(7, 2))],
            vec![task("done-me", date(7, 2))],
            (date(7, 1), date(7, 8)),
            date(7, 2),
            now(),
        );
        q.enqueue(Action::Complete { task_id: "done-me".into() }, now()).unwrap();
        q.enqueue(
            Action::DeleteEvent { calendar_id: "primary".into(), event_id: "gid-done-me".into() },
            now(),
        )
        .unwrap();
        assert_eq!(q.len(), 2, "different channels — the DeleteEvent must not evict the Complete");

        // The Todoist close lands; only the calendar delete is still in flight.
        let head = q.head().unwrap();
        assert!(matches!(head.action, Action::Complete { .. }), "oldest-first");
        assert_eq!(q.resolve(head.id, Ok(())), Resolved::Sent);

        q.apply_overlay(&mut state);
        assert!(
            state.day(date(7, 2)).unwrap().events.is_empty(),
            "block still hidden by the pending DeleteEvent"
        );
    }

    #[test]
    fn overlay_complete_also_hides_diarize_block_but_defer_keeps_it() {
        let (_, mut q) = tmp_queue("overlayevents");
        let mut state = PlannerState::build(
            vec![diarize_block("done-me", date(7, 2)), diarize_block("deferred", date(7, 2))],
            vec![task("done-me", date(7, 2)), task("deferred", date(7, 2))],
            (date(7, 1), date(7, 8)),
            date(7, 2),
            now(),
        );
        q.enqueue(Action::Complete { task_id: "done-me".into() }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "deferred".into(), due: date(7, 6) }, now()).unwrap();
        q.apply_overlay(&mut state);
        let today = state.day(date(7, 2)).unwrap();
        assert_eq!(today.events.len(), 1, "completed block hidden");
        assert_eq!(today.events[0].todoist_task.as_deref(), Some("deferred"), "deferred block STAYS (Diarized moves it)");
    }

    #[test]
    fn overlay_defer_beyond_window_just_removes() {
        let (_, mut q) = tmp_queue("overlay2");
        let mut state = planner_with(vec![task("far", date(7, 2))]);
        q.enqueue(Action::Defer { task_id: "far".into(), due: date(8, 1) }, now()).unwrap();
        q.apply_overlay(&mut state);
        assert!(state.days.values().all(|d| d.tasks.is_empty()));
    }

    #[test]
    fn overlay_complete_on_absent_task_is_noop() {
        let (_, mut q) = tmp_queue("overlay3");
        let mut state = planner_with(vec![]);
        q.enqueue(Action::Complete { task_id: "already-gone".into() }, now()).unwrap();
        q.apply_overlay(&mut state); // must not panic
        assert!(state.days.values().all(|d| d.tasks.is_empty()));
    }

    #[test]
    fn apply_to_tasks_hides_completed_and_moved_tasks() {
        let (_, mut q) = tmp_queue("daytasks");
        let today = date(7, 2);
        let mut tasks = vec![task("done", today), task("moved", today), task("kept", today)];
        q.enqueue(Action::Complete { task_id: "done".into() }, now()).unwrap();
        q.enqueue(Action::Defer { task_id: "moved".into(), due: date(7, 6) }, now()).unwrap();
        q.apply_to_tasks(&mut tasks, today);
        let ids: Vec<_> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["kept"]);
    }

    #[test]
    fn head_resolve_cycle_matches_flush_semantics() {
        let (_, mut q) = tmp_queue("headresolve");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        let head = q.head().unwrap();
        assert_eq!(head.action.task_id(), "t1");
        // transient failure keeps it queued with an attempt recorded
        assert_eq!(q.resolve(head.id, Err(WriteError::Retryable("offline".into()))), Resolved::WillRetry);
        assert_eq!(q.items()[0].attempts, 1);
        // success removes it
        assert_eq!(q.resolve(head.id, Ok(())), Resolved::Sent);
        assert!(q.is_empty());
    }

    #[test]
    fn resolve_of_superseded_action_is_stale() {
        let (_, mut q) = tmp_queue("stale");
        q.enqueue(Action::Complete { task_id: "t1".into() }, now()).unwrap();
        let head = q.head().unwrap();
        // While the send is in flight, the user defers the same task (dedupe replaces it).
        q.enqueue(Action::Defer { task_id: "t1".into(), due: date(7, 3) }, now()).unwrap();
        assert_eq!(q.resolve(head.id, Ok(())), Resolved::Stale);
        assert_eq!(q.len(), 1, "the newer action must survive");
    }

    #[test]
    fn creates_stay_distinct_and_flush() {
        let (_, mut q) = tmp_queue("create");
        q.enqueue(Action::Create { temp_id: "tmp-0".into(), content: "Buy milk".into(), due: None, priority: Some(4) }, now()).unwrap();
        q.enqueue(Action::Create { temp_id: "tmp-1".into(), content: "Call Dave".into(), due: Some("today".into()), priority: None }, now()).unwrap();
        // unique temp_ids ⇒ distinct quick-adds coexist (not deduped like same-task writes)
        assert_eq!(q.len(), 2);
        let api = FakeApi::scripted(&[]);
        assert_eq!(q.flush(&api).sent, 2);
        assert_eq!(*api.calls.borrow(), vec!["create:Buy milk", "create:Call Dave"]);
    }

    #[test]
    fn create_overlay_is_noop() {
        let (_, mut q) = tmp_queue("createoverlay");
        let mut state = planner_with(vec![task("keep", date(7, 2))]);
        q.enqueue(Action::Create { temp_id: "tmp-0".into(), content: "New thing".into(), due: None, priority: None }, now()).unwrap();
        q.apply_overlay(&mut state); // must not touch existing tasks or panic
        assert_eq!(state.day(date(7, 2)).unwrap().tasks.len(), 1);
    }

    #[test]
    fn delete_flushes_and_overlay_removes() {
        let (_, mut q) = tmp_queue("delete");
        let mut state = planner_with(vec![task("del-me", date(7, 2))]);
        q.enqueue(Action::Delete { task_id: "del-me".into() }, now()).unwrap();
        q.apply_overlay(&mut state);
        assert!(state.days.values().all(|d| d.tasks.is_empty()), "deleted task removed from overlay");
        let api = FakeApi::scripted(&[]);
        assert_eq!(q.flush(&api).sent, 1);
        assert_eq!(*api.calls.borrow(), vec!["delete:del-me"]);
    }
}
