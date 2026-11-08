//! `reader-app` — the interactive Markdown reader/highlighter (AppLoad external app).
//!
//! Browse the vault, read paged clippings, and **pen-drag a passage** to highlight it (writes
//! Obsidian `==markers==` straight into the source `.md`, byte-identical to Obsidian). Touch drives
//! navigation; the pen drives selection — both read straight from the kernel input devices so input
//! survives suspend (see `evdev.rs`). File reads + highlight writes are fast, so they run inline on
//! the main thread; a highlight also nudges the sibling `vaultsync` for a prompt push.
//!
//! Mac:    reader-app --dir <clippings> [--sim | --frames-dir out/ --script s.txt]
//! Device: launched by AppLoad via deploy/appload/reader/external.manifest.json.

mod backend;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod evdev;
mod images;

use anyhow::{Context, Result};
use backend::{Backend, BackendEvent, PngBackend};
use image::RgbImage;
use reader::{
    clean_book_source, hit_test, layout_with_images, page_of_byte, parse, reading_masthead_height,
    set_status, set_tags, update, view_with_images,
    Block, Dirty, Doc, DocKind, LayoutOpts, Page, ReadStatus, ReaderEffect, ReaderEvent, ReaderModel,
    ReaderTarget, Screen, SortKey, StatusFilter, Tag,
};
use render::{draw::invert_rect, Canvas, Fonts};
use sb_core::creds::{spawn_self_update, Config, UpdateCheck, UpdateStatus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

const POLL_TIMEOUT: Duration = Duration::from_millis(120);
/// A loop iteration far longer than the poll cadence means the device suspended + resumed; repaint.
const RESUME_GAP: Duration = Duration::from_secs(3);

static QUIT: AtomicBool = AtomicBool::new(false);

struct Args {
    dir: PathBuf,
    frames_dir: Option<PathBuf>,
    script: Option<PathBuf>,
    sim: bool,
    width: u32,
    height: u32,
    prefs: Option<PathBuf>,
    config: PathBuf,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();
    install_sigterm_handler();

    // WiFi self-update: opening the reader kicks a detached background check (applies next launch),
    // matching the planner. Best-effort — no config (Mac/sim) or no `selfupdate` binary → no-op.
    // The returned handle is polled from the run loop to surface a status line.
    let update_check = match Config::load(&args.config) {
        Ok(cfg) => spawn_self_update(&cfg, &args.config),
        Err(_) => None,
    };

    let prefs_file = args.prefs.clone().unwrap_or_else(|| default_prefs_path(&args.dir));
    let prefs = load_prefs(&prefs_file);
    let files = reader::vault::scan(&args.dir, false);
    log::info!("vault {}: {} clippings", args.dir.display(), files.len());
    let mut model = ReaderModel::new(files);
    model.filter = filter_from_str(&prefs.filter);
    model.sort = sort_from_str(&prefs.sort);
    model.tag_filter = tagfilter_from_str(&prefs.tag_filter);
    // Only honour a saved author filter that some LIVE clipping still carries. The filter matches the
    // byline by exact string, so a byline that stops being derived — as "X" and "YouTube" did when
    // platforms stopped outranking authors — would otherwise open the library filtered to nothing,
    // with no visible cause. Mirrors `sort_from_str` falling back on an unrecognised value.
    //
    // "Live" (non-archived) rather than merely present: archiving every article by one author left
    // the filter naming a byline that `authors()` no longer knows, so the shelf stopped drawing the
    // row that would clear it — and this guard happily restored that state on every launch.
    model.author_filter = Some(prefs.author.clone())
        .filter(|a| !a.is_empty())
        .filter(|a| {
            let known = model.author_has_live_clippings(a);
            if !known {
                log::info!("dropping saved author filter {a:?} — no unarchived clipping carries that byline");
            }
            known
        });
    model.books = prefs.books;

    let canvas = Canvas { width: args.width, height: args.height };
    let mut backend = open_backend(&args)?;
    log::info!("backend: {}", backend.describe());

    run_loop(&mut model, backend.as_mut(), canvas, prefs.font_step, prefs_file, args.dir.clone(), update_check)
}

fn open_backend(args: &Args) -> Result<Box<dyn Backend>> {
    if let Some(dir) = &args.frames_dir {
        let script = match &args.script {
            Some(p) => Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?),
            None => None,
        };
        return Ok(Box::new(PngBackend::new(dir.clone(), script.as_deref())?));
    }
    let _ = args.sim;
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(backend::QtfbBackend::connect()?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(backend::SimBackend::open(args.width, args.height)?))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("no interactive backend for this OS — use --frames-dir")
    }
}

