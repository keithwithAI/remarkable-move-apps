//! `agendaapp` — the interactive planner. A merged Google Calendar + Todoist day view with
//! tap-to-complete and quick-pick defer, launched from the AppLoad launcher on the device.
//!
//! Threads: the main thread owns the backend (frame presents + input polling) and the pure
//! `ui` state machine; a worker thread does all network I/O (fetch + action-queue flush), so a
//! tap is never blocked behind a slow request. Every enqueued action is persisted before the
//! tap handler returns — losing power loses nothing.
//!
//! Mac usage:
//!   agendaapp --demo                      # minifb window, sample data
//!   agendaapp --demo --frames-dir out/ --script taps.txt   # headless PNG snapshots
//! Device usage: launched by AppLoad via deploy/appload/agenda/external.manifest.json.

mod backend;
mod demo;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev;

use actionqueue::{Action, Queue, Resolved, TodoistApi, WriteError};
use anyhow::{Context, Result};
use backend::{Backend, BackendEvent, PngBackend};
use chrono::{DateTime, Duration as ChronoDuration, Local};
use render::{draw::invert_rect, Canvas, Fonts};
use sb_core::{cache, creds::{spawn_self_update, Config, UpdateCheck, UpdateStatus}, PlannerState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use ui::{hit_test, update, view, AppModel, Dirty, Effect, Rect, UiEvent};

/// Re-fetch this often while the app sits open (taps always work off local state).
const AUTO_REFETCH: Duration = Duration::from_secs(15 * 60);
/// Worker wake cadence for retrying queued actions.
const WORKER_TICK: Duration = Duration::from_secs(30);
const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// A loop iteration this much longer than the ~POLL_TIMEOUT cadence means the process was frozen —
/// the device suspended and resumed. On resume we force a repaint + refetch (see run_loop) to
/// re-assert the surface and wake on fresh data. Well above any legitimate single-iteration cost,
/// so we never trigger it spuriously.
const RESUME_GAP: Duration = Duration::from_secs(3);

static QUIT: AtomicBool = AtomicBool::new(false);

struct Args {
    demo: bool,
    config: PathBuf,
    frames_dir: Option<PathBuf>,
    script: Option<PathBuf>,
    width: u32,
    height: u32,
}

enum Cmd {
    Fetch,
    /// Something was enqueued — flush soon.
    Nudge,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    install_sigterm_handler();

    let now = Local::now();
    let cfg = if args.demo {
        None
    } else {
        Some(Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?)
    };

    // WiFi self-update: kick off a background check as soon as config is loaded. Applies next
    // launch; the returned handle is polled from the run loop to surface a status line.
    let update_check = cfg.as_ref().and_then(|c| spawn_self_update(c, &args.config));

    // Queue + caches live next to the config; demo mode keeps them in a scratch dir.
    let cache_dir = match &cfg {
        Some(c) => c.cache_dir(),
        None => {
            let dir = std::env::temp_dir().join("sb-agendaapp-demo");
            let _ = std::fs::remove_file(dir.join("actions.json")); // fresh badge each demo run
            dir
        }
    };
    // Hold the queue write-lock for the whole session: the agenda timer runs CONCURRENTLY with
    // this app (it's systemd, not AppLoad) and skips its flush while we hold this — we flush
    // ourselves every 30s. Brief contention (the timer mid-flush at our launch) gets a short
    // retry; if it still can't be taken, proceed unlocked with a warning (the timer's own lock
    // use still prevents the torn-file case).
    let mut queue_lock = None;
    for _ in 0..5 {
        match actionqueue::try_write_lock(&cache_dir) {
            Ok(Some(l)) => {
                queue_lock = Some(l);
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(200)),
            Err(e) => {
                log::warn!("queue lock: {e}");
                break;
            }
        }
    }
    let _queue_lock = queue_lock;
    if _queue_lock.is_none() {
        log::warn!("running without the queue write lock (timer may flush concurrently)");
    }
    let queue = Arc::new(Mutex::new(Queue::load(&cache_dir.join("actions.json"))));
    let planner_cache = cache_dir.join("planner.json");

    let initial = initial_planner(&args, &cfg, &queue, &planner_cache, now);
    let mut model = AppModel::new(initial, now);
    model.pending = queue.lock().unwrap().len();

    // Worker thread: all network I/O.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<UiEvent>();
    {
        let cfg = cfg.clone();
        let queue = Arc::clone(&queue);
        let planner_cache = planner_cache.clone();
        std::thread::spawn(move || worker(cfg, queue, planner_cache, evt_tx, cmd_rx));
    }
    if !args.demo {
        model.fetching = true;
        let _ = cmd_tx.send(Cmd::Fetch);
    }

    let canvas = Canvas { width: args.width, height: args.height };
    let mut backend = open_backend(&args, canvas)?;
    log::info!("backend: {}", backend.describe());

    run_loop(&mut model, backend.as_mut(), canvas, &queue, &cmd_tx, &evt_rx, update_check)
}

fn initial_planner(
    args: &Args,
    cfg: &Option<Config>,
    queue: &Arc<Mutex<Queue>>,
    planner_cache: &std::path::Path,
    now: DateTime<Local>,
) -> PlannerState {
    if args.demo {
        return demo::demo_planner(now);
    }
    let _ = cfg;
    match cache::load_planner(planner_cache) {
        Some(mut cached) => {
            queue.lock().unwrap().apply_overlay(&mut cached);
            cached
        }
        None => PlannerState::empty(now.date_naive(), now),
    }
}

fn open_backend(args: &Args, _canvas: Canvas) -> Result<Box<dyn Backend>> {
    if let Some(dir) = &args.frames_dir {
        let script = match &args.script {
            Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?),
            None => None,
        };
        return Ok(Box::new(PngBackend::new(dir.clone(), script.as_deref())?));
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(backend::QtfbBackend::connect()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(backend::SimBackend::open(_canvas.width, _canvas.height)?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("no interactive backend for this OS — use --frames-dir")
    }
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    model: &mut AppModel,
    backend: &mut dyn Backend,
    canvas: Canvas,
    queue: &Arc<Mutex<Queue>>,
    cmd_tx: &Sender<Cmd>,
    evt_rx: &Receiver<UiEvent>,
    mut update_check: Option<UpdateCheck>,
) -> Result<()> {
    let fonts = Fonts::embedded();
    // Self-update status flashes, driven by polling the background check each iteration.
    let mut update_announced = update_check.is_none();
    let mut update_progress_announced = false;
    let mut current = view(model, &fonts, canvas, model.now);
    backend.present(&current.frame, Dirty::Full)?;

    let mut iter_start = SystemTime::now();
    'outer: loop {
        // Suspend/resume: an iteration far longer than our poll cadence means the process was frozen
        // while the device slept. While idle the app never repaints (a Tick only redraws on flash
        // expiry or midnight), so across suspend the compositor sees a quiescent surface and stops
        // routing touch to our socket — waking, the app looks alive but ignores taps. Force a full
        // repaint to re-assert the surface (which appears to revive input routing) and refetch so we
        // wake on a live, up-to-date planner. No terminate, so AppLoad keeps us on screen. Wall-clock,
        // since a monotonic Instant may not count time spent suspended.
        let gap = iter_start.elapsed().unwrap_or_default();
        iter_start = SystemTime::now();
        if gap > RESUME_GAP {
            log::info!("resumed from suspend (~{}s) — repainting + refetching", gap.as_secs());
            model.fetching = true;
            let _ = cmd_tx.send(Cmd::Fetch);
            current = view(model, &fonts, canvas, Local::now());
            backend.present(&current.frame, Dirty::Full)?;
        }

        if QUIT.load(Ordering::Relaxed) {
            log::info!("SIGTERM — exiting (queue is already persisted)");
            break;
        }

        let mut events: Vec<UiEvent> = Vec::new();

        // Surface the background self-update's lifecycle as status flashes.
        if let Some(check) = update_check.as_mut() {
            if !update_announced {
                update_announced = true;
                events.push(UiEvent::UpdateChecked("Checking for updates…".into()));
            }
            match check.poll() {
                UpdateStatus::Checking => {}
                UpdateStatus::InProgress => {
                    if !update_progress_announced {
                        update_progress_announced = true;
                        events.push(UiEvent::UpdateChecked("Updating…".into()));
                    }
                }
                done => {
                    let msg = match done {
                        UpdateStatus::Updated(v) => format!("Updated to v{v} — reopen to apply"),
                        UpdateStatus::UpToDate(v) => format!("Up to date (v{v})"),
                        _ => "Update check failed — will retry later".into(),
                    };
                    events.push(UiEvent::UpdateChecked(msg));
                    update_check = None;
                }
            }
        }

        while let Ok(e) = evt_rx.try_recv() {
            events.push(e);
        }

        match backend.poll(POLL_TIMEOUT)? {
            BackendEvent::Quit => break 'outer,
            BackendEvent::Tap { x, y } => {
                // Instant feedback: invert the tapped control before any work happens.
                if let Some((rect, target)) = hit_test(&current.hits, x, y) {
                    if target.is_interactive() {
                        flash_invert(backend, &current.frame, *rect)?;
                    }
                }
                events.push(UiEvent::Tap { x, y });
            }
            BackendEvent::None => {}
        }
        events.push(UiEvent::Tick { now: Local::now() });

        let mut dirty: Option<Dirty> = None;
        for event in events {
            for effect in update(model, event, &current.hits, canvas) {
                match effect {
                    Effect::Enqueue(action) => {
                        enqueue(queue, action, model.now);
                        let _ = cmd_tx.send(Cmd::Nudge);
                        let n = queue.lock().unwrap().len();
                        for e in update(model, UiEvent::QueueChanged(n), &current.hits, canvas) {
                            if let Effect::Redraw(d) = e {
                                dirty = Some(merge_dirty(dirty, d));
                            }
                        }
                    }
                    Effect::RequestFetch => {
                        let _ = cmd_tx.send(Cmd::Fetch);
                    }
                    Effect::Redraw(d) => dirty = Some(merge_dirty(dirty, d)),
                    Effect::Exit => break 'outer,
                }
            }
        }

        if let Some(d) = dirty {
            current = view(model, &fonts, canvas, model.now);
            backend.present(&current.frame, d)?;
        }
    }
    Ok(())
}

fn enqueue(queue: &Arc<Mutex<Queue>>, action: Action, now: DateTime<Local>) {
    let mut q = queue.lock().unwrap();
    if let Err(e) = q.enqueue(action, now) {
        log::error!("could not persist action: {e}");
    }
}

fn flash_invert(backend: &mut dyn Backend, frame: &image::RgbImage, rect: Rect) -> Result<()> {
    let mut inverted = frame.clone();
    invert_rect(&mut inverted, rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    backend.present(&inverted, Dirty::Region(rect.x, rect.y, rect.w, rect.h))
}

fn merge_dirty(acc: Option<Dirty>, next: Dirty) -> Dirty {
    match (acc, next) {
        (None, d) => d,
        (Some(Dirty::Full), _) | (Some(_), Dirty::Full) => Dirty::Full,
        (Some(Dirty::Region(ax, ay, aw, ah)), Dirty::Region(bx, by, bw, bh)) => {
            let x0 = ax.min(bx);
            let y0 = ay.min(by);
            let x1 = (ax + aw).max(bx + bw);
            let y1 = (ay + ah).max(by + bh);
            Dirty::Region(x0, y0, x1 - x0, y1 - y0)
        }
    }
}

/* ─────────────────────────── worker (network) ─────────────────────────── */

fn worker(
    cfg: Option<Config>,
    queue: Arc<Mutex<Queue>>,
    planner_cache: PathBuf,
    tx: Sender<UiEvent>,
    rx: Receiver<Cmd>,
) {
    let mut last_fetch: Option<Instant> = None;
    loop {
        let cmd = rx.recv_timeout(WORKER_TICK);
        if matches!(cmd, Err(RecvTimeoutError::Disconnected)) {
            return;
        }
        let Some(cfg) = cfg.as_ref() else { continue }; // demo mode: no I/O

        // Recalculates ride the action queue like every other write, so the flush above is the
        // whole story — no separate synchronous round-trip to block the worker on.
        flush_queue(cfg, &queue, &tx);

        let fetch_now = matches!(cmd, Ok(Cmd::Fetch))
            || last_fetch.map(|t| t.elapsed() > AUTO_REFETCH).unwrap_or(true);
        if fetch_now {
            match fetch_planner(cfg, Local::now()) {
                Ok(mut planner) => {
                    if let Some(dir) = planner_cache.parent() {
                        std::fs::create_dir_all(dir).ok();
                    }
                    if let Err(e) = cache::save_planner(&planner_cache, &planner) {
                        log::warn!("planner cache save failed: {e}");
                    }
                    queue.lock().unwrap().apply_overlay(&mut planner);
                    last_fetch = Some(Instant::now());
                    let _ = tx.send(UiEvent::DataRefreshed(planner));
                }
                Err(e) => {
                    log::warn!("fetch failed: {e:#}");
                    last_fetch = Some(Instant::now()); // don't hammer while offline
                    let _ = tx.send(UiEvent::FetchFailed(String::new()));
                }
            }
        }
    }
}

/// Drain the action queue head-by-head, holding the lock only between network calls.
fn flush_queue(cfg: &Config, queue: &Arc<Mutex<Queue>>, tx: &Sender<UiEvent>) {
    if queue.lock().unwrap().is_empty() {
        return;
    }
    let token = match cfg.todoist_token() {
        Ok(Some(t)) => t,
        Ok(None) => {
            log::warn!("actions queued but no Todoist token configured");
            return;
        }
        Err(e) => {
            log::warn!("todoist token: {e}");
            return;
        }
    };
    let api = TodoistApi { token, service_account: Some(cfg.service_account_path()) };
    let mut sent = 0usize;
    loop {
        let Some(pending) = queue.lock().unwrap().head() else { break };
        let outcome: Result<(), WriteError> = match &pending.action {
            Action::Complete { task_id } => {
                use actionqueue::TodoistWriter;
                api.complete(task_id)
            }
            Action::Defer { task_id, due } => {
                use actionqueue::TodoistWriter;
                api.defer(task_id, *due)
            }
            Action::Delete { task_id } => {
                use actionqueue::TodoistWriter;
                api.delete(task_id)
            }
            Action::Create { content, due, priority, .. } => {
                use actionqueue::TodoistWriter;
                api.create(content, due.as_deref(), *priority)
            }
            Action::SetPriority { task_id, priority } => {
                use actionqueue::TodoistWriter;
                api.set_priority(task_id, *priority)
            }
            Action::DeleteEvent { calendar_id, event_id } => {
                use actionqueue::TodoistWriter;
                api.delete_event(calendar_id, event_id)
            }
            // Retired (see `actionqueue::Action::Recalculate`): drain a leftover, never send it.
            Action::Recalculate { .. } => Ok(()),
        };
        match queue.lock().unwrap().resolve(pending.id, outcome) {
            Resolved::Sent => sent += 1,
            Resolved::Parked | Resolved::Stale => {}
            Resolved::WillRetry => break,
        }
    }
    let remaining = queue.lock().unwrap().len();
    if sent > 0 || remaining == 0 {
        let _ = tx.send(UiEvent::QueueChanged(remaining));
    }
}

/// Fetch the full planner window. Any source failing is a hard error — showing a half-empty
/// planner silently would read as "nothing to do".
fn fetch_planner(cfg: &Config, now: DateTime<Local>) -> Result<PlannerState> {
    let today = now.date_naive();
    let from = today - ChronoDuration::days(cfg.planner.days_back as i64);
    let to = today + ChronoDuration::days(cfg.planner.days_ahead as i64);
    let (tmin, tmax) = sources::gcal::window_range(from, to);
    let events = sources::gcal::fetch_events(&cfg.service_account_path(), &cfg.calendars(), &tmin, &tmax, now)
        .context("google calendar")?;
    let tasks = match cfg.todoist_token()? {
        Some(tok) => sources::todoist::fetch_tasks(&tok, &cfg.planner.filter()).context("todoist")?,
        None => Vec::new(),
    };
    Ok(PlannerState::build(events, tasks, (from, to), today, now))
}

/* ─────────────────────────── plumbing ─────────────────────────── */

#[cfg(target_os = "linux")]
fn install_sigterm_handler() {
    extern "C" fn on_term(_: libc::c_int) {
        QUIT.store(true, Ordering::Relaxed);
    }
    let handler = on_term as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
    }
}

#[cfg(not(target_os = "linux"))]
fn install_sigterm_handler() {}

fn parse_args() -> Args {
    let mut a = Args {
        demo: false,
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
        frames_dir: None,
        script: None,
        width: 954,
        height: 1696,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--config" => a.config = it.next().expect("--config needs a path").into(),
            "--frames-dir" => a.frames_dir = it.next().map(Into::into),
            "--script" => a.script = it.next().map(Into::into),
            "--width" => a.width = it.next().and_then(|s| s.parse().ok()).unwrap_or(a.width),
            "--height" => a.height = it.next().and_then(|s| s.parse().ok()).unwrap_or(a.height),
            "-V" | "--version" => {
                println!(env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }
    a
}

fn print_help() {
    eprintln!(
        "agendaapp — interactive calendar + task planner (AppLoad external app)\n\n\
         USAGE:\n  \
         agendaapp [--config <config.toml>] [--demo]\n            \
         [--frames-dir <dir> [--script <taps.txt>]] [--width N] [--height N]\n\n  \
         --demo        sample data, no network or credentials\n  \
         --frames-dir  headless: write a PNG per frame instead of opening a window\n  \
         --script      with --frames-dir: replay taps, one `x,y` per line"
    );
}
