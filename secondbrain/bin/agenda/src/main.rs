//! `agenda` — Phase 1 + 4A. Fetch today's Google Calendar events + Todoist tasks, render a
//! 1696×954 greyscale PNG, and install it as the suspend screen. Offline-safe via the cache.
//!
//! Also the planner app's safety net: each timer run flushes the shared offline action queue
//! (so a task completed on the device eventually reaches Todoist even if the app never
//! reopens) and overlays still-pending actions so the sleep screen never resurrects them.
//!
//! On the Mac, run with `--demo --out preview.png` to render sample data and preview the layout
//! without any network or credentials.

use actionqueue::{Queue, TodoistApi};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use device::{ConfigSuspend, MockSuspendScreen, SuspendScreen};
use render::{Canvas, Fonts};
use sb_core::{cache, creds::Config, AgendaState, Event, Task};
use std::path::PathBuf;

struct Args {
    demo: bool,
    config: PathBuf,
    out: Option<PathBuf>,
    cache: Option<PathBuf>,
    width: u32,
    height: u32,
    /// Skip the network fetch when the cache is younger than this many seconds (0 = always fetch).
    max_age: u64,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    let now = Local::now();
    let fonts = Fonts::embedded();
    let canvas = Canvas { width: args.width, height: args.height };

    let state = if args.demo { demo_state(now) } else { build_state(&args, now)? };
    let img = render::render_agenda(&state, canvas, &fonts);

    let screen: Box<dyn SuspendScreen> = match &args.out {
        Some(p) => Box::new(MockSuspendScreen { out: p.clone() }),
        None => Box::new(ConfigSuspend {
            image_path: PathBuf::from("/home/root/secondbrain/agenda.png"),
            xochitl_conf: PathBuf::from("/home/root/.config/remarkable/xochitl.conf"),
        }),
    };
    screen.install(&img).context("installing suspend screen")?;

    println!(
        "agenda {}: now={}, upcoming={}, tasks={}{} → {}",
        state.date,
        state.now.is_some() as u8,
        state.upcoming.len(),
        state.tasks.len(),
        if state.stale { " (STALE/cached)" } else { "" },
        screen.describe(),
    );
    Ok(())
}

fn build_state(args: &Args, now: DateTime<Local>) -> Result<AgendaState> {
    let cfg = Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    let cache_path = args
        .cache
        .clone()
        .unwrap_or_else(|| cfg.base_dir.join("cache").join("agenda.json"));

    let mut queue = Queue::load(&cfg.cache_dir().join("actions.json"));

    // Throttle: when invoked with --max-age (the pre-sleep hook), if the cache is younger than
    // that, render straight from it with no network at all. This keeps the device's frequent
    // auto-sleeps from hammering the Google/Todoist APIs — a real fetch still happens on the timer
    // and on the first sleep after the cache goes stale. Queued actions stay applied locally.
    if args.max_age > 0 && cache_fresh(&cache_path, args.max_age) {
        if let Some(mut state) = cache::load(&cache_path) {
            state.stale = false; // recent-by-design cache, not an offline failure
            queue.apply_to_tasks(&mut state.tasks, state.date);
            log::info!("cache fresh (< {}s); rendering offline, skipping fetch", args.max_age);
            return Ok(state);
        }
    }

    // Retry the planner's queued actions first, so the fetch below sees their results. Queue
    // WRITES need the inter-process lock: the planner app holds it for its whole session, and
    // when it does, we skip the flush (the planner flushes itself every 30s) and reload the
    // queue fresh under our lock otherwise, so we never save a stale snapshot over a
    // just-enqueued action.
    if !queue.is_empty() {
        match actionqueue::try_write_lock(&cfg.cache_dir()) {
            Ok(Some(_lock)) => {
                queue = Queue::load(&cfg.cache_dir().join("actions.json"));
                match cfg.todoist_token() {
                    Ok(Some(token)) => {
                        let report = queue
                            .flush(&TodoistApi { token, service_account: Some(cfg.service_account_path()) });
                        log::info!(
                            "action queue: {} sent, {} parked, {} still pending",
                            report.sent,
                            report.parked,
                            report.remaining
                        );
                    }
                    _ => log::warn!("{} queued actions but no Todoist token", queue.len()),
                }
            }
            Ok(None) => {
                log::info!("planner app holds the queue lock — skipping flush (it flushes itself)")
            }
            Err(e) => log::warn!("queue lock unavailable ({e}) — skipping flush"),
        }
    }

    let mut state = match fetch(&cfg, now) {
        Ok(state) => {
            if let Some(p) = cache_path.parent() {
                std::fs::create_dir_all(p).ok();
            }
            if let Err(e) = cache::save(&cache_path, &state) {
                log::warn!("cache save failed: {e}");
            }
            state
        }
        Err(e) => {
            log::warn!("fetch failed ({e:#}); falling back to cache {}", cache_path.display());
            cache::load(&cache_path).unwrap_or_else(|| AgendaState::build(vec![], vec![], now))
        }
    };
    // Anything still pending stays applied optimistically (never resurrect a completed task).
    queue.apply_to_tasks(&mut state.tasks, state.date);
    Ok(state)
}