/// Height to reserve above page 1 for the reading masthead (title + byline·date), matching what
/// `view::draw_reading_masthead` renders. `content_w` tracks the body text column (`page_w − 2·margin_x`).
/// Returns 0 when there's no byline/date so a title-only masthead reserves just the title's rows.
fn masthead_extra(fonts: &Fonts, opts: &LayoutOpts, title: &str, byline: &Option<String>, published: &Option<String>) -> f32 {
    let has_byline = byline.as_deref().map_or(false, |s| !s.is_empty())
        || published.as_deref().map_or(false, |s| !s.is_empty());
    let content_w = opts.page_w as f32 - 2.0 * opts.margin_x;
    reading_masthead_height(fonts, title, has_byline, content_w) as f32
}

#[allow(clippy::too_many_arguments)]
fn run_loop(
    model: &mut ReaderModel,
    backend: &mut dyn Backend,
    canvas: Canvas,
    mut font_step: i32,
    prefs_file: PathBuf,
    vault_dir: PathBuf,
    mut update_check: Option<UpdateCheck>,
) -> Result<()> {
    let fonts = Fonts::embedded();
    let mut opts = LayoutOpts { scale: scale_for_step(font_step), ..LayoutOpts::default() };
    // Normal-mode content area: content_top (88) reserves the slim reading top chrome
    // (view::READING_HEADER_H = 80); the page-1 article masthead is reserved separately per-doc via
    // opts.first_page_extra. Focus mode swaps to near-full-screen margins and RE-PAGINATES — page
    // turns are continuous (no repeated lines) — with the current top line pinned as a page top via
    // opts.force_break_at, so toggling keeps the reader's place.
    // img_max_h below derives from the content area, so article images auto-fit the body column.
    let (base_top, base_bottom) = (opts.content_top, opts.content_bottom);
    let mut saved = current_prefs(font_step, model);
    // Article images, fetched over WiFi for the open doc, keyed by URL and pre-scaled to the text
    // column. Empty until a doc's images land (see the two-phase load at the top of the loop); the
    // reading view draws labelled placeholder boxes meanwhile. Bytes are disk-cached beside the vault.
    let cache_dir = prefs_file.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("cache"));
    let img_max_w = (opts.page_w as f32 - 2.0 * opts.margin_x) as u32;
    let img_max_h = (opts.content_bottom - opts.content_top - 40.0) as u32;
    let mut open_images: HashMap<String, RgbImage> = HashMap::new();
    let mut pending_images = false;
    // Background children polled each iteration: the self-update check (status flashes) and a
    // ↺-triggered vaultsync (feeds Synced + a vault rescan when it lands).
    let mut update_announced = update_check.is_none(); // "Checking…" flashed once, on the first tick
    let mut update_progress_announced = false;
    let mut sync_run: Option<std::process::Child> = None;
    let mut current = view_with_images(model, &fonts, canvas.width, canvas.height, &open_images);
    backend.present(&current.frame, Dirty::Full)?;

    let mut iter_start = SystemTime::now();
    // An input event pulled while coalescing a burst that must NOT act on the stale hit map (a
    // chrome tap / long-press): it opens the next iteration instead, against freshly rendered hits.
    let mut carry: Option<BackendEvent> = None;
    'outer: loop {
        // Suspend/resume: a long gap means the device slept; repaint to re-assert the surface.
        let gap = iter_start.elapsed().unwrap_or_default();
        iter_start = SystemTime::now();
        if gap > RESUME_GAP {
            log::info!("resumed from suspend (~{}s) — repainting", gap.as_secs());
            current = view_with_images(model, &fonts, canvas.width, canvas.height, &open_images);
            backend.present(&current.frame, Dirty::Full)?;
        }
        if QUIT.load(Ordering::Relaxed) {
            log::info!("SIGTERM — exiting");
            break;
        }

        // Collect this iteration's events: the backend input, then a Tick.
        let mut queue: VecDeque<ReaderEvent> = VecDeque::new();

        // Surface the background self-update's lifecycle as status flashes.
        if let Some(check) = update_check.as_mut() {
            if !update_announced {
                update_announced = true;
                queue.push_back(ReaderEvent::UpdateChecked { msg: "Checking for updates…".into() });
            }
            match check.poll() {
                UpdateStatus::Checking => {}
                UpdateStatus::InProgress => {
                    if !update_progress_announced {
                        update_progress_announced = true;
                        queue.push_back(ReaderEvent::UpdateChecked { msg: "Updating…".into() });
                    }
                }
                done => {
                    let msg = match done {
                        UpdateStatus::Updated(v) => format!("Updated to v{v} — reopen to apply"),
                        UpdateStatus::UpToDate(v) => format!("Up to date (v{v})"),
                        _ => "Update check failed — will retry later".into(),
                    };
                    queue.push_back(ReaderEvent::UpdateChecked { msg });
                    update_check = None;
                }
            }
        }

        // A ↺-triggered vaultsync finished: rescan the vault (new clippings appear) + flash the outcome.
        if let Some(child) = sync_run.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let ok = status.success();
                if ok {
                    let files = reader::vault::scan(&vault_dir, false);
                    log::info!("post-sync rescan: {} clippings", files.len());
                    queue.push_back(ReaderEvent::VaultRescanned { files });
                }
                queue.push_back(ReaderEvent::Synced { ok });
                sync_run = None;
            }
        }

        // Two-phase image load: a doc opens with placeholder boxes (already on screen); now fetch +
        // prepare its images over WiFi (blocking — fine, placeholders are up), then re-lay-out so the
        // pages size to the real images, and repaint via a Relaid event.
        if pending_images {
            pending_images = false;
            if let Screen::Reading(r) = &model.screen {
                let urls = image_urls(&r.doc);
                if !urls.is_empty() {
                    log::info!("fetching {} image(s)…", urls.len());
                    open_images = images::load(&cache_dir, &urls, img_max_w, img_max_h);
                    log::info!("prepared {}/{} image(s)", open_images.len(), urls.len());
                    if !open_images.is_empty() {
                        if let Some((pages, page_ix)) = relayout_current(model, &fonts, opts, &open_images) {
                            queue.push_back(ReaderEvent::Relaid { pages, page_ix });
                        }
                    }
                }
            }
        }
        // Intake: the carried-over event from the last iteration (it needed this fresh hit map) or a
        // blocking poll — then coalesce any burst already queued behind a slow render. Nav-safe events
        // (page turns, swipes, pen strokes — none consult the hit map) fold into ONE iteration, so an
        // impatient page-flick becomes one net page move + one render + one e-ink refresh instead of
        // painting every intermediate page. Anything hit-map-sensitive (a chrome tap, a long-press)
        // ends the drain and carries over. Sim/headless backends report no pending input, keeping
        // strict frame-per-event semantics.
        let mut batch: Vec<BackendEvent> = vec![match carry.take() {
            Some(ev) => ev,
            None => backend.poll(POLL_TIMEOUT)?,
        }];
        while carry.is_none() && backend.has_pending() {
            let ev = backend.poll(Duration::ZERO)?;
            let nav_safe = match ev {
                BackendEvent::None => break,
                BackendEvent::Quit | BackendEvent::Swipe { .. } => true,
                BackendEvent::PenDown { .. } | BackendEvent::PenMove { .. } | BackendEvent::PenUp { .. } => true,
                BackendEvent::Tap { x, y } => matches!(
                    hit_test(&current.hits, x, y),
                    None | Some((_, ReaderTarget::PrevPage | ReaderTarget::NextPage | ReaderTarget::ToggleFocus))
                ),
                BackendEvent::LongPress { .. } => false,
            };
            if nav_safe {
                batch.push(ev);
            } else {
                carry = Some(ev);
            }
        }
        let mut flashed: Option<reader::Rect> = None;
        for ev in batch {
            match ev {
                BackendEvent::Quit => break 'outer,
                BackendEvent::Tap { x, y } => {
                    // Flash the tapped target for touch feedback — but skip the full-height body zones (the
                    // centre focus toggle and the left/right side-tap page-turn thirds), whose rects span the
                    // whole middle of the page; inverting a third of the screen is a jarring band, and the
                    // page turn / chrome reflow is unmistakable feedback on its own. Real controls (chips,
                    // keys, list rows) are all short, so a height threshold separates them cleanly.
                    // …and skip a highlight: it's transparent to a quick tap (its menu is a long-press), so
                    // flashing it would imply an action the tap won't take. Same no-flash as plain-text taps.
                    // (In a coalesced burst only the opening event can be a short control — nav-safe taps
                    // are full-height thirds — so at most one flash fires per iteration.)
                    if let Some(rect) = match hit_test(&current.hits, x, y) {
                        Some((rect, t)) if rect.h <= 200 && !matches!(t, ReaderTarget::TapHighlight { .. }) => Some(*rect),
                        _ => None,
                    } {
                        flash_invert(backend, &mut current.frame, rect)?;
                        flashed = Some(rect);
                    }
                    queue.push_back(ReaderEvent::Tap { x, y });
                }
                BackendEvent::LongPress { x, y } => queue.push_back(ReaderEvent::LongPress { x, y }),
                BackendEvent::PenDown { x, y } => queue.push_back(ReaderEvent::PenDown { x, y }),
                BackendEvent::PenMove { x, y } => queue.push_back(ReaderEvent::PenMove { x, y }),
                BackendEvent::PenUp { x, y } => queue.push_back(ReaderEvent::PenUp { x, y }),
                BackendEvent::Swipe { x, y, dir } => queue.push_back(ReaderEvent::Swipe { x, y, dir }),
                BackendEvent::None => {}
            }
        }
        queue.push_back(ReaderEvent::Tick);

        let mut dirty: Option<Dirty> = None;
        // A page-turn auto-status write, held until after this iteration's present (see below).
        let mut deferred_status: Option<(usize, ReadStatus)> = None;
        while let Some(event) = queue.pop_front() {
            for effect in update(model, event, &current.hits, &fonts, canvas) {
                match effect {
                    ReaderEffect::Redraw(d) => dirty = Some(merge_dirty(dirty, d)),
                    ReaderEffect::Exit => break 'outer,
                    ReaderEffect::SyncVault => {
                        // ↺ refresh: run vaultsync in the background; the poll at the top of the loop
                        // feeds Synced + a rescan when it lands. Ignored while one is already running
                        // (the "Syncing…" flash is already up).
                        if sync_run.is_none() {
                            sync_run = spawn_vaultsync();
                            if sync_run.is_none() {
                                model.flash("Sync unavailable");
                                dirty = Some(Dirty::Full);
                            }
                        }
                    }
                    ReaderEffect::Open(ix) => {
                        // New doc → drop the old doc's images and queue a fresh fetch; it renders with
                        // placeholder boxes first, then swaps in the images once they land.
                        open_images = HashMap::new();
                        pending_images = true;
                        // A newly opened doc starts unfocused — restore the normal content area and
                        // drop any position anchor left by a previous doc's focus session.
                        opts.content_top = base_top;
                        opts.content_bottom = base_bottom;
                        opts.force_break_at = None;
                        // Reserve the page-1 masthead space for this doc's title/byline·date.
                        opts.first_page_extra = model
                            .vault
                            .get(ix)
                            .map(|vf| masthead_extra(&fonts, &opts, &vf.title, &vf.byline, &vf.published))
                            .unwrap_or(0.0);
                        match load_file(model, ix, &fonts, opts, &open_images) {
                            Ok((source, doc, pages)) => queue.push_back(ReaderEvent::Opened { file_ix: ix, source, doc, pages }),
                            Err(e) => {
                                log::warn!("open failed: {e:#}");
                                model.busy = false;
                                model.flash("Couldn't open file");
                                dirty = Some(Dirty::Full);
                            }
                        }
                    }
                    ReaderEffect::Highlight { path, range, add } => {
                        match write_highlight(model, &path, range.clone(), add, &fonts, &mut opts, &open_images) {
                            Ok((source, doc, pages)) => {
                                nudge_sync();
                                queue.push_back(ReaderEvent::Rehighlighted { source, doc, pages, added: add });
                            }
                            Err(e) => {
                                log::warn!("highlight write failed: {e:#}");
                                model.busy = false;
                                model.flash("Highlight failed");
                                dirty = Some(Dirty::Full);
                            }
                        }
                    }
                    ReaderEffect::AddNote { path, anchor, text } => {
                        match write_note(model, &path, anchor, &text, &fonts, &mut opts, &open_images) {
                            Ok((source, doc, pages)) => {
                                nudge_sync();
                                queue.push_back(ReaderEvent::NoteAdded { source, doc, pages });
                            }
                            Err(e) => {
                                log::warn!("note write failed: {e:#}");
                                model.busy = false;
                                model.flash("Note failed");
                                dirty = Some(Dirty::Full);
                            }
                        }
                    }
                    ReaderEffect::Rescale { delta } => {
                        font_step = (font_step + delta).clamp(FONT_STEP_MIN, FONT_STEP_MAX);
                        opts.scale = scale_for_step(font_step);
                        match relayout_current(model, &fonts, opts, &open_images) {
                            Some((pages, page_ix)) => queue.push_back(ReaderEvent::Relaid { pages, page_ix }),
                            None => model.busy = false,
                        }
                    }
                    ReaderEffect::FocusPaginate { anchor, forward } => {
                        // The first in-focus page turn: switch to focus's own near-full-screen grid,
                        // with a forced page break at `anchor` — forward, the anchored page starts at
                        // the first byte the entry screen did NOT show; backward, the page BEFORE the
                        // anchor ends exactly at the first byte it did. Continuous, no repeats. The
                        // bottom inset (56) structurally reserves the "N min left" corner so body
                        // text can never collide with the label. FOCUS_TOP is hand-matched to the
                        // view's focus masthead constant, like content_top/READING_HEADER_H.
                        const FOCUS_TOP: f32 = 14.0;
                        opts.content_top = FOCUS_TOP;
                        opts.content_bottom = canvas.height as f32 - 56.0;
                        opts.force_break_at = Some(anchor);
                        if let Screen::Reading(r) = &model.screen {
                            let pages = layout_with_images(&r.doc, &fonts, opts, &open_images);
                            // The forced break guarantees a page boundary at the first content
                            // at-or-after `anchor`. Forward anchors are END offsets (the byte after
                            // the last visible line), so `page_of_byte` (starts at-or-BEFORE) would
                            // land one page early — find the anchored page directly. (Mirrored by
                            // the transition tests in update.rs.)
                            let k = pages
                                .iter()
                                .position(|p| p.runs().map(|g| g.src.start).min().unwrap_or(usize::MAX) >= anchor)
                                .unwrap_or(pages.len().saturating_sub(1));
                            let page_ix = if forward { k } else { k.saturating_sub(1) };
                            queue.push_back(ReaderEvent::Relaid { pages, page_ix });
                        }
                    }
                    ReaderEffect::RestoreLayout { anchor } => {
                        // Leaving focus after in-focus turns: back to the normal content area, pinned
                        // so the current top line stays the page top (one clean full-refresh reanchor).
                        opts.content_top = base_top;
                        opts.content_bottom = base_bottom;
                        opts.force_break_at = Some(anchor);
                        if let Some((pages, page_ix)) = relayout_current(model, &fonts, opts, &open_images) {
                            queue.push_back(ReaderEvent::Relaid { pages, page_ix });
                        }
                    }
                    // A page-turn auto-advance is deferred: the file write + doc relayout run AFTER the
                    // turn has painted, so a tap never waits on I/O. Last-wins dedupe — a burst crossing
                    // page 2 and the last page collapses to the final (highest) status.
                    ReaderEffect::SetStatus { file_ix, status, quiet: true } => deferred_status = Some((file_ix, status)),
                    ReaderEffect::SetStatus { file_ix, status, quiet: false } => match write_status(model, file_ix, status, &fonts, &mut opts, &open_images) {
                        // The open file changed: re-laid-out, refresh the reading view.
                        Ok(Some((source, doc, pages))) => {
                            nudge_sync();
                            queue.push_back(ReaderEvent::StatusSet { source, doc, pages, status, quiet: false });
                        }
                        // A list row changed (file wasn't open): just update its marker.
                        Ok(None) => {
                            nudge_sync();
                            queue.push_back(ReaderEvent::StatusMarked { file_ix, status, quiet: false });
                        }
                        Err(e) => {
                            log::warn!("status write failed: {e:#}");
                            model.busy = false;
                            model.flash("Status write failed");
                            dirty = Some(Dirty::Full);
                        }
                    },
                    ReaderEffect::SetTags { file_ix, tags } => match write_tags(model, file_ix, &tags, &fonts, &mut opts, &open_images) {
                        // The open file changed: re-laid-out, refresh the reading view (panel stays open).
                        Ok(Some((source, doc, pages))) => {
                            nudge_sync();
                            queue.push_back(ReaderEvent::TagsSet { source, doc, pages, tags });
                        }
                        // A file changed while not open: just update its list tags.
                        Ok(None) => {
                            nudge_sync();
                            queue.push_back(ReaderEvent::TagsMarked { file_ix, tags });
                        }
                        Err(e) => {
                            log::warn!("tags write failed: {e:#}");
                            model.busy = false;
                            model.flash("Tag write failed");
                            dirty = Some(Dirty::Full);
                        }
                    },
                }
            }
        }

        // Did the redraw we're about to present repaint over the tap-flash? A Full always does; a
        // Region only if it contains the flashed rect. The search keyboard is deliberately excluded
        // from the per-keystroke repaint, so a flashed key falls through and must be cleared below.
        let flash_covered = match (&dirty, flashed) {
            (Some(d), Some(rect)) => region_contains(d, rect),
            _ => false,
        };
        if let Some(d) = dirty {
            current = view_with_images(model, &fonts, canvas.width, canvas.height, &open_images);
            backend.present(&current.frame, d)?;
            // Persist text size / filter / sort whenever they change (only writes on a real change).
            let now = current_prefs(font_step, model);
            if now != saved {
                save_prefs(&prefs_file, &now);
                saved = now;
            }
        }
        // Clear a tap-flash the redraw didn't paint over, so it doesn't stay inverted. In practice this
        // is the search keyboard, whose keys sit below the keystroke repaint region; `current.frame`
        // holds the key in its normal state, so re-presenting its rect restores it.
        if let (Some(rect), false) = (flashed, flash_covered) {
            backend.present(&current.frame, Dirty::Region(rect.x, rect.y, rect.w, rect.h))?;
        }
        // Deferred page-turn auto-status: the page already painted above, so the file write + doc
        // relayout no longer sit between the tap and the refresh. The quiet install repaints nothing
        // (the re-laid pages are pixel-identical — only frontmatter bytes moved), but the hit map is
        // rebuilt: its TapHighlight byte ranges must track the shifted source offsets.
        if let Some((file_ix, status)) = deferred_status {
            match write_status(model, file_ix, status, &fonts, &mut opts, &open_images) {
                Ok(Some((source, doc, pages))) => {
                    nudge_sync();
                    let eff = update(model, ReaderEvent::StatusSet { source, doc, pages, status, quiet: true }, &current.hits, &fonts, canvas);
                    debug_assert!(eff.is_empty(), "a quiet status install must not redraw");
                    current = view_with_images(model, &fonts, canvas.width, canvas.height, &open_images);
                }
                Ok(None) => {
                    nudge_sync();
                    let _ = update(model, ReaderEvent::StatusMarked { file_ix, status, quiet: true }, &current.hits, &fonts, canvas);
                }
                Err(e) => {
                    // Rare: surface the failure with its own (second) refresh — correctness over calm.
                    log::warn!("status write failed: {e:#}");
                    model.busy = false;
                    model.flash("Status write failed");
                    current = view_with_images(model, &fonts, canvas.width, canvas.height, &open_images);
                    backend.present(&current.frame, Dirty::Full)?;
                }
            }
        }
    }
    Ok(())
}

