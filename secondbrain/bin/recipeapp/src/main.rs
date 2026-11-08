//! `recipeapp` — the Obsidian-backed recipe browser. A filterable library (cuisine / tags /
//! quick / rating) over one `.md` per recipe, full paged reading via the reader crate's
//! renderer, and on-device star ratings written back into the note's frontmatter.
//!
//! Mac usage:
//!   recipeapp --dir /path/to/Recipes                        # minifb window
//!   recipeapp --dir … --frames-dir out/ --script taps.txt   # headless PNG snapshots
//! Device usage: launched by AppLoad via deploy/appload/recipes/external.manifest.json.

mod backend;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev;

use anyhow::{Context, Result};
use backend::{Backend, BackendEvent, PngBackend};
use recipe::{hit_test, update, view, Dirty, RecipeEffect, RecipeEvent, RecipeModel};
use render::{draw::invert_rect, Canvas, Fonts};
use sb_core::creds::{spawn_self_update, Config, UpdateCheck, UpdateStatus};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// See agendaapp: a long-frozen iteration means suspend/resume — repaint + rescan.
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

    let update_check = Config::load(&args.config).ok().and_then(|cfg| spawn_self_update(&cfg, &args.config));

    let mut model = RecipeModel::new(recipe::scan(&args.dir));
    let canvas = Canvas { width: args.width, height: args.height };
    let mut backend = open_backend(&args, canvas)?;
    log::info!("backend: {} — {} recipes from {}", backend.describe(), model.recipes.len(), args.dir.display());

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
    model: &mut RecipeModel,
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
            let _ = update(model, RecipeEvent::Rescanned(recipe::scan(dir)), &current.hits);
            current = view(model, &fonts, canvas);
            backend.present(&current.frame, Dirty::Full)?;
        }
        if QUIT.load(Ordering::Relaxed) {
            log::info!("SIGTERM — exiting");
            break;
        }

        let mut events: Vec<RecipeEvent> = Vec::new();

        if let Some(check) = update_check.as_mut() {
            match check.poll() {
                UpdateStatus::Checking | UpdateStatus::InProgress => {}
                done => {
                    let msg = match done {
                        UpdateStatus::Updated(v) => format!("Updated to v{v} — reopen to apply"),
                        UpdateStatus::UpToDate(v) => format!("Up to date (v{v})"),
                        _ => "Update check failed".into(),
                    };
                    model.flash = Some((msg, 12));
                    update_check = None;
                }
            }
        }

        match backend.poll(POLL_TIMEOUT)? {
            BackendEvent::Quit => break 'outer,
            BackendEvent::Tap { x, y } => {
                if let Some((rect, _)) = hit_test(&current.hits, x, y) {
                    flash_invert(backend, &current.frame, *rect)?;
                }
                events.push(RecipeEvent::Tap { x, y });
            }
            BackendEvent::None => {}
        }
        events.push(RecipeEvent::Tick);

        let mut dirty: Option<Dirty> = None;
        for event in events {
            for effect in update(model, event, &current.hits) {
                match effect {
                    RecipeEffect::LoadDoc { ix } => {
                        let Some(path) = model.recipes.get(ix).map(|r| r.path.clone()) else {
                            continue;
                        };
                        match std::fs::read_to_string(&path) {
                            Ok(source) => {
                                let doc = reader::parse(&source);
                                let opts = reader::LayoutOpts {
                                    content_top: recipe::READ_TOP,
                                    content_bottom: recipe::READ_BOTTOM,
                                    first_page_extra: recipe::READ_FIRST_EXTRA,
                                    ..Default::default()
                                };
                                let pages = reader::layout(&doc, &fonts, opts);
                                for e2 in update(model, RecipeEvent::DocLoaded { ix, doc, pages }, &current.hits) {
                                    if let RecipeEffect::Redraw(d) = e2 {
                                        dirty = Some(merge_dirty(dirty, d));
                                    }
                                }
                            }
                            Err(e) => log::warn!("open failed for {}: {e:#}", path.display()),
                        }
                    }
                    RecipeEffect::WriteRating { path, rating } => {
                        // Read fresh (a sync pull may have raced us), set the key, write atomically.
                        let write = std::fs::read_to_string(&path)
                            .map(|src| sb_core::fm::set_scalar(&src, "rating", &rating.to_string()))
                            .and_then(|new| write_atomic(&path, new.as_bytes()));
                        if let Err(e) = write {
                            log::warn!("rating write failed: {e:#}");
                            for e2 in update(model, RecipeEvent::Rescanned(recipe::scan(dir)), &current.hits) {
                                if let RecipeEffect::Redraw(d) = e2 {
                                    dirty = Some(merge_dirty(dirty, d));
                                }
                            }
                        }
                    }
                    RecipeEffect::Sync => nudge_sync(),
                    RecipeEffect::Rescan => {
                        for e2 in update(model, RecipeEvent::Rescanned(recipe::scan(dir)), &current.hits) {
                            if let RecipeEffect::Redraw(d) = e2 {
                                dirty = Some(merge_dirty(dirty, d));
                            }
                        }
                    }
                    RecipeEffect::Redraw(d) => dirty = Some(merge_dirty(dirty, d)),
                    RecipeEffect::Exit => break 'outer,
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

/// Write `bytes` to `path` atomically (dot-prefixed temp + rename; see the reader's write path).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Spawn the sibling `vaultsync` binary detached — fire-and-forget git push after a rating write.
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
        dir: PathBuf::from("/home/root/secondbrain/vault/Recipes"),
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
        "recipeapp — Obsidian-backed recipe browser (AppLoad external app)\n\n\
         USAGE:\n  \
         recipeapp [--dir <Recipes dir>] [--config <config.toml>]\n            \
         [--frames-dir <dir> [--script <taps.txt>]] [--width N] [--height N]\n\n  \
         --dir         the synced vault's Recipes folder (one .md per recipe)\n  \
         --frames-dir  headless: write a PNG per frame instead of opening a window\n  \
         --script      with --frames-dir: replay taps, one `x,y` per line"
    );
}