/// True if `path` exists and was modified less than `max_age_secs` ago — lets the pre-sleep hook
/// skip the network fetch when the cached agenda is still recent.
fn cache_fresh(path: &std::path::Path, max_age_secs: u64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age.as_secs() < max_age_secs)
        .unwrap_or(false)
}

fn fetch(cfg: &Config, now: DateTime<Local>) -> Result<AgendaState> {
    let (tmin, tmax) = sources::gcal::today_range(now);
    let events = sources::gcal::fetch_events(&cfg.service_account_path(), &cfg.calendars(), &tmin, &tmax, now)
        .context("google calendar")?;
    let tasks = match cfg.todoist_token()? {
        Some(tok) => sources::todoist::fetch_tasks(&tok, &cfg.todoist.filter).unwrap_or_else(|e| {
            log::warn!("todoist fetch failed: {e}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    Ok(AgendaState::build(events, tasks, now))
}

fn demo_state(now: DateTime<Local>) -> AgendaState {
    use chrono::TimeZone;
    let base = now.date_naive();
    let at = |h: u32, m: u32| -> DateTime<Local> {
        Local
            .from_local_datetime(&base.and_hms_opt(h, m, 0).unwrap())
            .earliest()
            .unwrap_or(now)
    };
    let ev = |t: &str, sh: u32, sm: u32, eh: u32, em: u32, cal: &str| Event {
        title: t.into(),
        start: at(sh, sm),
        end: at(eh, em),
        calendar_id: cal.into(),
        id: None,
        description: String::new(),
        color_id: None,
        todoist_task: None,
    };
    // Diarized-scheduled task blocks appear as colored events with a Todoist link.
    let block = |id: &str, t: &str, color: u8, sh: u32, sm: u32, eh: u32, em: u32| Event {
        color_id: Some(color),
        todoist_task: Some(id.into()),
        ..ev(t, sh, sm, eh, em, "work")
    };
    let events = vec![
        ev("Morning pages", 7, 30, 8, 0, "personal"),
        ev("Team standup", 9, 30, 9, 45, "work"),
        ev("Design review — reMarkable second brain", 11, 0, 12, 0, "work"),
        ev("Lunch with Sam", 12, 30, 13, 30, "personal"),
        block("1", "Finish the agenda renderer", 11, 14, 0, 15, 0),
        block("3", "Draft Phase 3 AppLoad spike plan", 6, 15, 15, 16, 0),
        block("5", "Water the plants", 5, 16, 30, 16, 45),
        ev("Pick up the device", 17, 30, 18, 0, "personal"),
    ];
    let task = |id: &str, t: &str, p: u8, recurring: bool| Task {
        id: id.into(),
        title: t.into(),
        priority: p,
        due: Some(base),
        duration_min: 30,
        done: false,
        is_recurring: recurring,
    };
    let tasks = vec![
        task("1", "Finish the agenda renderer", 4, false),
        task("2", "Port reading-highlighter == rules", 4, false),
        task("3", "Draft Phase 3 AppLoad spike plan", 3, false),
        task("4", "Reply to the newsletter thread", 2, false),
        task("5", "Water the plants", 2, true),
    ];
    AgendaState::build(events, tasks, at(11, 20))
}

fn parse_args() -> Args {
    let mut a = Args {
        demo: false,
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
        out: None,
        cache: None,
        // The Move's portrait panel; suspend orientation still [verify on device] — override
        // with --width/--height if the lock screen turns out to want landscape.
        width: 954,
        height: 1696,
        max_age: 0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--config" => a.config = it.next().expect("--config needs a path").into(),
            "--out" => a.out = it.next().map(Into::into),
            "--cache" => a.cache = it.next().map(Into::into),
            "--max-age" => a.max_age = it.next().and_then(|s| s.parse().ok()).unwrap_or(a.max_age),
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
        "agenda — render today's calendar + tasks and install as the suspend screen\n\n\
         USAGE:\n  \
         agenda [--config <config.toml>] [--out <preview.png>] [--cache <agenda.json>]\n         \
         [--width N] [--height N] [--max-age SECS] [--demo]\n\n  \
         --demo      render sample data (no network, no config)\n  \
         --out       write the PNG to a file instead of installing as the suspend screen\n  \
         --max-age   skip the fetch if the cache is younger than SECS (0 = always fetch)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_fresh_respects_max_age() {
        let dir = std::env::temp_dir().join(format!("agenda-fresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("agenda.json");
        std::fs::write(&f, "{}").unwrap();
        // A just-written cache is fresh under a 60s window; max-age 0 disables the fast path;
        // a missing file is never fresh.
        assert!(cache_fresh(&f, 60), "a just-written cache should be fresh");
        assert!(!cache_fresh(&f, 0), "max_age 0 always fetches");
        assert!(!cache_fresh(&dir.join("missing.json"), 60));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