/// Read + parse + lay out `vault[ix]`. A Book (Readwise Kindle export) is first cleaned into prose
/// ([`clean_book_source`]) — safe because books are read-only: the cleaned text is only ever
/// displayed, never written back.
fn load_file(model: &ReaderModel, ix: usize, fonts: &Fonts, opts: LayoutOpts, images: &HashMap<String, RgbImage>) -> Result<(String, Doc, Vec<Page>)> {
    let vf = model.vault.get(ix).context("file index out of range")?;
    let raw = std::fs::read_to_string(&vf.path).with_context(|| format!("reading {}", vf.path.display()))?;
    let source = if vf.kind == DocKind::Book { clean_book_source(&raw) } else { raw };
    let (doc, pages) = lay_out(&source, fonts, opts, images);
    Ok((source, doc, pages))
}

/// Distinct absolute image URLs referenced by `doc`, in document order — the fetch list for a clipping.
fn image_urls(doc: &Doc) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    doc.blocks
        .iter()
        .filter_map(|b| match b {
            Block::Image { url, .. } => Some(url.clone()),
            _ => None,
        })
        .filter(|u| u.starts_with("http") && seen.insert(u.clone()))
        .collect()
}

/// Apply/remove the highlight over `range` in the currently-open source, write it back, and re-lay-out.
/// Write `bytes` to `path` atomically: to a dot-prefixed temp in the same directory, then rename.
/// The device suspends aggressively — a direct overwrite interrupted mid-write truncates the
/// article, which vaultsync would then commit and push. The dot prefix keeps the temp invisible to
/// Obsidian and to `vault::scan` (which only picks up `.md`). Same temp+rename pattern as
/// `sb_core::cache::save_json`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

