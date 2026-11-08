//! `habitapp` — the interactive habit tracker. One Obsidian note per habit (frontmatter defines
//! it; completions are `- [x] YYYY-MM-DD` log lines the app appends), a today list with
//! tap-to-check + streaks, and a per-habit month heatmap. Launched from AppLoad.
//!
//! Single-threaded: file writes are tiny local edits (read fresh → toggle a line → atomic write)
//! followed by a detached vaultsync spawn — a tap never blocks on the network.
//!
//! Mac usage:
//!   habitapp --dir /path/to/Habits                          # minifb window
//!   habitapp --dir … --frames-dir out/ --script taps.txt    # headless PNG snapshots
//! Device usage: launched by AppLoad via deploy/appload/habits/external.manifest.json.

mod backend;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev;

use anyhow::{Context, Result};
use backend::{Backend, BackendEvent, PngBackend};
use chrono::Local;
use habit::{hit_test, update, view, Dirty, HabitEffect, HabitEvent, HabitModel};
use render::{draw::invert_rect, Canvas, Fonts};
use sb_core::creds::{spawn_self_update, Config, UpdateCheck, UpdateStatus};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// See agendaapp: an iteration far beyond the poll cadence means the device suspended — repaint
/// (re-asserts the surface + input routing) and rescan (the vault may have synced while asleep).
const RESUME_GAP: Duration = Duration::from_secs(3);

static QUIT: AtomicBool = AtomicBool::new(false);

struct Args {
    dir: PathBuf,
    config: PathBuf,
    frames_dir: Option<PathBuf>,
    script: Option<PathBuf>,
    width: u32,
    height: u32,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    install_sigterm_handler();

    // Background self-update check (fire-and-forget; missing config just skips it).
    let update_check = Config::load(&args.config).ok().and_then(|cfg| spawn_self_update(&cfg, &args.config));

    let mut model = HabitModel::new(habit::scan(&args.dir), Local::now());
    let canvas = Canvas { width: args.width, height: args.height };
    let mut backend = open_backend(&args, canvas)?;
    log::info!("backend: {} — {} habits from {}", backend.describe(), model.habits.len(), args.dir.display());

    run_loop(&mut model, backend.as_mut(), canvas, &args.dir, update_check)
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

fn run_loop(
    model: &mut HabitModel,
    backend: &mut dyn Backend,
    canvas: Canvas,
    dir: &Path,
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
            log::info!("resumed from suspend (~{}s) — rescanning + repainting", gap.as_secs());
            let habits = habit::scan(dir);
            let _ = update(model, HabitEvent::Rescanned(habits), &current.hits);
            model.now = Local::now();
            current = view(model, &fonts, canvas);
            backend.present(&current.frame, Dirty::Full)?;
        }
        if QUIT.load(Ordering::Relaxed) {
            log::info!("SIGTERM — exiting");
            break;
        }

        let mut events: Vec<HabitEvent> = Vec::new();

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
                    model.flash = Some(habit::model::Flash { text: msg, expires_at: Local::now() + chrono::Duration::seconds(3) });
                    update_check = None;
                }
            }
        }

        match backend.poll(POLL_TIMEOUT)? {
            BackendEvent::Quit => break 'outer,
            BackendEvent::Tap { x, y } => {
                // Instant feedback: invert the tapped control before any work happens.
                if let Some((rect, _)) = hit_test(&current.hits, x, y) {
                    flash_invert(backend, &current.frame, *rect)?;
                }
                events.push(HabitEvent::Tap { x, y });
            }
            BackendEvent::None => {}
        }
        events.push(HabitEvent::Tick { now: Local::now() });

        let mut dirty: Option<Dirty> = None;
        for event in events {
            for effect in update(model, event, &current.hits) {
                match effect {
                    HabitEffect::WriteToggle { path, date, on } => {
                        // Read fresh (a sync pull may have raced us), edit, write atomically.
                        let write = std::fs::read_to_string(&path)
                            .map(|src| habit::toggle_log(&src, date, on))
                            .and_then(|new| write_atomic(&path, new.as_bytes()));
                        if let Err(e) = write {
                            log::warn!("toggle write failed: {e:#}");
                            // Heal the optimistic UI from disk truth.
                            let habits = habit::scan(dir);
                            for e2 in update(model, HabitEvent::Rescanned(habits), &current.hits) {
                                if let HabitEffect::Redraw(d) = e2 {
                                    dirty = Some(merge_dirty(dirty, d));
                                }
                            }
                        }
                    }
                    HabitEffect::Sync => nudge_sync(),
                    HabitEffect::Rescan => {
                        let habits = habit::scan(dir);
                        for e2 in update(model, HabitEvent::Rescanned(habits), &current.hits) {
                            if let HabitEffect::Redraw(d) = e2 {
                                dirty = Some(merge_dirty(dirty, d));
                            }
                        }
                    }
                    HabitEffect::Redraw(d) => dirty = Some(merge_dirty(dirty, d)),
                    HabitEffect::Exit => break 'outer,
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

/// Write `bytes` to `path` atomically (dot-prefixed temp + rename — survives aggressive suspend;
/// invisible to Obsidian and the scans). Same pattern as the reader's highlight writes.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Spawn the sibling `vaultsync` binary detached — fire-and-forget git push after a log edit.
fn nudge_sync() {
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return;
    };
    let vaultsync = exe_dir.join("vaultsync");
    if !vaultsync.exists() {
        return;
    }
    let mut cmd = std::process::Command::new(&vaultsync);
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        Ok(_) => log::info!("nudged vaultsync"),
        Err(e) => log::warn!("vaultsync spawn failed: {e}"),
    }
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
        dir: PathBuf::from("/home/root/secondbrain/vault/Habits"),
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
        frames_dir: None,
        script: None,
        width: 954,
        height: 1696,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => a.dir = it.next().expect("--dir needs a path").into(),
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
        "habitapp — Obsidian-backed habit tracker (AppLoad external app)\n\n\
         USAGE:\n  \
         habitapp [--dir <Habits dir>] [--config <config.toml>]\n           \
         [--frames-dir <dir> [--script <taps.txt>]] [--width N] [--height N]\n\n  \
         --dir         the synced vault's Habits folder (one .md per habit)\n  \
         --frames-dir  headless: write a PNG per frame instead of opening a window\n  \
         --script      with --frames-dir: replay taps, one `x,y` per line"
    );
}
