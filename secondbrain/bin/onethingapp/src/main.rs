//! `onethingapp` — "One Thing": a wall-clock focus timer bound to one deliberately-chosen Todoist
//! task per day. Pick the task, run a 25/45/60/90-minute session, mark it done when time's up.
//!
//! The countdown is anchored to an absolute end instant, so the device's aggressive suspend
//! doesn't lose time — waking mid-session shows the correct remaining, and a session that expired
//! while asleep opens on the "Time's up" screen. (No alarm can fire while suspended; by design.)
//!
//! Network I/O runs on a worker thread (agendaapp's pattern): task fetches over
//! `sources::todoist::fetch_all_tasks` + offline completions through the SHARED
//! `cache/actions.json` queue (safe: AppLoad runs one app at a time; the flush handles every
//! Action variant, so agenda-enqueued writes flush from here too).
//!
//! Mac usage:
//!   onethingapp --demo                                        # canned tasks, no network
//!   onethingapp --config <config.toml>                        # live Todoist
//!   onethingapp --demo --frames-dir out/ --script taps.txt    # headless PNG snapshots
//! Device usage: launched by AppLoad via deploy/appload/onething/external.manifest.json.

mod backend;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev;

use actionqueue::{Action, Queue, Resolved, TodoistApi, WriteError};
use anyhow::{Context, Result};
use backend::{Backend, BackendEvent, PngBackend};
use chrono::Local;
use onething::{hit_test, persist, update, view, Dirty, Effect, Event, Model};
use render::{draw::invert_rect, Canvas, Fonts};
use sb_core::cache;
use sb_core::creds::{spawn_self_update, Config, UpdateCheck, UpdateStatus};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// See agendaapp: an iteration far beyond the poll cadence means the device suspended — repaint
/// (re-asserts the surface + input routing); the wall-clock timer needs no fixing up.
const RESUME_GAP: Duration = Duration::from_secs(3);
const WORKER_TICK: Duration = Duration::from_secs(30);
const AUTO_REFETCH: Duration = Duration::from_secs(15 * 60);

static QUIT: AtomicBool = AtomicBool::new(false);

enum Cmd {
    Fetch,
    Nudge,
}

struct Args {
    config: PathBuf,
    demo: bool,
    frames_dir: Option<PathBuf>,
    script: Option<PathBuf>,
    width: u32,
    height: u32,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    install_sigterm_handler();

    let cfg = if args.demo { None } else { Config::load(&args.config).ok() };
    let update_check = cfg.as_ref().and_then(|c| spawn_self_update(c, &args.config));

    // Demo mode gets a scratch cache dir so it never touches the real queue/snapshot.
    let cache_dir = match &cfg {
        Some(c) => c.cache_dir(),
        None => std::env::temp_dir().join(format!("onething-demo-{}", std::process::id())),
    };
    std::fs::create_dir_all(&cache_dir).ok();
    let snapshot_path = cache_dir.join("onething.json");
    let tasks_cache = cache_dir.join("onething-tasks.json");

    let mut model = persist::seed(cache::load_json(&snapshot_path), Local::now());
    // Instant offline picker: last fetch's task list, until the worker refreshes it.
    if args.demo {
        model.tasks = demo_tasks();
    } else if let Some(tasks) = cache::load_json(&tasks_cache) {
        model.tasks = tasks;
    }

    let queue = Arc::new(Mutex::new(Queue::load(&cache_dir.join("actions.json"))));
    model.pending = queue.lock().unwrap().len();

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Event>();
    {
        let queue = queue.clone();
        let cfg = cfg.clone();
        let tasks_cache = tasks_cache.clone();
        std::thread::spawn(move || worker(cfg, queue, tasks_cache, evt_tx, cmd_rx));
    }
    if !args.demo {
        let _ = cmd_tx.send(Cmd::Fetch);
        model.fetching = true;
    }

    let canvas = Canvas { width: args.width, height: args.height };
    let mut backend = open_backend(&args, canvas)?;
    log::info!("backend: {} — {} cached tasks", backend.describe(), model.tasks.len());