/// Shift the position anchor (`opts.force_break_at`) for a source edit starting at `edit_start`
/// that changed the total length by `delta`, so later relayouts keep pinning the same line. Edits
/// at-or-after the anchor leave it untouched.
fn shift_anchor(opts: &mut LayoutOpts, edit_start: usize, delta: i64) {
    if let Some(b) = opts.force_break_at {
        if edit_start < b {
            opts.force_break_at = Some((b as i64 + delta).max(0) as usize);
        }
    }
}

fn write_highlight(model: &ReaderModel, path: &Path, range: std::ops::Range<usize>, add: bool, fonts: &Fonts, opts: &mut LayoutOpts, images: &HashMap<String, RgbImage>) -> Result<(String, Doc, Vec<Page>)> {
    let source = match &model.screen {
        Screen::Reading(r) => r.source.clone(),
        _ => anyhow::bail!("no open document"),
    };
    let new = if add {
        highlight::apply_highlight(&source, range.start, range.end).context("apply highlight")?
    } else {
        // Delete drops the highlight AND any note it carries (its `[^n]` ref + definition).
        highlight::remove_highlight_and_note(&source, range.start, range.end).context("remove highlight")?
    };
    write_atomic(path, new.as_bytes())?;
    // All marker insertions/removals happen at-or-after the selection start (a note's EOF definition
    // included), so the anchor shifts exactly when the edit precedes it.
    shift_anchor(opts, range.start, new.len() as i64 - source.len() as i64);
    let (doc, pages) = lay_out(&new, fonts, *opts, images);
    Ok((new, doc, pages))
}

/// Attach, edit, or clear a note on the highlight enclosing `anchor` in the open document (splice/replace
/// `[^n]` + a `[^n]: …` definition; empty `text` drops it), write it, and re-lay-out.
fn write_note(model: &ReaderModel, path: &Path, anchor: usize, text: &str, fonts: &Fonts, opts: &mut LayoutOpts, images: &HashMap<String, RgbImage>) -> Result<(String, Doc, Vec<Page>)> {
    let source = match &model.screen {
        Screen::Reading(r) => r.source.clone(),
        _ => anyhow::bail!("no open document"),
    };
    let new = highlight::upsert_note(&source, anchor, text).context("upsert note")?;
    write_atomic(path, new.as_bytes())?;
    // A note edit touches two spots: a few `[^n]` bytes at the highlight and the definition at the
    // END of the file. The definition never precedes a page top and the ref is sub-line-sized, so
    // leaving the anchor untouched is within a couple of bytes of exact.
    let (doc, pages) = lay_out(&new, fonts, *opts, images);
    Ok((new, doc, pages))
}

fn lay_out(source: &str, fonts: &Fonts, opts: LayoutOpts, images: &HashMap<String, RgbImage>) -> (Doc, Vec<Page>) {
    let doc = parse(source);
    let pages = layout_with_images(&doc, fonts, opts, images);
    (doc, pages)
}