    run_loop(&mut model, backend.as_mut(), canvas, &snapshot_path, &queue, &cmd_tx, &evt_rx, update_check)
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
    model: &mut Model,
    backend: &mut dyn Backend,
    canvas: Canvas,
    snapshot_path: &PathBuf,
    queue: &Arc<Mutex<Queue>>,
    cmd_tx: &Sender<Cmd>,
    evt_rx: &Receiver<Event>,
    mut update_check: Option<UpdateCheck>,
) -> Result<()> {
    let fonts = Fonts::embedded();
    let mut current = view(model, &fonts, canvas);
    backend.present(&current.frame, Dirty::Full)?;

    let mut iter_start = SystemTime::now();
    'outer: loop {
        let gap = iter_start.elapsed().unwrap_or_default();
        iter_start = SystemTime::now();
        if gap > RESUME_GAP {
            log::info!("resumed from suspend (~{}s) — repainting", gap.as_secs());
            // The Tick below moves the wall-clock timer (and lands on Done if it expired asleep);
            // present first so the surface + input routing revive immediately.
            current = view(model, &fonts, canvas);
            backend.present(&current.frame, Dirty::Full)?;
        }
        if QUIT.load(Ordering::Relaxed) {
            log::info!("SIGTERM — exiting");
            break;
        }

        let mut events: Vec<Event> = Vec::new();

        // Surface the background self-update's lifecycle once it settles.
        if let Some(check) = update_check.as_mut() {
            match check.poll() {
                UpdateStatus::Checking | UpdateStatus::InProgress => {}
                done => {
                    let msg = match done {
                        UpdateStatus::Updated(v) => format!("Updated to v{v} — reopen to apply"),
                        UpdateStatus::UpToDate(v) => format!("Up to date (v{v})"),
                        _ => "Update check failed".into(),
                    };
                    events.push(Event::UpdateChecked(msg));
                    update_check = None;
                }
            }
        }

        // Drain worker events (fetched tasks, queue-length changes).
        while let Ok(ev) = evt_rx.try_recv() {
            events.push(ev);
        }

        match backend.poll(POLL_TIMEOUT)? {
            BackendEvent::Quit => break 'outer,
            BackendEvent::Tap { x, y } => {
                if let Some((rect, _)) = hit_test(&current.hits, x, y) {
                    flash_invert(backend, &current.frame, *rect)?;
                }
                events.push(Event::Tap { x, y });
            }
            BackendEvent::None => {}
        }
        events.push(Event::Tick { now: Local::now() });

        let mut dirty: Option<Dirty> = None;
        for event in events {
            for effect in update(model, event, &current.hits, canvas) {
                match effect {
                    Effect::Persist => {
                        if let Err(e) = cache::save_json(snapshot_path, &persist::snapshot(model)) {
                            log::warn!("snapshot save failed: {e}");
                        }
                    }
                    Effect::Enqueue(action) => {
                        let n = {
                            let mut q = queue.lock().unwrap();
                            if let Err(e) = q.enqueue(action, Local::now()) {
                                log::warn!("enqueue failed: {e}");
                            }
                            q.len()
                        };
                        model.pending = n;
                        let _ = cmd_tx.send(Cmd::Nudge);
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
            current = view(model, &fonts, canvas);
            backend.present(&current.frame, d)?;
        }
    }
    Ok(())
}

/* ─────────────────────────── worker (network) ─────────────────────────── */

fn worker(cfg: Option<Config>, queue: Arc<Mutex<Queue>>, tasks_cache: PathBuf, tx: Sender<Event>, rx: Receiver<Cmd>) {
    let mut last_fetch: Option<Instant> = None;
    loop {
        let cmd = rx.recv_timeout(WORKER_TICK);
        if matches!(cmd, Err(RecvTimeoutError::Disconnected)) {
            return;
        }
        let Some(cfg) = cfg.as_ref() else { continue }; // demo mode: no I/O

        flush_queue(cfg, &queue, &tx);

        let fetch_now = matches!(cmd, Ok(Cmd::Fetch)) || last_fetch.map(|t| t.elapsed() > AUTO_REFETCH).unwrap_or(true);
        if fetch_now {
            let token = match cfg.todoist_token() {
                Ok(Some(t)) => t,
                _ => {
                    let _ = tx.send(Event::FetchFailed("no Todoist token".into()));
                    last_fetch = Some(Instant::now());
                    continue;
                }
            };
            match sources::todoist::fetch_all_tasks(&token) {
                Ok(tasks) => {
                    if let Err(e) = cache::save_json(&tasks_cache, &tasks) {
                        log::warn!("task cache save failed: {e}");
                    }
                    last_fetch = Some(Instant::now());
                    let _ = tx.send(Event::TasksFetched(tasks));
                }
                Err(e) => {
                    log::warn!("fetch failed: {e:#}");
                    last_fetch = Some(Instant::now()); // don't hammer while offline
                    let _ = tx.send(Event::FetchFailed("offline?".into()));
                }
            }
        }
    }
}

/// Drain the shared action queue head-by-head (agendaapp's pattern, verbatim match: the file may
/// hold agenda-enqueued Defer/Delete/Create/DeleteEvent actions too, and this app must flush them).
fn flush_queue(cfg: &Config, queue: &Arc<Mutex<Queue>>, tx: &Sender<Event>) {
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
        use actionqueue::TodoistWriter;
        let outcome: Result<(), WriteError> = match &pending.action {
            Action::Complete { task_id } => api.complete(task_id),
            Action::Defer { task_id, due } => api.defer(task_id, *due),
            Action::Delete { task_id } => api.delete(task_id),
            Action::Create { content, due, priority, .. } => api.create(content, due.as_deref(), *priority),
            Action::SetPriority { task_id, priority } => api.set_priority(task_id, *priority),
            Action::DeleteEvent { calendar_id, event_id } => api.delete_event(calendar_id, event_id),
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
        let _ = tx.send(Event::QueueChanged(remaining));
    }
}

/* ─────────────────────────── plumbing ─────────────────────────── */

/// Canned tasks for `--demo` / headless snapshots — no network, deterministic frames.
fn demo_tasks() -> Vec<sb_core::Task> {
    let d = |mo: u32, day: u32| chrono::NaiveDate::from_ymd_opt(2026, mo, day);
    let t = |id: &str, title: &str, priority: u8, due| sb_core::Task {
        id: id.into(),
        title: title.into(),
        priority,
        due,
        duration_min: 0,
        done: false,
        is_recurring: false,
    };
    vec![
        t("1", "Write the quarterly investor update", 4, d(7, 14)),
        t("2", "Book flights for the Lisbon trip", 3, d(7, 16)),
        t("3", "Renew home insurance", 2, d(7, 20)),
        t("4", "Read the AI safety paper", 1, None),
        t("5", "Fix the garden gate hinge", 1, None),
        t("6", "Plan Q3 hiring round", 3, d(7, 15)),
    ]
}

fn flash_invert(backend: &mut dyn Backend, frame: &image::RgbImage, rect: render::ui::Rect) -> Result<()> {
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
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
        demo: false,
        frames_dir: None,
        script: None,
        width: 954,
        height: 1696,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => a.config = it.next().expect("--config needs a path").into(),
            "--demo" => a.demo = true,
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
        "onethingapp — one focus session on one Todoist task (AppLoad external app)\n\n\
         USAGE:\n  \
         onethingapp [--config <config.toml>] [--demo]\n              \
         [--frames-dir <dir> [--script <taps.txt>]] [--width N] [--height N]\n\n  \
         --demo        canned tasks + scratch cache (no network, no config)\n  \
         --frames-dir  headless: write a PNG per frame instead of opening a window\n  \
         --script      with --frames-dir: replay taps, one `x,y` per line"
    );
}