/// Re-lay-out the open document at the current `opts`, holding the reading position (maps the current
/// page's top source byte through the new pagination). `None` if nothing is open.
fn relayout_current(model: &ReaderModel, fonts: &Fonts, opts: LayoutOpts, images: &HashMap<String, RgbImage>) -> Option<(Vec<Page>, usize)> {
    let Screen::Reading(r) = &model.screen else {
        return None;
    };
    let top = r.pages.get(r.page_ix).and_then(|p| p.runs().map(|g| g.src.start).min()).unwrap_or(0);
    let pages = layout_with_images(&r.doc, fonts, opts, images);
    let page_ix = page_of_byte(&pages, top);
    Some((pages, page_ix))
}

/// Write `status` into `vault[file_ix]`'s front matter (byte-preserving elsewhere). Sources the text
/// from the open document when it's the current file (and returns the re-laid-out `Some(...)`), else
/// reads/writes it on disk and returns `None` — a list-row change that needn't touch any open view.
fn write_status(model: &ReaderModel, file_ix: usize, status: ReadStatus, fonts: &Fonts, opts: &mut LayoutOpts, images: &HashMap<String, RgbImage>) -> Result<Option<(String, Doc, Vec<Page>)>> {
    let path = model.vault.get(file_ix).context("status: file index out of range")?.path.clone();
    let open = matches!(&model.screen, Screen::Reading(r) if r.file_ix == file_ix);
    let source = match &model.screen {
        Screen::Reading(r) if open => r.source.clone(),
        _ => std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    };
    let new = set_status(&source, status.as_frontmatter());
    write_atomic(&path, new.as_bytes())?;
    if open {
        // A frontmatter edit sits at the top of the file — always before the anchor.
        shift_anchor(opts, 0, new.len() as i64 - source.len() as i64);
        let (doc, pages) = lay_out(&new, fonts, *opts, images);
        Ok(Some((new, doc, pages)))
    } else {
        Ok(None)
    }
}

/// Write the managed `tags` into `vault[file_ix]`'s front matter, MERGING over any non-managed
/// Obsidian tags already there so they survive the write. The managed set (canonical order) is
/// written ahead of the preserved tags. Sources text from the open document when it's the current
/// file (returns the re-laid-out `Some(...)`), else reads/writes on disk and returns `None`.
fn write_tags(model: &ReaderModel, file_ix: usize, tags: &[Tag], fonts: &Fonts, opts: &mut LayoutOpts, images: &HashMap<String, RgbImage>) -> Result<Option<(String, Doc, Vec<Page>)>> {
    let path = model.vault.get(file_ix).context("tags: file index out of range")?.path.clone();
    let open = matches!(&model.screen, Screen::Reading(r) if r.file_ix == file_ix);
    let source = match &model.screen {
        Screen::Reading(r) if open => r.source.clone(),
        _ => std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    };
    // Keep any tag the reader doesn't manage; write the selected managed tags ahead of them.
    let non_managed: Vec<String> =
        parse::frontmatter(&source).tags.into_iter().filter(|t| Tag::from_slug(t).is_none()).collect();
    let mut slugs: Vec<String> = tags.iter().map(|t| t.slug().to_string()).collect();
    slugs.extend(non_managed);
    let slug_refs: Vec<&str> = slugs.iter().map(String::as_str).collect();
    let new = set_tags(&source, &slug_refs);
    write_atomic(&path, new.as_bytes())?;
    if open {
        // A frontmatter edit sits at the top of the file — always before the anchor.
        shift_anchor(opts, 0, new.len() as i64 - source.len() as i64);
        let (doc, pages) = lay_out(&new, fonts, *opts, images);
        Ok(Some((new, doc, pages)))
    } else {
        Ok(None)
    }
}

// ---- Reader preferences: text size, list filter + sort. Persisted beside the vault (a sibling
//      `cache/` dir), NOT inside it, so nothing extra syncs to Obsidian. ----

const FONT_STEP_MIN: i32 = -2;
const FONT_STEP_MAX: i32 = 5;

/// The body scale for a font step: a 1.38× baseline (one A- step below the old 1.5× default) plus 12%
/// per step, so −2 ≈ 1.14× (≈43px) … 0 = 1.38× (≈52px, the default) … +5 ≈ 1.98× (≈75px).
fn scale_for_step(step: i32) -> f32 {
    1.38 + 0.12 * step.clamp(FONT_STEP_MIN, FONT_STEP_MAX) as f32
}

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
struct Prefs {
    #[serde(default)]
    font_step: i32,
    #[serde(default)]
    filter: String,
    #[serde(default)]
    sort: String,
    #[serde(default)]
    tag_filter: String,
    /// The active author filter (the byline string), or `""` for all authors.
    #[serde(default)]
    author: String,
    /// The Books collection was active (the read-only Kindle-highlight library).
    #[serde(default)]
    books: bool,
}

fn current_prefs(font_step: i32, model: &ReaderModel) -> Prefs {
    Prefs {
        font_step,
        filter: filter_to_str(model.filter).into(),
        sort: sort_to_str(model.sort).into(),
        tag_filter: tagfilter_to_str(model.tag_filter).into(),
        author: model.author_filter.clone().unwrap_or_default(),
        books: model.books,
    }
}

fn default_prefs_path(dir: &Path) -> PathBuf {
    dir.parent().map(|p| p.join("cache")).unwrap_or_else(|| dir.to_path_buf()).join("reader-prefs.json")
}

fn load_prefs(path: &Path) -> Prefs {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_prefs(path: &Path, prefs: &Prefs) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(prefs) {
        Ok(s) => {
            if let Err(e) = write_atomic(path, s.as_bytes()) {
                log::warn!("prefs save failed: {e}");
            }
        }
        Err(e) => log::warn!("prefs encode failed: {e}"),
    }
}

fn filter_from_str(s: &str) -> StatusFilter {
    match s {
        "notread" => StatusFilter::NotRead,
        "reading" => StatusFilter::Reading,
        "read" => StatusFilter::Read,
        _ => StatusFilter::All,
    }
}
fn filter_to_str(f: StatusFilter) -> &'static str {
    match f {
        StatusFilter::All => "all",
        StatusFilter::NotRead => "notread",
        StatusFilter::Reading => "reading",
        StatusFilter::Read => "read",
    }
}
fn sort_from_str(s: &str) -> SortKey {
    match s {
        "recent" => SortKey::Recent,
        "added" => SortKey::Added,
        _ => SortKey::Title, // unknown (e.g. from a newer version) falls back safely
    }
}
fn sort_to_str(s: SortKey) -> &'static str {
    match s {
        SortKey::Recent => "recent",
        SortKey::Added => "added",
        SortKey::Title => "title",
    }
}
/// The persisted tag filter is the managed tag's slug (`ai`/…), or `""` for "all tags".
fn tagfilter_from_str(s: &str) -> Option<Tag> {
    Tag::from_slug(s)
}
fn tagfilter_to_str(t: Option<Tag>) -> &'static str {
    match t {
        Some(tag) => tag.slug(),
        None => "",
    }
}

/// Fire-and-forget a prompt vault sync after a highlight (the 20-min timer covers the rest).
fn nudge_sync() {
    if spawn_vaultsync().is_some() {
        log::info!("nudged vaultsync");
    }
}

/// Spawn the sibling `vaultsync` binary detached, returning the child so a caller can poll its
/// completion (the ↺ refresh does; the highlight nudge drops it). `None` if the binary is absent
/// (e.g. on the Mac) or the spawn fails.
fn spawn_vaultsync() -> Option<std::process::Child> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))?;
    let vaultsync = exe_dir.join("vaultsync");
    if !vaultsync.exists() {
        return None;
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
        Ok(child) => Some(child),
        Err(e) => {
            log::warn!("vaultsync spawn failed: {e}");
            None
        }
    }
}

fn flash_invert(backend: &mut dyn Backend, frame: &mut RgbImage, rect: reader::Rect) -> Result<()> {
    // Invert in place, present just the rect, then restore — no full-frame clone on the tap path
    // (inversion is an involution, so the second call undoes the first exactly).
    invert_rect(frame, rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    let res = backend.present(frame, Dirty::Region(rect.x, rect.y, rect.w, rect.h));
    invert_rect(frame, rect.x as f32, rect.y as f32, rect.w as f32, rect.h as f32);
    res
}

/// Does presenting `d` fully repaint `r`? A Full refresh always does; a Region does only if it
/// geometrically contains `r`. Used to decide whether a tap-flash still needs clearing.
fn region_contains(d: &Dirty, r: reader::Rect) -> bool {
    match d {
        Dirty::Full => true,
        Dirty::Region(x, y, w, h) => *x <= r.x && *y <= r.y && *x + *w >= r.x + r.w && *y + *h >= r.y + r.h,
    }
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
        dir: PathBuf::from("/home/root/secondbrain/vault"),
        frames_dir: None,
        script: None,
        sim: false,
        width: 954,
        height: 1696,
        prefs: None,
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dir" => a.dir = it.next().expect("--dir needs a path").into(),
            "--frames-dir" => a.frames_dir = it.next().map(Into::into),
            "--script" => a.script = it.next().map(Into::into),
            "--prefs" => a.prefs = it.next().map(Into::into),
            "--config" => a.config = it.next().expect("--config needs a path").into(),
            "--sim" => a.sim = true,
            "--width" => a.width = it.next().and_then(|s| s.parse().ok()).unwrap_or(a.width),
            "--height" => a.height = it.next().and_then(|s| s.parse().ok()).unwrap_or(a.height),
            "-V" | "--version" => {
                println!(env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                eprintln!("reader-app — Markdown reader/highlighter (AppLoad external app)\n\nUSAGE:\n  reader-app [--dir <vault>] [--prefs <file>] [--config <config.toml>] [--sim] [--frames-dir <dir> [--script <s.txt>]] [--width N] [--height N]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}
