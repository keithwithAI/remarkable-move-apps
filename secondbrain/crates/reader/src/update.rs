//! The reader's pure state transition: `(model, event) → effects`, mutating the model in place.
//!
//! Taps drive navigation (via the [`HitMap`] the view produced); pen down/move/up drive
//! selection (via [`crate::layout::page_hit`], which needs the fonts to measure within a run).
//! Highlight writes and file reads are returned as [`ReaderEffect`]s for the runtime to perform.

use crate::layout::page_hit;
use crate::model::{Dirty, HighlightMenu, NoteEntry, ReadStatus, Reading, ReaderEffect, ReaderEvent, ReaderModel, Screen, Selection, StatusFilter, Tag};
use crate::view::{authors_rows_per_page, list_rows_per_page, page_minutes, search_rows_per_page, ReaderTarget, Rect, SEARCH_KB_H};
use render::ui::KbLayer;
use crate::Page;
use render::{Canvas, Fonts};
use std::ops::Range;

/// A pen press whose release lands within this many px of where it went down is treated as a *tap*
/// (activates the UI under it, exactly like a touch) rather than a drag → highlight. Kept below the
/// touch tap slop: the stylus is precise, so a deliberate short-word drag (~14px) must highlight
/// while a footer-button tap (sub-12px) still activates the button and never smears a highlight.
const MIN_DRAG_PX: i32 = 12;

/// A long-press opens the highlight menu for a highlight whose run box contains the press horizontally
/// (± this) and lies within [`HL_LONGPRESS_SLOP_Y`] of it vertically — so pressing a thin, one-line
/// highlight is forgiving. Long-press no longer competes with the nav thirds (a quick tap navigates),
/// so both tolerances can be generous: pressing the indent before a line or just past a short line's
/// end still counts as "on the highlight".
const HL_LONGPRESS_SLOP_X: i32 = 48;
const HL_LONGPRESS_SLOP_Y: i32 = 48;

pub fn update(
    model: &mut ReaderModel,
    event: ReaderEvent,
    hits: &[(Rect, ReaderTarget)],
    fonts: &Fonts,
    canvas: Canvas,
) -> Vec<ReaderEffect> {
    match event {
        ReaderEvent::Tap { x, y } => on_tap(model, x, y, hits, canvas),
        ReaderEvent::LongPress { x, y } => on_long_press(model, x, y, hits, canvas),
        ReaderEvent::PenDown { x, y } => on_pen_down(model, x, y, fonts),
        ReaderEvent::PenMove { x, y } => on_pen_move(model, x, y, fonts),
        ReaderEvent::PenUp { x, y } => on_pen_up(model, x, y, hits, fonts, canvas),
        ReaderEvent::Swipe { x, dir, .. } => on_swipe(model, x, dir, canvas),
        ReaderEvent::Opened { file_ix, source, doc, pages } => on_opened(model, file_ix, source, doc, pages),
        ReaderEvent::Rehighlighted { source, doc, pages, added } => on_rehighlighted(model, source, doc, pages, added),
        ReaderEvent::NoteAdded { source, doc, pages } => on_note_added(model, source, doc, pages),
        ReaderEvent::Relaid { pages, page_ix } => on_relaid(model, pages, page_ix),
        ReaderEvent::StatusSet { source, doc, pages, status, quiet } => on_status_set(model, source, doc, pages, status, quiet),
        ReaderEvent::StatusMarked { file_ix, status, quiet } => on_status_marked(model, file_ix, status, quiet, canvas),
        ReaderEvent::TagsSet { source, doc, pages, tags } => on_tags_set(model, source, doc, pages, tags),
        ReaderEvent::TagsMarked { file_ix, tags } => on_tags_marked(model, file_ix, tags, canvas),
        ReaderEvent::Synced { ok } => {
            model.flash(if ok { "Synced" } else { "Sync failed" });
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderEvent::UpdateChecked { msg } => {
            model.flash(msg);
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderEvent::VaultRescanned { files } => on_vault_rescanned(model, files, canvas),
        ReaderEvent::Tick => on_tick(model),
    }
}

/// Swap in a fresh vault scan (after a sync pulled new/changed clippings). The scan is re-sorted,
/// so indices shift: the open file's `file_ix` is remapped by path (kept as-is if it vanished —
/// the in-memory source still reads fine), and the list scroll is clamped into the new length.
fn on_vault_rescanned(model: &mut ReaderModel, files: Vec<crate::model::VaultFile>, canvas: Canvas) -> Vec<ReaderEffect> {
    // A rescan renumbers the vault — any multi-select ticks would point at the wrong clippings.
    model.selected = None;
    // Drop any open highlight menu so a stale range can't linger across the rescan.
    if let Screen::Reading(r) = &mut model.screen {
        r.highlight_menu = None;
    }
    if let Screen::Reading(r) = &model.screen {
        let cur = model.vault.get(r.file_ix).map(|f| f.path.clone());
        if let Some(new_ix) = cur.and_then(|p| files.iter().position(|f| f.path == p)) {
            if let Screen::Reading(r) = &mut model.screen {
                r.file_ix = new_ix;
            }
        }
    }
    model.vault = files;
    let n = model.visible().len();
    let per = list_rows_per_page(canvas.height).max(1);
    if let Screen::List { scroll } | Screen::Search { scroll, .. } = &mut model.screen {
        if *scroll >= n {
            *scroll = n.saturating_sub(1) / per * per;
        }
    }
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// Apply a bulk action over the ticked clippings: one effect per file (the proven single-file write
/// paths — the runtime feeds each one back as `StatusMarked`/`TagsMarked`, which refreshes the row),
/// `busy` while they land, and a trailing full redraw. The selection SURVIVES: Archive is the only
/// action that drops it, because archived rows leave the view. Read and Fav leave the batch ticked,
/// so you can mark a page read and then archive it without re-ticking.
fn bulk(model: &mut ReaderModel, effect: impl Fn(&ReaderModel, usize) -> ReaderEffect) -> Vec<ReaderEffect> {
    let Some(sel) = model.selected.clone() else {
        return vec![];
    };
    let live: Vec<usize> = sel.into_iter().filter(|&ix| ix < model.vault.len()).collect();
    if live.is_empty() {
        return vec![ReaderEffect::Redraw(Dirty::Full)];
    }
    model.busy = true;
    let m: &ReaderModel = model;
    let mut effects: Vec<ReaderEffect> = live.iter().map(|&ix| effect(m, ix)).collect();
    effects.push(ReaderEffect::Redraw(Dirty::Full));
    effects
}

fn on_tap(model: &mut ReaderModel, x: i32, y: i32, hits: &[(Rect, ReaderTarget)], canvas: Canvas) -> Vec<ReaderEffect> {
    // A quick tap navigates / hits chrome — a highlight is *transparent* to a tap (its menu is a
    // long-press), so a tap that lands on highlighted text still turns the page instead of fighting
    // the thin target. Skip `TapHighlight` and resolve to the nav zone / control beneath it.
    let Some((_, target)) = hits.iter().rev().find(|(r, t)| r.contains(x, y) && !matches!(t, ReaderTarget::TapHighlight { .. })) else {
        return vec![];
    };
    let per = match model.screen {
        Screen::Search { kb, .. } => search_rows_per_page(canvas.height, kb, model.selected.is_some()),
        _ => list_rows_per_page(canvas.height),
    };
    match target.clone() {
        ReaderTarget::Exit => vec![ReaderEffect::Exit],
        ReaderTarget::SyncVault => {
            // Pull new clippings: flash now, let the runtime run vaultsync in the background and
            // feed back Synced + VaultRescanned. The runtime ignores the effect if a sync is
            // already running, so a double-tap can't stack two.
            model.flash("Syncing…");
            vec![ReaderEffect::Redraw(Dirty::Full), ReaderEffect::SyncVault]
        }
        ReaderTarget::OpenFile(ix) => {
            if model.busy || ix >= model.vault.len() {
                return vec![];
            }
            model.busy = true;
            vec![ReaderEffect::Open(ix)]
        }
        ReaderTarget::ListPrev => {
            if let Screen::List { scroll } | Screen::Search { scroll, .. } = &mut model.screen {
                *scroll = scroll.saturating_sub(per);
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::ListNext => {
            let n = model.visible().len();
            if let Screen::List { scroll } | Screen::Search { scroll, .. } = &mut model.screen {
                if *scroll + per < n {
                    *scroll += per;
                }
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::PrevPage => turn_page(model, -1, canvas),
        ReaderTarget::NextPage => turn_page(model, 1, canvas),
        ReaderTarget::Back => {
            // Return to wherever we came from: the search results if a query is active, else the list —
            // scrolled to the just-read file's slot (using the destination's rows-per-page).
            let to_search = !model.query.trim().is_empty();
            // Returning to search lands on the committed results view (keyboard down) — the query
            // was already committed when the result was opened.
            let dest_per = if to_search { search_rows_per_page(canvas.height, false, false) } else { list_rows_per_page(canvas.height) };
            let scroll = match &model.screen {
                Screen::Reading(r) => {
                    let fix = r.file_ix;
                    model.visible().iter().position(|&i| i == fix).map(|p| (p / dest_per) * dest_per).unwrap_or(0)
                }
                Screen::List { scroll } | Screen::Search { scroll, .. } => *scroll,
                Screen::Authors { .. } => 0,
            };
            model.screen = if to_search { Screen::Search { scroll, kb: false } } else { Screen::List { scroll } };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::TextSmaller => rescale(model, -1),
        ReaderTarget::TextLarger => rescale(model, 1),
        ReaderTarget::SetStatusFor(ix, status) => {
            if model.busy || ix >= model.vault.len() {
                return vec![];
            }
            // Books are read-only — never write a status into a Readwise export (defensive; the
            // view draws no status controls for them).
            if model.vault[ix].kind == crate::model::DocKind::Book {
                return vec![];
            }
            // No-op when it's already that state (tapping the active reading-view segment).
            if model.vault[ix].status == status {
                return vec![];
            }
            model.busy = true;
            vec![ReaderEffect::SetStatus { file_ix: ix, status, quiet: false }]
        }
        // The three filter setters all originate from the shelf now (or a legacy tap); applying any of
        // them closes the shelf so the freshly filtered list is visible in one refresh.
        ReaderTarget::SetFilter(f) => {
            model.filter = f;
            model.books = false; // a status view is an article view — leave the Books collection
            model.shelf_open = false;
            model.selected = None; // the rows a selection referred to may no longer be visible
            if let Screen::List { scroll } = &mut model.screen {
                *scroll = 0; // the filtered list changed length — start at the top
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SetSort(s) => {
            model.sort = s;
            model.shelf_open = false;
            model.selected = None;
            if let Screen::List { scroll } = &mut model.screen {
                *scroll = 0;
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SetTagFilter(t) => {
            model.tag_filter = t;
            model.books = false; // tag/collection views are article views — leave Books
            // Opening a COLLECTION (Favourites / Archived) shows the whole collection — a leftover
            // status filter (e.g. Reading) would silently hide members of it. Content-tag filters
            // keep ANDing with status (they're topic narrowing, not a place you go).
            if matches!(t, Some(Tag::Favourite) | Some(Tag::Archive)) {
                model.filter = StatusFilter::All;
            }
            model.shelf_open = false;
            model.selected = None;
            if let Screen::List { scroll } = &mut model.screen {
                *scroll = 0; // the filtered list changed length — start at the top
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SetAuthorFilter(a) => {
            model.author_filter = a;
            model.books = false; // an author view is an article view — leave the Books collection
            model.shelf_open = false;
            model.selected = None;
            // Applying from the picker lands back on the freshly filtered list; from the shelf the
            // screen is already the list — either way, start at the top.
            model.screen = Screen::List { scroll: 0 };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::OpenAuthors => {
            model.shelf_open = false;
            model.selected = None;
            model.screen = Screen::Authors { scroll: 0 };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::CloseAuthors => {
            model.screen = Screen::List { scroll: 0 };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::AuthorsPrev => {
            let per = authors_rows_per_page(canvas.height);
            if let Screen::Authors { scroll } = &mut model.screen {
                *scroll = scroll.saturating_sub(per);
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::AuthorsNext => {
            let per = authors_rows_per_page(canvas.height);
            let total = model.authors().len() + 1; // + the "All authors" row
            if let Screen::Authors { scroll } = &mut model.screen {
                if *scroll + per < total {
                    *scroll += per;
                }
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SetBooks(b) => {
            // Enter/leave the read-only Books collection. The status/tag filters are left as they
            // were (Books ignores them; leaving restores the previous article view).
            model.books = b;
            model.shelf_open = false;
            model.selected = None;
            if let Screen::List { scroll } = &mut model.screen {
                *scroll = 0; // a different list — start at the top
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::OpenShelf => {
            model.shelf_open = true;
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::CloseShelf => {
            model.shelf_open = false;
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::OpenMore => set_panels(model, true, false),
        ReaderTarget::CloseMore => set_panels(model, false, false),
        ReaderTarget::OpenContents => set_panels(model, false, true),
        ReaderTarget::CloseContents => set_panels(model, false, false),
        ReaderTarget::JumpToHeading(page_ix) => jump_to_heading(model, page_ix),
        // The reading-toolbar Archive icon (the only source of ToggleTag(Archive) — the More sheet
        // carries only Tag::CONTENT) archives AND drops back to the list; content tags toggle in place.
        ReaderTarget::ToggleTag(Tag::Archive) => archive_and_return(model, per),
        ReaderTarget::ToggleTag(t) => toggle_tag(model, t),
        ReaderTarget::ToggleFocus => toggle_focus(model),
        ReaderTarget::OpenSearch => {
            model.query.clear();
            model.search_layer = KbLayer::Letters;
            model.search_shift = false;
            model.selected = None;
            model.screen = Screen::Search { scroll: 0, kb: true };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::CloseSearch => {
            model.query.clear();
            model.search_layer = KbLayer::Letters;
            model.search_shift = false;
            model.selected = None;
            model.screen = Screen::List { scroll: 0 };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SubmitSearch => {
            // Keyboard "Search": commit the query — drop the keyboard so the results fill the
            // screen. With nothing typed there's nothing to show, so it just closes search.
            model.search_layer = KbLayer::Letters;
            model.search_shift = false;
            model.selected = None;
            if model.query.trim().is_empty() {
                model.query.clear();
                model.screen = Screen::List { scroll: 0 };
            } else {
                model.screen = Screen::Search { scroll: 0, kb: false };
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::EditQuery => {
            // Results view: tapping the query bar brings the keyboard back to refine the query.
            model.selected = None;
            if let Screen::Search { kb, .. } = &mut model.screen {
                *kb = true;
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SelectMode => {
            model.selected = Some(Vec::new());
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::ToggleSelect(ix) => {
            if let Some(sel) = &mut model.selected {
                match sel.iter().position(|&s| s == ix) {
                    Some(p) => {
                        sel.remove(p);
                    }
                    None => sel.push(ix),
                }
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::ClearFilters => {
            model.clear_filters();
            model.selected = None;
            model.screen = Screen::List { scroll: 0 };
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::SelectAll => {
            // Tick every row on this page — or untick them if they're all ticked already (the chip
            // reads "None" then). Ticks made on other pages are left alone either way, so a sweep
            // across a long filter is All · next · All · … and then one Archive.
            let page = match model.screen {
                Screen::List { scroll } => model.page_indices(scroll, list_rows_per_page(canvas.height)),
                Screen::Search { scroll, kb } => {
                    model.page_indices(scroll, search_rows_per_page(canvas.height, kb, true))
                }
                _ => return vec![],
            };
            if let Some(sel) = &mut model.selected {
                if !page.is_empty() && page.iter().all(|i| sel.contains(i)) {
                    sel.retain(|i| !page.contains(i));
                } else {
                    for i in page {
                        if !sel.contains(&i) {
                            sel.push(i);
                        }
                    }
                }
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::ReadSelected => {
            // Bulk status write: one SetStatus per ticked file (the same single-file path the list's
            // status gutter uses). Flips to NotRead once they're all Read, so the one chip undoes
            // itself.
            let status = if model.selection_all_read() { ReadStatus::NotRead } else { ReadStatus::Read };
            bulk(model, move |_, ix| ReaderEffect::SetStatus { file_ix: ix, status, quiet: false })
        }
        ReaderTarget::FavSelected => {
            // Bulk favourite, flipping to un-favourite once they all carry the tag. Merges into each
            // file's existing tags, like the reading view's Fav toggle.
            let add = !model.selection_all_fav();
            bulk(model, move |m, ix| {
                let mut tags = m.vault[ix].tags.clone();
                match (add, tags.contains(&Tag::Favourite)) {
                    (true, false) => tags.push(Tag::Favourite),
                    (false, true) => tags.retain(|t| *t != Tag::Favourite),
                    _ => {}
                }
                ReaderEffect::SetTags { file_ix: ix, tags }
            })
        }
        ReaderTarget::CancelSelect => {
            model.selected = None;
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        ReaderTarget::ArchiveSelected => {
            // Archive every ticked clipping, appending to whatever tags it already carries. Unlike
            // Read/Fav this LEAVES select mode: the archived rows drop out of the view, so there'd
            // be nothing left ticked to act on.
            let effects = bulk(model, |m, ix| {
                let mut tags = m.vault[ix].tags.clone();
                if !tags.contains(&Tag::Archive) {
                    tags.push(Tag::Archive);
                }
                ReaderEffect::SetTags { file_ix: ix, tags }
            });
            model.selected = None;
            effects
        }
        ReaderTarget::Key(c) => {
            model.query.push(c);
            if let Screen::Search { scroll, .. } = &mut model.screen {
                *scroll = 0;
            }
            // One-shot Shift: a shifted key flips the board back to lowercase, so the keyboard must
            // repaint too (full); an unshifted key only touches the query + results above it.
            let was_shift = model.search_shift;
            model.search_shift = false;
            if was_shift {
                vec![ReaderEffect::Redraw(Dirty::Full)]
            } else {
                let kb_top = canvas.height as i32 - SEARCH_KB_H;
                vec![ReaderEffect::Redraw(Dirty::Region(0, 0, canvas.width as i32, kb_top))]
            }
        }
        ReaderTarget::Backspace => search_edit(model, |q| { q.pop(); }, canvas),
        ReaderTarget::ClearQuery => {
            // Clearing restarts the search, so bring the keyboard back if the committed results
            // view had dismissed it (a Full redraw — the reclaimed rows must repaint too).
            model.selected = None;
            let reopened = matches!(model.screen, Screen::Search { kb: false, .. });
            if let Screen::Search { kb, .. } = &mut model.screen {
                *kb = true;
            }
            let eff = search_edit(model, |q| q.clear(), canvas);
            if reopened { vec![ReaderEffect::Redraw(Dirty::Full)] } else { eff }
        }
        ReaderTarget::KbShift => {
            model.search_shift = !model.search_shift;
            vec![ReaderEffect::Redraw(kb_region(canvas))]
        }
        ReaderTarget::KbLayer => {
            model.search_layer = match model.search_layer {
                KbLayer::Letters => KbLayer::Symbols,
                KbLayer::Symbols => KbLayer::Letters,
            };
            vec![ReaderEffect::Redraw(kb_region(canvas))]
        }
        // The note sheet's keyboard (mirrors the search keyboard, but edits the note buffer).
        ReaderTarget::NoteKey(c) => {
            let mut was_shift = false;
            if let Screen::Reading(r) = &mut model.screen {
                if let Some(note) = &mut r.note {
                    was_shift = note.shift;
                    note.buf.push(c);
                    note.shift = false;
                }
            }
            // A shifted key flips the board back to lowercase → repaint the keys too (full); an
            // unshifted key only touches the field above the docked keyboard.
            if was_shift {
                vec![ReaderEffect::Redraw(Dirty::Full)]
            } else {
                let kb_top = canvas.height as i32 - SEARCH_KB_H;
                vec![ReaderEffect::Redraw(Dirty::Region(0, 0, canvas.width as i32, kb_top))]
            }
        }
        ReaderTarget::NoteBackspace => note_edit(model, |b| { b.pop(); }, canvas),
        ReaderTarget::NoteShift => {
            if let Screen::Reading(r) = &mut model.screen {
                if let Some(note) = &mut r.note {
                    note.shift = !note.shift;
                }
            }
            vec![ReaderEffect::Redraw(kb_region(canvas))]
        }
        ReaderTarget::NoteLayer => {
            if let Screen::Reading(r) = &mut model.screen {
                if let Some(note) = &mut r.note {
                    note.layer = match note.layer {
                        KbLayer::Letters => KbLayer::Symbols,
                        KbLayer::Symbols => KbLayer::Letters,
                    };
                }
            }
            vec![ReaderEffect::Redraw(kb_region(canvas))]
        }
        ReaderTarget::NoteSave => note_save(model),
        ReaderTarget::NoteCancel => {
            if let Screen::Reading(r) = &mut model.screen {
                r.note = None;
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        // A quick tap navigates (the hit-test above skips TapHighlight); the menu is opened by a
        // long-press → `on_long_press`, or a pen scrub → `on_pen_up`. Kept only for exhaustiveness.
        ReaderTarget::TapHighlight { .. } => vec![],
        ReaderTarget::HighlightMenuDismiss => {
            let menu = match &mut model.screen {
                Screen::Reading(r) => r.highlight_menu.take(),
                _ => None,
            };
            // Repaint just where the pill was; the page beneath comes back in one small partial.
            match menu {
                Some(m) => vec![ReaderEffect::Redraw(pill_region(m.x, m.y, canvas))],
                None => vec![ReaderEffect::Redraw(Dirty::Full)],
            }
        }
        // Open the note editor for the tapped highlight, prefilled with its existing note text (edit,
        // don't stack a second). Closes the menu.
        ReaderTarget::HighlightAddNote => {
            if let Screen::Reading(r) = &mut model.screen {
                if let Some(menu) = r.highlight_menu.take() {
                    let existing = crate::parse::note_for_highlight(&r.source, menu.range.start);
                    let had_note = existing.is_some();
                    let buf = existing.map(|(_, t)| t).unwrap_or_default();
                    r.note = Some(NoteEntry { buf, anchor: menu.range.start, layer: KbLayer::Letters, shift: false, had_note });
                }
            }
            vec![ReaderEffect::Redraw(Dirty::Full)]
        }
        // Delete the tapped highlight (and any note it carries — see `remove_highlight_and_note`). The
        // only delete path now the pen toggle-off + undo stack are gone, so it's permanent.
        ReaderTarget::HighlightDelete => {
            if model.busy {
                return vec![];
            }
            let range = match &mut model.screen {
                Screen::Reading(r) => r.highlight_menu.take().map(|m| m.range),
                _ => None,
            };
            let path = match &model.screen {
                Screen::Reading(r) => model.vault.get(r.file_ix).map(|f| f.path.clone()),
                _ => None,
            };
            match (range, path) {
                (Some(range), Some(path)) => {
                    model.busy = true;
                    vec![ReaderEffect::Highlight { path, range, add: false }]
                }
                _ => vec![ReaderEffect::Redraw(Dirty::Full)],
            }
        }
    }
}

/// A long-press (finger dwell) opens the highlight menu for the highlight at — or within a line of —
/// the press. Anywhere else it's a no-op: a quick tap does the navigating, so an off-target hold never
/// turns the page (the backend already swallowed the release).
fn on_long_press(model: &mut ReaderModel, x: i32, y: i32, hits: &[(Rect, ReaderTarget)], canvas: Canvas) -> Vec<ReaderEffect> {
    let hit = highlight_near(hits, x, y, HL_LONGPRESS_SLOP_Y);
    if let Screen::Reading(r) = &mut model.screen {
        if let Some(range) = hit {
            // Menu opens: a stylus dwell fires mid-stroke, so clear any in-flight pen stroke — the
            // trailing PenUp (and any post-dwell jitter moves) then no-op. A finger long-press has no
            // stroke in flight, so this is a no-op for it.
            r.selection = None;
            r.pen_start = None;
            let has_note = crate::parse::note_for_highlight(&r.source, range.start).is_some();
            r.highlight_menu = Some(HighlightMenu { range, has_note, x, y });
            // Only the pill's rect refreshes — a small overlay must not flash the whole panel.
            return vec![ReaderEffect::Redraw(pill_region(x, y, canvas))];
        }
        // Off-highlight dwell: leave the stroke alive — a nib rested on plain text before dragging
        // must still produce its highlight (this was the "highlight needs three tries" bug). Remember
        // the dwell so a still lift is a no-op rather than a tap → page turn.
        if r.pen_start.is_some() {
            r.pen_dwelled = true;
        }
    }
    vec![]
}

/// The `TapHighlight` range nearest a press: its run box must contain `x` horizontally
/// (± [`HL_LONGPRESS_SLOP_X`]) and lie within `slop_y` of `y` vertically. Returns the closest —
/// smallest vertical gap, then smallest horizontal overhang, so a slightly-off press still opens the
/// intended highlight (and two highlights on one line resolve to the nearer).
fn highlight_near(hits: &[(Rect, ReaderTarget)], x: i32, y: i32, slop_y: i32) -> Option<Range<usize>> {
    let mut best: Option<((i32, i32), Range<usize>)> = None;
    for (r, t) in hits {
        let ReaderTarget::TapHighlight { range } = t else {
            continue;
        };
        let dx = (r.x - x).max(x - (r.x + r.w)).max(0);
        if dx > HL_LONGPRESS_SLOP_X {
            continue;
        }
        let dy = (r.y - y).max(y - (r.y + r.h)).max(0);
        if dy > slop_y {
            continue;
        }
        if best.as_ref().map_or(true, |(bd, _)| (dy, dx) < *bd) {
            best = Some(((dy, dx), range.clone()));
        }
    }
    best.map(|(_, r)| r)
}

/// Apply an edit to the in-progress note buffer and repaint everything above the docked keyboard (a
/// `Dirty::Region` so the static keyboard isn't re-blitted — mirrors [`search_edit`]).
fn note_edit(model: &mut ReaderModel, edit: impl FnOnce(&mut String), canvas: Canvas) -> Vec<ReaderEffect> {
    if let Screen::Reading(r) = &mut model.screen {
        if let Some(note) = &mut r.note {
            edit(&mut note.buf);
        }
    }
    let kb_top = canvas.height as i32 - SEARCH_KB_H;
    vec![ReaderEffect::Redraw(Dirty::Region(0, 0, canvas.width as i32, kb_top))]
}

/// Save the note: emit an [`ReaderEffect::AddNote`] for the runtime to splice into the source. Writes
/// when there's text to save OR when an existing note was cleared (empty text → the runtime drops the
/// footnote); a brand-new note left blank just closes the sheet with no write. Closes the sheet either
/// way.
fn note_save(model: &mut ReaderModel) -> Vec<ReaderEffect> {
    let (path, anchor, text, had_note) = match &model.screen {
        Screen::Reading(r) => match &r.note {
            Some(note) => (model.vault.get(r.file_ix).map(|f| f.path.clone()), note.anchor, note.buf.clone(), note.had_note),
            None => return vec![],
        },
        _ => return vec![],
    };
    if let Screen::Reading(r) = &mut model.screen {
        r.note = None;
    }
    match path {
        Some(path) if !text.trim().is_empty() || had_note => {
            model.busy = true;
            vec![ReaderEffect::AddNote { path, anchor, text }]
        }
        _ => vec![ReaderEffect::Redraw(Dirty::Full)],
    }
}

/// The docked-keyboard band (the bottom `SEARCH_KB_H` px) — repainted when Shift/Layer flips the keys
/// while the query + results above it stay put.
fn kb_region(canvas: Canvas) -> Dirty {
    let kb_top = canvas.height as i32 - SEARCH_KB_H;
    Dirty::Region(0, kb_top, canvas.width as i32, SEARCH_KB_H)
}

/// Apply an edit to the search query, snap results back to the top, and repaint everything *above*
/// the docked keyboard (a `Dirty::Region` so the static keyboard isn't re-blitted — keeps e-ink
/// typing snappy; the periodic deep refresh clears any partial-update ghosting).
fn search_edit(model: &mut ReaderModel, edit: impl FnOnce(&mut String), canvas: Canvas) -> Vec<ReaderEffect> {
    edit(&mut model.query);
    if let Screen::Search { scroll, .. } = &mut model.screen {
        *scroll = 0;
    }
    let kb_top = canvas.height as i32 - SEARCH_KB_H;
    vec![ReaderEffect::Redraw(Dirty::Region(0, 0, canvas.width as i32, kb_top))]
}

/// Flip focus (full-screen) mode. Entering (and leaving from the entry overlay) changes NO layout —
/// the central text is pixel-frozen; the view just hides/shows the chrome and draws the preview
/// bands. Only leaving after in-focus page turns (the focus-paginated sub-state) re-lays-out, pinned
/// so the current top line stays put.
fn toggle_focus(model: &mut ReaderModel) -> Vec<ReaderEffect> {
    if let Screen::Reading(r) = &mut model.screen {
        if !r.focus {
            r.focus = true;
            r.focus_paginated = false;
            return vec![ReaderEffect::Redraw(Dirty::Full)];
        }
        r.focus = false;
        if std::mem::take(&mut r.focus_paginated) {
            let anchor = r.pages.get(r.page_ix).and_then(|p| p.runs().map(|g| g.src.start).min()).unwrap_or(0);
            return vec![ReaderEffect::RestoreLayout { anchor }];
        }
        return vec![ReaderEffect::Redraw(Dirty::Full)];
    }
    vec![]
}

/// Set which reading-view panel (if any) is open — the More sheet and Contents panel are mutually
/// exclusive, so opening one closes the other. No-op off the reading screen or when already in the
/// requested state.
fn set_panels(model: &mut ReaderModel, more: bool, contents: bool) -> Vec<ReaderEffect> {
    if let Screen::Reading(r) = &mut model.screen {
        if r.more_open != more || r.contents_open != contents {
            r.more_open = more;
            r.contents_open = contents;
            return vec![ReaderEffect::Redraw(Dirty::Full)];
        }
    }
    vec![]
}

/// Jump to a heading's page from the Contents panel, then close the panel.
fn jump_to_heading(model: &mut ReaderModel, page_ix: usize) -> Vec<ReaderEffect> {
    if let Screen::Reading(r) = &mut model.screen {
        r.contents_open = false;
        if page_ix < r.pages.len() {
            r.page_ix = page_ix;
        }
        return vec![ReaderEffect::Redraw(Dirty::Full)];
    }
    vec![]
}

/// Toggle a managed tag on the open clipping: recompute the managed set (in canonical [`Tag::ALL`]
/// order so the written block + ticks are stable), then ask the runtime to write it.
fn toggle_tag(model: &mut ReaderModel, t: Tag) -> Vec<ReaderEffect> {
    if model.busy || model.reading_is_book() {
        return vec![];
    }
    let Screen::Reading(r) = &model.screen else {
        return vec![];
    };
    let file_ix = r.file_ix;
    let Some(f) = model.vault.get(file_ix) else {
        return vec![];
    };
    let present = f.tags.contains(&t);
    let tags: Vec<Tag> = Tag::ALL
        .into_iter()
        .filter(|&x| if x == t { !present } else { f.tags.contains(&x) })
        .collect();
    model.busy = true;
    vec![ReaderEffect::SetTags { file_ix, tags }]
}

/// The reading-footer Archive pill: toggle the archive tag AND drop straight back to the library
/// list. We capture the file's slot in the *current* list BEFORE the write (archiving hides it from
/// `visible()`), switch to the list, and let `write_tags` land the change while we're off the doc
/// (→ `TagsMarked` updates `vault[file_ix].tags`). Same set-rebuild as [`toggle_tag`].
fn archive_and_return(model: &mut ReaderModel, per: usize) -> Vec<ReaderEffect> {
    if model.busy || model.reading_is_book() {
        return vec![];
    }
    let Screen::Reading(r) = &model.screen else {
        return vec![];
    };
    let file_ix = r.file_ix;
    let Some(f) = model.vault.get(file_ix) else {
        return vec![];
    };
    let present = f.tags.contains(&Tag::Archive);
    let tags: Vec<Tag> = Tag::ALL
        .into_iter()
        .filter(|&x| if x == Tag::Archive { !present } else { f.tags.contains(&x) })
        .collect();
    let scroll = model.visible().iter().position(|&i| i == file_ix).map(|p| (p / per) * per).unwrap_or(0);
    model.screen = Screen::List { scroll };
    model.busy = true;
    vec![ReaderEffect::SetTags { file_ix, tags }]
}

/// A text-size step: guard, then hand the re-layout to the runtime (which owns the persisted scale).
fn rescale(model: &mut ReaderModel, delta: i32) -> Vec<ReaderEffect> {
    if model.busy || !matches!(model.screen, Screen::Reading(_)) {
        return vec![];
    }
    model.busy = true;
    vec![ReaderEffect::Rescale { delta }]
}

fn turn_page(model: &mut ReaderModel, delta: i32, canvas: Canvas) -> Vec<ReaderEffect> {
    // Read the current pagination + status before mutating (both immutable borrows of `model`).
    let Screen::Reading(r) = &model.screen else {
        return vec![];
    };
    // The first turn inside focus leaves the entry overlay: the panel fully refreshes on a turn
    // anyway, so switch to focus's own full-screen pagination, continuing EXACTLY after (forward) /
    // before (backward) the bytes the entry screen showed — bands included, nothing repeats.
    if r.focus && !r.focus_paginated {
        let window = crate::view::focus_entry_window(&r.pages, r.page_ix, canvas.height as f32);
        let doc_start = r.pages.first().and_then(|p| p.runs().map(|g| g.src.start).min()).unwrap_or(0);
        let doc_end = r.pages.last().and_then(|p| p.runs().map(|g| g.src.end).max()).unwrap_or(0);
        let (anchor, forward) = if delta > 0 {
            if window.end >= doc_end {
                return vec![]; // everything ahead is already on screen
            }
            (window.end, true)
        } else {
            if window.start <= doc_start {
                return vec![]; // already at the very start
            }
            (window.start, false)
        };
        if let Screen::Reading(r) = &mut model.screen {
            r.focus_paginated = true;
            r.selection = None;
        }
        // No auto-status here: page indices change meaning across the re-pagination; the next
        // ordinary turn advances the status as usual.
        return vec![ReaderEffect::FocusPaginate { anchor, forward }];
    }
    let n = r.pages.len();
    let next = (r.page_ix as i32 + delta).clamp(0, n.saturating_sub(1) as i32) as usize;
    if next == r.page_ix {
        return vec![];
    }
    let file_ix = r.file_ix;
    let cur = model.reading_status();
    if let Screen::Reading(r) = &mut model.screen {
        r.page_ix = next;
        r.selection = None;
    }
    let mut effects = vec![ReaderEffect::Redraw(Dirty::Full)];
    // Auto-advance the read status by reading progress (upgrade-only — see `auto_status_target`).
    // Quiet: the runtime defers the write until after the page turn paints, and the feedback installs
    // with no flash/redraw — the turn must never wait on file I/O + a doc relayout.
    // Never for a book: a page turn must not write into a read-only Readwise export.
    if !model.reading_is_book() {
        if let Some(status) = auto_status_target(next, n, cur) {
            effects.push(ReaderEffect::SetStatus { file_ix, status, quiet: true });
        }
    }
    effects
}

/// Partial-refresh region for the highlight pill at `(x, y)` (a hair of padding for AA edges) —
/// opening/closing the little menu must not flash the whole panel.
fn pill_region(x: i32, y: i32, canvas: Canvas) -> Dirty {
    let r = crate::view::highlight_pill_rect(x, y, canvas.width as i32);
    let pad = 3;
    Dirty::Region(
        (r.x - pad).max(0),
        (r.y - pad).max(0),
        (r.w + 2 * pad).min(canvas.width as i32),
        (r.h + 2 * pad).min(canvas.height as i32),
    )
}

fn status_rank(s: ReadStatus) -> u8 {
    match s {
        ReadStatus::NotRead => 0,
        ReadStatus::Reading => 1,
        ReadStatus::Read => 2,
    }
}

/// Reading-progress auto-status for a page change: the final page → Read (including a ONE-page
/// doc, whose only page IS its last — opening a one-screen article is reading it; without this,
/// short clippings could never auto-advance at all); page 2+ → Reading; page 1 of several → no
/// change. Returns `None` unless the target would *advance* the status, so paging backward (or
/// revisiting a read article) never downgrades it.
fn auto_status_target(page_ix: usize, npages: usize, cur: ReadStatus) -> Option<ReadStatus> {
    let target = if npages >= 1 && page_ix + 1 == npages {
        ReadStatus::Read
    } else if page_ix >= 1 {
        ReadStatus::Reading
    } else {
        return None;
    };
    (status_rank(target) > status_rank(cur)).then_some(target)
}

/// A Kindle-style edge swipe turns the page. It must START in the outer third on the side matching
/// the direction — right third + leftward (`dir == 1`) → next; left third + rightward (`dir == -1`)
/// → previous — so a horizontal drag across the middle of the text doesn't flip pages. Reading-only.
fn on_swipe(model: &mut ReaderModel, x0: i32, dir: i32, canvas: Canvas) -> Vec<ReaderEffect> {
    if !matches!(model.screen, Screen::Reading(_)) {
        return vec![];
    }
    let third = canvas.width as i32 / 3;
    let from_edge = match dir {
        1 => x0 >= 2 * third,
        -1 => x0 < third,
        _ => false,
    };
    if from_edge {
        turn_page(model, dir, canvas)
    } else {
        vec![]
    }
}

fn on_pen_down(model: &mut ReaderModel, x: i32, y: i32, fonts: &Fonts) -> Vec<ReaderEffect> {
    // A book is read-only: the pen never starts a selection (pen_start still tracks the press so a
    // still lift can route as a tap — pen page turns keep working).
    let book = model.reading_is_book();
    if let Screen::Reading(r) = &mut model.screen {
        r.pen_start = Some((x, y));
        r.pen_dwelled = false;
        if !book {
            if let Some(page) = r.pages.get(r.page_ix) {
                if let Some(b) = page_hit(page, fonts, x as f32, y as f32) {
                    r.selection = Some(Selection { anchor: b, caret: b });
                }
            }
        }
    }
    vec![] // nothing visible until the drag produces a range
}

fn on_pen_move(model: &mut ReaderModel, x: i32, y: i32, fonts: &Fonts) -> Vec<ReaderEffect> {
    if let Screen::Reading(r) = &mut model.screen {
        if let (Some(sel), Some(page)) = (r.selection.as_mut(), r.pages.get(r.page_ix)) {
            if let Some(b) = page_hit(page, fonts, x as f32, y as f32) {
                sel.caret = b;
            }
        }
    }
    // v1: no live tint (avoids e-ink drag lag) — the highlight appears on release.
    vec![]
}

fn on_pen_up(model: &mut ReaderModel, x: i32, y: i32, hits: &[(Rect, ReaderTarget)], fonts: &Fonts, canvas: Canvas) -> Vec<ReaderEffect> {
    // Books: no selection ever exists (see on_pen_down) — a still lift is a tap (navigate), a drag
    // or a dwelt lift is a no-op. Handled up front so the article path below stays untouched.
    if model.reading_is_book() {
        let (start, dwelled) = match &mut model.screen {
            Screen::Reading(r) => (r.pen_start.take(), std::mem::take(&mut r.pen_dwelled)),
            _ => return vec![],
        };
        let moved = matches!(start, Some((sx, sy)) if (x - sx).abs() >= MIN_DRAG_PX || (y - sy).abs() >= MIN_DRAG_PX);
        if !moved && !dwelled {
            return on_tap(model, x, y, hits, canvas);
        }
        return vec![];
    }
    let (start, sel, dwelled) = match &mut model.screen {
        Screen::Reading(r) => (r.pen_start.take(), r.selection.take(), std::mem::take(&mut r.pen_dwelled)),
        _ => return vec![],
    };
    let Some(mut sel) = sel else {
        return vec![];
    };
    // A press that barely moved is a tap, not a drag: route it to the UI just like a touch, so
    // tapping the nav buttons (or a highlight, or plain text) with the stylus does the right thing and
    // never smears a stray highlight.
    let moved = matches!(start, Some((sx, sy)) if (x - sx).abs() >= MIN_DRAG_PX || (y - sy).abs() >= MIN_DRAG_PX);
    if !moved {
        // A nib that rested long enough to dwell and then lifted in place is neither a tap nor a
        // highlight — the user paused, then thought better of it. Never turn the page for it.
        if dwelled {
            return vec![];
        }
        return on_tap(model, x, y, hits, canvas);
    }
    // A drag while the note sheet is open must not start a new highlight (which would replace the
    // note being typed); taps are handled above, so the note keyboard stays usable.
    if matches!(&model.screen, Screen::Reading(r) if r.note.is_some()) {
        return vec![];
    }
    if let Screen::Reading(r) = &model.screen {
        if let Some(page) = r.pages.get(r.page_ix) {
            if let Some(b) = page_hit(page, fonts, x as f32, y as f32) {
                sel.caret = b;
            }
        }
    }
    let range = sel.range();
    if range.start >= range.end {
        return vec![]; // no span (e.g. dragged within one glyph) — nothing to highlight
    }
    let Screen::Reading(r) = &model.screen else {
        return vec![];
    };
    let range = snap_to_words(&r.source, range);
    // A pen press that stays within an existing highlight opens its menu — the pen's path to Note /
    // Delete (a quick tap navigates; a drag onto new text highlights). Dragging over a highlight can't
    // add anything, so this repurposes that otherwise-dead gesture. Compute the menu + path from the
    // immutable borrow first, so the mutable re-borrow that sets the menu doesn't overlap it.
    let menu = r.pages.get(r.page_ix).and_then(|page| {
        range_is_highlighted(page, &range)
            .then(|| enclosing_highlight_range(page, &range))
            .flatten()
            .map(|hl| {
                let has_note = crate::parse::note_for_highlight(&r.source, hl.start).is_some();
                HighlightMenu { range: hl, has_note, x, y }
            })
    });
    let path = model.vault.get(r.file_ix).map(|f| f.path.clone());
    if let Some(menu) = menu {
        let (mx, my) = (menu.x, menu.y);
        if let Screen::Reading(r) = &mut model.screen {
            r.highlight_menu = Some(menu);
        }
        // Only the pill's rect refreshes (see `pill_region`).
        return vec![ReaderEffect::Redraw(pill_region(mx, my, canvas))];
    }
    let Some(path) = path else {
        return vec![];
    };
    model.busy = true;
    vec![ReaderEffect::Highlight { path, range, add: true }]
}

/// The full inner byte range of the highlighted span that `range` falls within — the same span
/// [`crate::view`] builds a `TapHighlight` target for. Lets a pen press landing on an existing
/// highlight open its menu (rather than the old re-highlight no-op).
fn enclosing_highlight_range(page: &Page, range: &Range<usize>) -> Option<Range<usize>> {
    let runs: Vec<_> = page.runs().collect();
    let mut i = 0;
    while i < runs.len() {
        if !runs[i].highlight {
            i += 1;
            continue;
        }
        let start = i;
        while i < runs.len() && runs[i].highlight {
            i += 1;
        }
        let (s, e) = (runs[start].src.start, runs[i - 1].src.end);
        if s.max(range.start) < e.min(range.end) {
            return Some(s..e);
        }
    }
    None
}

/// Grow a byte range out to whole-word boundaries, so a pen drag starting/ending mid-word still
/// highlights entire words. Word bytes = ASCII alphanumerics, apostrophes (for contractions) and
/// any UTF-8 multibyte lead/continuation (letters like é); it never crosses whitespace or
/// punctuation, and the result stays on char boundaries.
fn snap_to_words(s: &str, range: Range<usize>) -> Range<usize> {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'\'' || c >= 0x80;
    let mut start = range.start;
    while start > 0 && word(b[start - 1]) {
        start -= 1;
    }
    let mut end = range.end.min(b.len());
    while end < b.len() && word(b[end]) {
        end += 1;
    }
    while start > 0 && !s.is_char_boundary(start) {
        start -= 1;
    }
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

/// A selection is "already highlighted" only if it overlaps at least one run and *every* run it
/// overlaps is highlighted — then a pen drag over it toggles the highlight off.
fn range_is_highlighted(page: &Page, range: &Range<usize>) -> bool {
    let mut overlapped = false;
    for run in page.runs() {
        let s = run.src.start.max(range.start);
        let e = run.src.end.min(range.end);
        if s < e {
            overlapped = true;
            if !run.highlight {
                return false;
            }
        }
    }
    overlapped
}

fn on_opened(
    model: &mut ReaderModel,
    file_ix: usize,
    source: String,
    doc: crate::Doc,
    pages: Vec<Page>,
) -> Vec<ReaderEffect> {
    model.busy = false;
    let title = model
        .vault
        .get(file_ix)
        .map(|f| f.title.clone())
        .filter(|_| true)
        .unwrap_or_else(|| doc.title.clone());
    model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&source, &pages), file_ix, source, doc, pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, title, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
    let mut effects = vec![ReaderEffect::Redraw(Dirty::Full)];
    // A ONE-page clipping has no page turn to hang the auto-status on — opening it IS reading it
    // to the last page, so the upgrade-only status advance runs here. (Multi-page docs advance on
    // page changes; books never auto-write.)
    let (npages, cur) = match &model.screen {
        Screen::Reading(r) => (r.pages.len(), model.reading_status()),
        _ => (0, ReadStatus::NotRead),
    };
    if npages == 1 && !model.reading_is_book() {
        if let Some(status) = auto_status_target(0, 1, cur) {
            effects.push(ReaderEffect::SetStatus { file_ix, status, quiet: true });
        }
    }
    effects
}

fn on_rehighlighted(
    model: &mut ReaderModel,
    source: String,
    doc: crate::Doc,
    pages: Vec<Page>,
    added: bool,
) -> Vec<ReaderEffect> {
    model.busy = false;
    if let Screen::Reading(r) = &mut model.screen {
        r.pages = pages;
        r.page_ix = r.page_ix.min(r.pages.len().saturating_sub(1));
        r.doc = doc;
        r.source = source;
        r.page_minutes = page_minutes(&r.source, &r.pages);
        r.selection = None;
        r.highlight_menu = None;
        if let Some(f) = model.vault.get_mut(r.file_ix) {
            f.has_highlights = r.source.contains("==");
        }
    }
    model.flash(if added { "Highlighted" } else { "Removed highlight" });
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A note write (`==…==[^n]` + `[^n]: …`) + re-layout finished: swap in the refreshed source/layout and
/// close the note sheet.
fn on_note_added(model: &mut ReaderModel, source: String, doc: crate::Doc, pages: Vec<Page>) -> Vec<ReaderEffect> {
    model.busy = false;
    if let Screen::Reading(r) = &mut model.screen {
        r.pages = pages;
        r.page_ix = r.page_ix.min(r.pages.len().saturating_sub(1));
        r.doc = doc;
        r.source = source;
        r.page_minutes = page_minutes(&r.source, &r.pages);
        r.selection = None;
        r.note = None;
        r.highlight_menu = None;
    }
    model.flash("Note added");
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A text-size re-layout finished: swap in the new pages + the remapped page index.
fn on_relaid(model: &mut ReaderModel, pages: Vec<Page>, page_ix: usize) -> Vec<ReaderEffect> {
    model.busy = false;
    if let Screen::Reading(r) = &mut model.screen {
        r.pages = pages;
        r.page_minutes = page_minutes(&r.source, &r.pages);
        r.page_ix = page_ix.min(r.pages.len().saturating_sub(1));
        r.selection = None;
        // The relaid pages have new byte→rect geometry; a menu's cached range would point into the old
        // layout, so drop it (the highlight is still tappable at its new position).
        r.highlight_menu = None;
    }
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A status write + re-layout finished: refresh the source/doc/pages (offsets shifted with the new
/// front-matter line) and reflect the status in the list. Not an undoable edit. `quiet` (a deferred
/// page-turn auto-advance) installs silently: no flash, no redraw — the re-laid pages render
/// pixel-identically, so a second e-ink refresh would just flash the same image.
fn on_status_set(model: &mut ReaderModel, source: String, doc: crate::Doc, pages: Vec<Page>, status: ReadStatus, quiet: bool) -> Vec<ReaderEffect> {
    model.busy = false;
    if let Screen::Reading(r) = &mut model.screen {
        r.pages = pages;
        r.page_ix = r.page_ix.min(r.pages.len().saturating_sub(1));
        r.doc = doc;
        r.source = source;
        r.page_minutes = page_minutes(&r.source, &r.pages);
        r.selection = None;
        if let Some(f) = model.vault.get_mut(r.file_ix) {
            f.status = status;
        }
    }
    if quiet {
        return vec![];
    }
    model.flash(status_flash(status));
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A list-initiated status write finished (the file wasn't open): just update its list marker.
/// `quiet` as in [`on_status_set`].
fn on_status_marked(model: &mut ReaderModel, file_ix: usize, status: ReadStatus, quiet: bool, canvas: Canvas) -> Vec<ReaderEffect> {
    model.busy = false;
    if let Some(f) = model.vault.get_mut(file_ix) {
        f.status = status;
    }
    if quiet {
        return vec![];
    }
    model.flash(status_flash(status));
    resettle(model, canvas);
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

fn status_flash(status: ReadStatus) -> &'static str {
    match status {
        ReadStatus::NotRead => "Marked not read",
        ReadStatus::Reading => "Marked reading",
        ReadStatus::Read => "Marked read",
    }
}

/// A tag write + re-layout finished for the OPEN file: refresh the source/doc/pages (offsets shifted
/// with the rewritten `tags:` block) and reflect the new tags in the list. Not an undoable edit; the
/// panel stays open so several tags can be toggled in a row.
fn on_tags_set(model: &mut ReaderModel, source: String, doc: crate::Doc, pages: Vec<Page>, tags: Vec<Tag>) -> Vec<ReaderEffect> {
    model.busy = false;
    let msg = tags_flash(&tags);
    if let Screen::Reading(r) = &mut model.screen {
        r.pages = pages;
        r.page_ix = r.page_ix.min(r.pages.len().saturating_sub(1));
        r.doc = doc;
        r.source = source;
        r.page_minutes = page_minutes(&r.source, &r.pages);
        r.selection = None;
        if let Some(f) = model.vault.get_mut(r.file_ix) {
            f.tags = tags;
        }
    }
    model.flash(msg);
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A tag write finished for a file that wasn't open: just update its list tags.
fn on_tags_marked(model: &mut ReaderModel, file_ix: usize, tags: Vec<Tag>, canvas: Canvas) -> Vec<ReaderEffect> {
    model.busy = false;
    let msg = tags_flash(&tags);
    if let Some(f) = model.vault.get_mut(file_ix) {
        f.tags = tags;
    }
    model.flash(msg);
    resettle(model, canvas);
    vec![ReaderEffect::Redraw(Dirty::Full)]
}

/// A write changed which clippings the current view holds — archiving drops them out, and so does
/// marking a batch Read while the Unread filter is on. Keep the scroll inside the shorter list
/// (paged to a row boundary) and drop ticks for rows that just left: a tick you can't see is one you
/// can't untick, and it would otherwise ride along on the next bulk action.
fn resettle(model: &mut ReaderModel, canvas: Canvas) {
    // A bulk write can empty the view it was made in — archive every article by one author and the
    // author filter now matches nothing. Walk the filters off rather than leave a blank library,
    // and say which one went.
    if let Some(msg) = model.clear_filters_until_visible() {
        model.flash(msg);
    }
    let visible = model.visible();
    if let Some(sel) = &mut model.selected {
        sel.retain(|i| visible.contains(i));
    }
    let n = visible.len();
    let select = model.selected.is_some();
    let (scroll, per) = match &mut model.screen {
        Screen::Search { scroll, kb } => (Some(scroll), search_rows_per_page(canvas.height, *kb, select)),
        Screen::List { scroll } => (Some(scroll), list_rows_per_page(canvas.height)),
        Screen::Reading(_) | Screen::Authors { .. } => (None, 1),
    };
    if let Some(scroll) = scroll {
        let per = per.max(1);
        if *scroll >= n {
            *scroll = n.saturating_sub(1) / per * per;
        }
    }
}

fn tags_flash(tags: &[Tag]) -> String {
    if tags.contains(&Tag::Archive) {
        // Archiving is the dominant action (and what a bulk select-archive ends on).
        "Archived".to_string()
    } else if tags.is_empty() {
        "Tags cleared".to_string()
    } else {
        format!("Tagged: {}", tags.iter().map(|t| t.label()).collect::<Vec<_>>().join(", "))
    }
}

fn on_tick(model: &mut ReaderModel) -> Vec<ReaderEffect> {
    let mut effects = Vec::new();
    if let Some(f) = &mut model.flash {
        f.ticks_left = f.ticks_left.saturating_sub(1);
        if f.ticks_left == 0 {
            model.flash = None;
            effects.push(ReaderEffect::Redraw(Dirty::Full));
        }
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadStatus, SortKey, StatusFilter, Tag, VaultFile};
    use crate::view::{view, HitMap};
    use crate::{layout, parse, LayoutOpts};
    use std::path::PathBuf;

    const CANVAS: Canvas = Canvas { width: 954, height: 1696 };

    fn fonts() -> Fonts {
        Fonts::embedded()
    }
    fn vault(n: usize) -> Vec<VaultFile> {
        (0..n)
            .map(|i| VaultFile {
                kind: crate::model::DocKind::Article,
                path: PathBuf::from(format!("/v/{i}.md")),
                title_lower: format!("file {i}"),
                title: format!("File {i}"),
                has_highlights: false,
                published: None,
                created: None,
                byline: None,
                byline_lower: None,
                status: ReadStatus::NotRead,
                tags: Vec::new(),
                minutes: 3 + (i as u32 % 9),
            })
            .collect()
    }
    fn hits(model: &ReaderModel) -> HitMap {
        view(model, &fonts(), 954, 1696).hits
    }
    fn tap(model: &mut ReaderModel, pred: impl Fn(&ReaderTarget) -> bool) -> Vec<ReaderEffect> {
        let h = hits(model);
        let (r, _) = h.iter().find(|(_, t)| pred(t)).expect("target present");
        let (cx, cy) = r.center();
        update(model, ReaderEvent::Tap { x: cx, y: cy }, &h, &fonts(), CANVAS)
    }
    /// Press-and-hold on the centre of the first matching target — the gesture that opens the highlight
    /// menu (a quick [`tap`] navigates instead).
    fn long_press(model: &mut ReaderModel, pred: impl Fn(&ReaderTarget) -> bool) -> Vec<ReaderEffect> {
        let h = hits(model);
        let (r, _) = h.iter().find(|(_, t)| pred(t)).expect("target present");
        let (cx, cy) = r.center();
        update(model, ReaderEvent::LongPress { x: cx, y: cy }, &h, &fonts(), CANVAS)
    }

    fn open_sample(model: &mut ReaderModel, src: &str) {
        let doc = parse(src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        update(model, ReaderEvent::Opened { file_ix: 0, source: src.to_string(), doc, pages }, &[], &fonts(), CANVAS);
    }

    /// A vault of `n` articles plus `b` read-only books (books sort last: "zz book …").
    fn vault_with_books(n: usize, b: usize) -> Vec<VaultFile> {
        let mut v = vault(n);
        for i in 0..b {
            let mut f = vault(1).remove(0);
            f.kind = crate::model::DocKind::Book;
            f.path = PathBuf::from(format!("/v/books/{i}.md"));
            f.title = format!("Zz Book {i}");
            f.title_lower = f.title.to_lowercase();
            v.push(f);
        }
        v
    }

    #[test]
    fn books_collection_shows_only_books_and_other_views_only_articles() {
        let mut model = ReaderModel::new(vault_with_books(4, 2));
        let vis = model.visible();
        assert_eq!(vis.len(), 4, "default view hides books");
        assert!(vis.iter().all(|&i| model.vault[i].kind == crate::model::DocKind::Article));
        // Open the shelf → tap Books.
        tap(&mut model, |t| *t == ReaderTarget::OpenShelf);
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetBooks(true));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(model.books && !model.shelf_open);
        let vis = model.visible();
        assert_eq!(vis.len(), 2, "Books view shows only books");
        assert!(vis.iter().all(|&i| model.vault[i].kind == crate::model::DocKind::Book));
        // Books ignores a leftover status filter.
        model.filter = StatusFilter::Read;
        assert_eq!(model.visible().len(), 2, "status filter ignored in Books");
        // Picking a status view from the shelf leaves Books.
        tap(&mut model, |t| *t == ReaderTarget::OpenShelf);
        tap(&mut model, |t| *t == ReaderTarget::SetFilter(StatusFilter::All));
        assert!(!model.books, "a status view returns to articles");
        assert_eq!(model.visible().len(), 4);
    }

    #[test]
    fn books_shelf_counts_exclude_books_from_article_tallies() {
        let model = ReaderModel::new(vault_with_books(4, 3));
        let c = model.library_counts();
        assert_eq!(c.all, 4, "All counts articles only");
        assert_eq!(c.books, 3);
    }

    #[test]
    fn book_pen_drag_never_highlights_but_a_pen_tap_still_navigates() {
        let mut model = ReaderModel::new(vault_with_books(0, 1));
        model.books = true;
        // Open the (only) book with a multi-page body.
        model.busy = true;
        open_sample(&mut model, &("word ".repeat(1500)));
        // A pen drag across the text produces NO selection and NO highlight write.
        update(&mut model, ReaderEvent::PenDown { x: 200, y: 600 }, &[], &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.selection.is_none()), "no selection starts on a book");
        update(&mut model, ReaderEvent::PenMove { x: 640, y: 700 }, &[], &fonts(), CANVAS);
        let h = hits(&model);
        let eff = update(&mut model, ReaderEvent::PenUp { x: 640, y: 700 }, &h, &fonts(), CANVAS);
        assert!(eff.iter().all(|e| !matches!(e, ReaderEffect::Highlight { .. })), "drag is a no-op, got {eff:?}");
        assert!(!model.busy);
        // A still pen tap in the right third turns the page like a touch.
        update(&mut model, ReaderEvent::PenDown { x: 800, y: 800 }, &[], &fonts(), CANVAS);
        let h = hits(&model);
        update(&mut model, ReaderEvent::PenUp { x: 802, y: 801 }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 1), "pen tap pages forward");
    }

    #[test]
    fn book_page_turns_emit_no_status_writes_and_reading_bar_hides_write_actions() {
        let mut model = ReaderModel::new(vault_with_books(0, 1));
        model.books = true;
        model.busy = true;
        open_sample(&mut model, &("word ".repeat(1500)));
        let eff = tap(&mut model, |t| *t == ReaderTarget::NextPage);
        assert!(eff.iter().all(|e| !matches!(e, ReaderEffect::SetStatus { .. })), "no auto-status for a book");
        // The reading bar carries no Fav/Archive and the page no highlight targets…
        let h = hits(&model);
        assert!(!h.iter().any(|(_, t)| matches!(t, ReaderTarget::ToggleTag(_))), "no tag toggles for a book");
        assert!(!h.iter().any(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })));
        // …and the More sheet is text-size only.
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::TextLarger), "Aa± stays");
        assert!(!h.iter().any(|(_, t)| matches!(t, ReaderTarget::SetStatusFor(..))), "no status picker for a book");
    }

    #[test]
    fn tapping_a_file_requests_open_then_shows_reading() {
        let mut model = ReaderModel::new(vault(5));
        let eff = tap(&mut model, |t| matches!(t, ReaderTarget::OpenFile(0)));
        assert_eq!(eff, vec![ReaderEffect::Open(0)]);
        assert!(model.busy, "busy until Opened arrives");
        open_sample(&mut model, "# Hi\n\nsome words here to read");
        assert!(!model.busy);
        assert!(matches!(model.screen, Screen::Reading(_)));
    }

    #[test]
    fn list_paging_advances_and_back_returns_near_file() {
        let mut model = ReaderModel::new(vault(200));
        tap(&mut model, |t| *t == ReaderTarget::ListNext);
        let per = list_rows_per_page(1696);
        assert!(matches!(model.screen, Screen::List { scroll } if scroll == per));
        // Open a file, then Back should scroll the list to that file's page.
        model.busy = false;
        update(&mut model, ReaderEvent::Opened { file_ix: 100, source: "hello world".into(), doc: parse("hello world"), pages: layout(&parse("hello world"), &fonts(), LayoutOpts::default()) }, &[], &fonts(), CANVAS);
        tap(&mut model, |t| *t == ReaderTarget::Back);
        let pos = model.visible().iter().position(|&i| i == 100).unwrap();
        assert!(matches!(model.screen, Screen::List { scroll } if scroll == (pos / per) * per));
    }

    #[test]
    fn page_nav_moves_and_clamps() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &("word ".repeat(1500)));
        let total = if let Screen::Reading(r) = &model.screen { r.pages.len() } else { 0 };
        assert!(total > 2);
        tap(&mut model, |t| *t == ReaderTarget::NextPage);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 1));
    }

    #[test]
    fn edge_swipe_turns_the_page() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &("word ".repeat(1500)));
        assert!(matches!(&model.screen, Screen::Reading(r) if r.pages.len() > 2));
        let swipe = |m: &mut ReaderModel, x: i32, dir: i32| {
            update(m, ReaderEvent::Swipe { x, y: 850, dir }, &[], &fonts(), CANVAS)
        };
        // Right edge, leftward → next.
        swipe(&mut model, 820, 1);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 1), "right-edge swipe advances");
        // Left edge, rightward → previous.
        swipe(&mut model, 120, -1);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 0), "left-edge swipe goes back");
        // Middle of the page → no page turn.
        swipe(&mut model, 477, 1);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 0), "center swipe is a no-op");
        // Correct direction but wrong edge (next from the left third) → no page turn.
        swipe(&mut model, 120, 1);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 0), "wrong-edge swipe is a no-op");
    }

    #[test]
    fn tapping_the_centre_toggles_focus() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(400));
        // Normal reading view shows chrome: a Back chip and page nav are hittable.
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::Back), "chrome visible before focus");

        // A centre tap flips into focus and asks the runtime to re-paginate full-screen (pinned to
        // the current top line via LayoutOpts::force_break_at, so the reader keeps their place).
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)], "entering focus moves nothing — no relayout");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.focus), "focus on");

        // In focus the view drops all header/footer chrome but keeps the body tap zones — the centre
        // focus toggle and the side-tap page turns (so you can still page full-screen).
        let h = hits(&model);
        assert!(!h.iter().any(|(_, t)| *t == ReaderTarget::Back), "no Back chip in focus");
        assert!(!h.iter().any(|(_, t)| *t == ReaderTarget::OpenMore), "no More button in focus");
        assert!(!h.iter().any(|(_, t)| *t == ReaderTarget::OpenContents), "no Contents button in focus");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::ToggleFocus), "centre toggle still present");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::NextPage), "side-tap page nav persists in focus");

        // A second centre tap turns focus back off and the chrome returns.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)], "leaving from the entry overlay moves nothing");
        assert!(matches!(&model.screen, Screen::Reading(r) if !r.focus), "focus off");
        assert!(hits(&model).iter().any(|(_, t)| *t == ReaderTarget::Back), "chrome returns");
    }

    #[test]
    fn the_first_focus_turn_continues_exactly_after_the_entry_window() {
        // Enter focus mid-doc (moves nothing), then turn forward: the model emits FocusPaginate at
        // the entry window's end; the runtime-shaped relayout must open the new page at exactly that
        // byte — nothing repeated, nothing skipped.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(1500));
        update(&mut model, ReaderEvent::Swipe { x: 820, y: 850, dir: 1 }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::Swipe { x: 820, y: 850, dir: 1 }, &[], &fonts(), CANVAS);
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)], "entering focus moves nothing");
        let (doc, window) = match &model.screen {
            Screen::Reading(r) => (r.doc.clone(), crate::view::focus_entry_window(&r.pages, r.page_ix, CANVAS.height as f32)),
            _ => unreachable!(),
        };
        // Turn forward: the transition effect, anchored at the first unseen byte.
        let eff = update(&mut model, ReaderEvent::Swipe { x: 820, y: 850, dir: 1 }, &[], &fonts(), CANVAS);
        let (anchor, forward) = match &eff[..] {
            [ReaderEffect::FocusPaginate { anchor, forward }] => (*anchor, *forward),
            other => panic!("expected FocusPaginate, got {other:?}"),
        };
        assert!(forward);
        assert_eq!(anchor, window.end, "the new page starts after the last visible line");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.focus_paginated), "focus-paginated sub-state entered");
        // Runtime-shaped relayout: focus grid + forced break at the anchor.
        let fopts = LayoutOpts { content_top: 14.0, content_bottom: CANVAS.height as f32 - 56.0, force_break_at: Some(anchor), ..LayoutOpts::default() };
        let pages = layout(&doc, &fonts(), fopts);
        // Mirrors main.rs's FocusPaginate arm: the anchored page is the first one starting >= anchor.
        let page_ix = pages.iter().position(|p| p.runs().map(|g| g.src.start).min().unwrap_or(usize::MAX) >= anchor).unwrap();
        update(&mut model, ReaderEvent::Relaid { pages, page_ix }, &[], &fonts(), CANVAS);
        let (top, skipped) = match &model.screen {
            Screen::Reading(r) => {
                let top = r.pages[r.page_ix].runs().map(|g| g.src.start).min().unwrap();
                (top, r.source[window.end..top].to_string())
            }
            _ => unreachable!(),
        };
        assert!(top >= window.end, "no repeat across the transition: {top} < {}", window.end);
        assert!(skipped.trim().is_empty(), "only inter-word whitespace between the entry window and the new page, got {skipped:?}");

        // Toggling off from here re-anchors the (new) top line back into the normal layout.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        let anchor = match &eff[..] {
            [ReaderEffect::RestoreLayout { anchor }] => *anchor,
            other => panic!("expected RestoreLayout, got {other:?}"),
        };
        assert_eq!(anchor, top, "restore pins the current top line");
        let nopts = LayoutOpts { force_break_at: Some(anchor), ..LayoutOpts::default() };
        let pages = layout(&doc, &fonts(), nopts);
        let page_ix = crate::layout::page_of_byte(&pages, anchor);
        update(&mut model, ReaderEvent::Relaid { pages, page_ix }, &[], &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if !r.focus && !r.focus_paginated));
        let restored = match &model.screen {
            Screen::Reading(r) => r.pages[r.page_ix].runs().map(|g| g.src.start).min().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(restored, anchor, "leaving focus keeps the same top line");
    }

    #[test]
    fn a_backward_first_focus_turn_ends_exactly_before_the_entry_window() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(1500));
        update(&mut model, ReaderEvent::Swipe { x: 820, y: 850, dir: 1 }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::Swipe { x: 820, y: 850, dir: 1 }, &[], &fonts(), CANVAS);
        tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        let window = match &model.screen {
            Screen::Reading(r) => crate::view::focus_entry_window(&r.pages, r.page_ix, CANVAS.height as f32),
            _ => unreachable!(),
        };
        let eff = update(&mut model, ReaderEvent::Swipe { x: 120, y: 850, dir: -1 }, &[], &fonts(), CANVAS);
        match &eff[..] {
            [ReaderEffect::FocusPaginate { anchor, forward: false }] => {
                assert_eq!(*anchor, window.start, "the previous page ends right before the first visible line");
            }
            other => panic!("expected a backward FocusPaginate, got {other:?}"),
        }
    }

    #[test]
    fn the_entry_turn_no_ops_at_the_document_edges() {
        // Page 1 of a doc, focus entry: turning back has nowhere to go.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(1500));
        tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        let eff = update(&mut model, ReaderEvent::Swipe { x: 120, y: 850, dir: -1 }, &[], &fonts(), CANVAS);
        assert!(eff.is_empty(), "backward at the start no-ops, got {eff:?}");
        assert!(matches!(&model.screen, Screen::Reading(r) if !r.focus_paginated), "still on the entry overlay");
    }

    #[test]
    fn side_taps_turn_pages() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(1500)); // multi-page
        assert!(matches!(&model.screen, Screen::Reading(r) if r.pages.len() > 2));
        // Right-side body tap → next; left-side body tap → prev. (`tap` finds the body zone, which is
        // pushed before the footer chip.)
        tap(&mut model, |t| *t == ReaderTarget::NextPage);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 1), "right-side tap advances");
        tap(&mut model, |t| *t == ReaderTarget::PrevPage);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 0), "left-side tap goes back");
        // The first page has no prev zone (gated on page_ix > 0).
        assert!(!hits(&model).iter().any(|(_, t)| *t == ReaderTarget::PrevPage), "no prev zone on page 1");
    }

    #[test]
    fn paging_auto_advances_read_status() {
        let mut model = ReaderModel::new(vault(1)); // starts NotRead
        open_sample(&mut model, &"word ".repeat(1500));
        let total = if let Screen::Reading(r) = &model.screen { r.pages.len() } else { 0 };
        assert!(total > 2, "need a multi-page doc for the thresholds");

        // Same path Prev/Next taps and edge swipes take.
        let turn = |m: &mut ReaderModel, dir: i32| {
            update(m, ReaderEvent::Swipe { x: if dir > 0 { 820 } else { 120 }, y: 850, dir }, &[], &fonts(), CANVAS)
        };
        let set = |status| ReaderEffect::SetStatus { file_ix: 0, status, quiet: true };

        // Page 0 → 1 crosses into Reading (emitted next to the redraw; paging stays non-blocking).
        let eff = turn(&mut model, 1);
        assert!(eff.contains(&set(ReadStatus::Reading)), "page 2 → reading, got {eff:?}");
        assert!(!eff.contains(&set(ReadStatus::Read)), "not Read until the final page");
        assert!(!model.busy, "auto-status must not block further paging");

        // Walk to the last page: only the final turn crosses into Read.
        let mut last = Vec::new();
        while !matches!(&model.screen, Screen::Reading(r) if r.page_ix + 1 == total) {
            last = turn(&mut model, 1);
        }
        assert!(last.contains(&set(ReadStatus::Read)), "final page → read, got {last:?}");

        // Monotonic: once the vault marker reads Read, paging backward never downgrades it.
        model.vault[0].status = ReadStatus::Read;
        let eff = turn(&mut model, -1);
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::SetStatus { .. })), "no downgrade paging back, got {eff:?}");

        // Landing back on page 1 (index 0) never emits a status at all.
        while matches!(&model.screen, Screen::Reading(r) if r.page_ix > 0) {
            let eff = turn(&mut model, -1);
            assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::SetStatus { .. })), "backward paging never emits a status downgrade");
        }
    }

    #[test]
    fn a_quiet_status_install_swaps_state_without_flash_or_redraw() {
        // The deferred page-turn auto-advance: the runtime writes the file after the turn painted and
        // feeds a quiet StatusSet — new source/pages install (offsets shifted with the frontmatter
        // edit) but nothing repaints: the re-laid pages are pixel-identical to what's on screen.
        let mut model = ReaderModel::new(vault(1));
        let src = "word ".repeat(400);
        open_sample(&mut model, &src);
        let new_src = format!("---\nstatus: reading\n---\n{src}");
        let doc = parse(&new_src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let eff = update(&mut model, ReaderEvent::StatusSet { source: new_src.clone(), doc, pages, status: ReadStatus::Reading, quiet: true }, &[], &fonts(), CANVAS);
        assert!(eff.is_empty(), "a quiet install emits nothing, got {eff:?}");
        assert!(model.flash.is_none(), "no flash for a quiet install");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.source == new_src), "the shifted source is installed");
        assert_eq!(model.vault[0].status, ReadStatus::Reading, "the list marker still updates");
        // The manual (More-sheet) path keeps its feedback.
        let doc = parse(&new_src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let eff = update(&mut model, ReaderEvent::StatusSet { source: new_src, doc, pages, status: ReadStatus::Read, quiet: false }, &[], &fonts(), CANVAS);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)], "a manual set still redraws");
        assert!(model.flash.is_some(), "a manual set still flashes");
    }

    #[test]
    fn precomputed_lowercase_keys_match_the_on_the_fly_oracle() {
        // `visible()` sorts/filters on precomputed `title_lower`/`byline_lower`; ordering and search
        // matches must equal recomputing `.to_lowercase()` on the spot (the pre-v0.11.2 behavior).
        let mut model = ReaderModel::new(search_vault());
        for f in &model.vault {
            assert_eq!(f.title_lower, f.title.to_lowercase());
            assert_eq!(f.byline_lower, f.byline.as_deref().map(str::to_lowercase));
        }
        for (query, sort) in [
            ("", SortKey::Title),
            ("the", SortKey::Title),
            ("SOURCE", SortKey::Recent),
            ("guardian", SortKey::Recent),
            ("", SortKey::Added),
            ("the", SortKey::Added),
        ] {
            model.query = query.into();
            model.sort = sort;
            let got = model.visible();
            // Oracle: filter + sort with fresh lowercasing.
            let q = query.trim().to_lowercase();
            let mut want: Vec<usize> = (0..model.vault.len())
                .filter(|&i| {
                    let f = &model.vault[i];
                    q.is_empty()
                        || f.title.to_lowercase().contains(&q)
                        || f.byline.as_deref().is_some_and(|b| b.to_lowercase().contains(&q))
                })
                .collect();
            let title = |i: usize| model.vault[i].title.to_lowercase();
            match sort {
                SortKey::Title => want.sort_by(|&a, &b| title(a).cmp(&title(b))),
                SortKey::Recent => want.sort_by(|&a, &b| {
                    let key = |i: usize| model.vault[i].published.as_deref().unwrap_or("");
                    key(b).cmp(key(a)).then_with(|| title(a).cmp(&title(b)))
                }),
                SortKey::Added => want.sort_by(|&a, &b| {
                    let key = |i: usize| {
                        let f = &model.vault[i];
                        f.created.as_deref().or(f.published.as_deref()).unwrap_or("")
                    };
                    key(b).cmp(key(a)).then_with(|| title(a).cmp(&title(b)))
                }),
            }
            assert_eq!(got, want, "query {query:?} sort {sort:?}");
        }
    }

    #[test]
    fn auto_status_target_thresholds() {
        use ReadStatus::*;
        // Page 0 of a multi-page doc never triggers, whatever the current state.
        assert_eq!(auto_status_target(0, 5, NotRead), None);
        // Interior pages → Reading; the final page → Read.
        assert_eq!(auto_status_target(1, 5, NotRead), Some(Reading));
        assert_eq!(auto_status_target(4, 5, NotRead), Some(Read));
        // Upgrade-only: a page 2 arrival won't pull a Read article back to Reading.
        assert_eq!(auto_status_target(1, 5, Read), None);
        assert_eq!(auto_status_target(1, 5, Reading), None);
        // A single-page doc: its only page IS the last page — opening it reads it (the open
        // path calls this with (0, 1)); still upgrade-only.
        assert_eq!(auto_status_target(0, 1, NotRead), Some(Read));
        assert_eq!(auto_status_target(0, 1, Read), None);
    }

    #[test]
    fn pen_drag_over_plain_text_requests_add_highlight() {
        let mut model = ReaderModel::new(vault(1));
        let src = "The quick brown fox jumps over the lazy dog and keeps on running for a while.";
        open_sample(&mut model, src);
        // Find two runs on the first page and drag between them.
        let (ax, ay, bx, by) = if let Screen::Reading(r) = &model.screen {
            let page = &r.pages[0];
            let first = page.runs().find(|r| r.text == "quick").unwrap();
            let last = page.runs().find(|r| r.text == "fox").unwrap();
            ((first.x + 1.0) as i32, (first.baseline - 5.0) as i32, (last.x + last.w - 1.0) as i32, (last.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        update(&mut model, ReaderEvent::PenDown { x: ax, y: ay }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::PenMove { x: bx, y: by }, &[], &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: bx, y: by }, &[], &fonts(), CANVAS);
        match &eff[..] {
            [ReaderEffect::Highlight { range, add, .. }] => {
                assert!(*add, "plain text → add");
                assert_eq!(&src[range.clone()], "quick brown fox");
            }
            other => panic!("expected one add-highlight effect, got {other:?}"),
        }
        assert!(model.busy);
    }

    #[test]
    fn pen_tap_without_drag_does_nothing() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "hello there world of words");
        let (x, y) = if let Screen::Reading(r) = &model.screen {
            let run = r.pages[0].runs().next().unwrap();
            ((run.x + 2.0) as i32, (run.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        update(&mut model, ReaderEvent::PenDown { x, y }, &[], &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x, y }, &[], &fonts(), CANVAS);
        assert!(eff.is_empty(), "a pen tap is not a highlight");
        assert!(!model.busy);
    }

    #[test]
    fn snap_extends_partial_words_to_boundaries() {
        let s = "The lazy brown fox";
        let a = s.find('e').unwrap(); // inside "The"
        let b = s.find("row").unwrap() + 1; // inside "brown"
        assert_eq!(&s[super::snap_to_words(s, a..b)], "The lazy brown");
        // A clean word range is unchanged.
        let q = s.find("lazy").unwrap();
        assert_eq!(super::snap_to_words(s, q..q + 4), q..q + 4);
    }

    #[test]
    fn a_pen_drag_across_an_existing_highlight_opens_its_menu() {
        // A pen scrub that stays inside an existing highlight opens that highlight's menu (the pen's
        // path to Note / Delete). It never re-highlights (dragging over a highlight can't add anything)
        // and never goes busy — opening the menu is a pure model change.
        let mut model = ReaderModel::new(vault(1));
        let src = "The ==quick brown fox== jumps over the lazy dog and keeps on running for a while.";
        open_sample(&mut model, src);
        let (ax, ay, bx, by) = if let Screen::Reading(r) = &model.screen {
            let page = &r.pages[0];
            let first = page.runs().find(|r| r.text.contains("quick")).unwrap();
            let last = page.runs().find(|r| r.text.contains("fox")).unwrap();
            ((first.x + 1.0) as i32, (first.baseline - 5.0) as i32, (last.x + last.w - 1.0) as i32, (last.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        update(&mut model, ReaderEvent::PenDown { x: ax, y: ay }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::PenMove { x: bx, y: by }, &[], &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: bx, y: by }, &[], &fonts(), CANVAS);
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::Highlight { .. })), "never re-highlights; got {eff:?}");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "the highlight menu is open");
        assert!(!model.busy, "opening the menu doesn't go busy");
    }

    #[test]
    fn text_size_taps_request_a_rescale() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, &"word ".repeat(50));
        // Aa−/Aa+ live in the ⋯ More sheet now; open it before tapping "Larger".
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        let eff = tap(&mut model, |t| *t == ReaderTarget::TextLarger);
        assert_eq!(eff, vec![ReaderEffect::Rescale { delta: 1 }]);
        assert!(model.busy, "busy until the runtime relays out");
        let pages = layout(&parse("word word"), &fonts(), LayoutOpts { scale: 1.2, ..Default::default() });
        update(&mut model, ReaderEvent::Relaid { pages, page_ix: 0 }, &[], &fonts(), CANVAS);
        assert!(!model.busy);
    }

    #[test]
    fn picking_a_status_writes_that_state_and_reflects_it_in_the_list() {
        let mut model = ReaderModel::new(vault(1)); // starts NotRead
        open_sample(&mut model, "---\nauthor: X\n---\nBody words here.");
        // The status picker lives in the ⋯ More sheet; open it, then tap "Read" directly (not a cycle).
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        let eff = tap(&mut model, |t| matches!(t, ReaderTarget::SetStatusFor(_, ReadStatus::Read)));
        match &eff[..] {
            [ReaderEffect::SetStatus { file_ix, status, quiet: false }] => {
                assert_eq!(*file_ix, 0);
                assert_eq!(*status, ReadStatus::Read, "the tapped state, not the next one");
            }
            other => panic!("expected SetStatus, got {other:?}"),
        }
        assert!(model.busy);
        let src = "---\nauthor: X\nstatus: read\n---\nBody words here.";
        let (doc, pages) = (parse(src), layout(&parse(src), &fonts(), LayoutOpts::default()));
        update(&mut model, ReaderEvent::StatusSet { source: src.into(), doc, pages, status: ReadStatus::Read, quiet: false }, &[], &fonts(), CANVAS);
        assert!(!model.busy);
        assert_eq!(model.vault[0].status, ReadStatus::Read, "the list row now reads 'read'");
    }

    #[test]
    fn tapping_the_active_status_segment_is_a_noop() {
        let mut model = ReaderModel::new(vault(1)); // NotRead — the "Not read" row is active
        open_sample(&mut model, "---\nauthor: X\n---\nBody.");
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        let eff = tap(&mut model, |t| matches!(t, ReaderTarget::SetStatusFor(_, ReadStatus::NotRead)));
        assert!(eff.is_empty(), "tapping the already-active state does nothing");
        assert!(!model.busy);
    }

    #[test]
    fn status_can_be_set_from_the_list_without_opening() {
        let mut model = ReaderModel::new(vault(3)); // all NotRead
        // Tap the first row's status gutter → cycles to the next state (Reading).
        let eff = tap(&mut model, |t| matches!(t, ReaderTarget::SetStatusFor(0, _)));
        assert_eq!(eff, vec![ReaderEffect::SetStatus { file_ix: 0, status: ReadStatus::Reading, quiet: false }]);
        assert!(model.busy);
        assert!(matches!(model.screen, Screen::List { .. }), "the note is not opened");
        // The runtime writes it (file wasn't open) and reports back via StatusMarked.
        update(&mut model, ReaderEvent::StatusMarked { file_ix: 0, status: ReadStatus::Reading, quiet: false }, &[], &fonts(), CANVAS);
        assert!(!model.busy);
        assert_eq!(model.vault[0].status, ReadStatus::Reading);
        assert!(matches!(model.screen, Screen::List { .. }), "still on the list");
    }

    #[test]
    fn filter_and_sort_taps_update_the_model_and_visible_set() {
        let mut model = ReaderModel::new(vault(6));
        model.vault[1].status = ReadStatus::Reading;
        model.vault[4].status = ReadStatus::Reading;
        // Filters live in the shelf; open it to tap them (applying a filter closes it again).
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetFilter(StatusFilter::Reading));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.filter, StatusFilter::Reading);
        assert!(!model.shelf_open, "applying a filter closes the shelf");
        assert_eq!(model.visible(), vec![1, 4], "only the reading files");
        // The SORT section carries one row per key — each explicitly selectable.
        for want in [SortKey::Added, SortKey::Recent, SortKey::Title] {
            model.shelf_open = true;
            tap(&mut model, |t| *t == ReaderTarget::SetSort(want));
            assert_eq!(model.sort, want);
            assert!(!model.shelf_open, "picking a sort closes the shelf");
        }
    }

    #[test]
    fn opening_under_a_filter_uses_the_real_vault_index() {
        let mut model = ReaderModel::new(vault(6));
        model.vault[3].status = ReadStatus::Read;
        model.filter = StatusFilter::Read;
        // The one visible row must open index 3, not row 0.
        let eff = tap(&mut model, |t| matches!(t, ReaderTarget::OpenFile(_)));
        assert_eq!(eff, vec![ReaderEffect::Open(3)]);
    }

    #[test]
    fn more_sheet_opens_toggles_tags_and_writes_keeping_it_open() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "---\nauthor: X\n---\nBody.");
        // The ⋯ More button opens the sheet.
        let eff = tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.more_open), "sheet open");
        // Toggling "Health" asks the runtime to write just that tag.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleTag(Tag::Health));
        assert_eq!(eff, vec![ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Health] }]);
        assert!(model.busy);
        // Runtime writes it + re-lays-out and reports back.
        let src = "---\nauthor: X\ntags:\n  - health\n---\nBody.";
        let (doc, pages) = (parse(src), layout(&parse(src), &fonts(), LayoutOpts::default()));
        update(&mut model, ReaderEvent::TagsSet { source: src.into(), doc, pages, tags: vec![Tag::Health] }, &[], &fonts(), CANVAS);
        assert!(!model.busy);
        assert_eq!(model.vault[0].tags, vec![Tag::Health], "list reflects the new tag");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.more_open), "sheet stays open for more toggles");
        // The Close (X) button closes it. (Both it and the full-screen scrim map to CloseMore; the
        // scrim's centre sits over a row, so tap the Close button — the last CloseMore hit — directly.)
        let h = hits(&model);
        let close = h.iter().filter(|(_, t)| *t == ReaderTarget::CloseMore).last().expect("Close present");
        let (cx, cy) = close.0.center();
        let eff = update(&mut model, ReaderEvent::Tap { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(matches!(&model.screen, Screen::Reading(r) if !r.more_open), "sheet closed");
    }

    #[test]
    fn toggling_tags_builds_the_managed_set_in_canonical_order() {
        let mut model = ReaderModel::new(vault(1));
        model.vault[0].tags = vec![Tag::Work];
        open_sample(&mut model, "---\ntags:\n  - work\n---\nBody.");
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        // Adding AI to {Work} yields {AI, Work} in ALL order (not append order).
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleTag(Tag::Ai));
        assert_eq!(eff, vec![ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Ai, Tag::Work] }]);
    }

    #[test]
    fn toggling_a_present_tag_removes_it() {
        let mut model = ReaderModel::new(vault(1));
        model.vault[0].tags = vec![Tag::Ai, Tag::Health];
        open_sample(&mut model, "---\ntags:\n  - ai\n  - health\n---\nBody.");
        tap(&mut model, |t| *t == ReaderTarget::OpenMore);
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleTag(Tag::Ai));
        assert_eq!(eff, vec![ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Health] }]);
    }

    #[test]
    fn archived_clippings_are_hidden_by_default_and_shown_in_the_archived_view() {
        let mut model = ReaderModel::new(vault(4)); // titles File 0..3, already sorted
        model.vault[1].tags = vec![Tag::Ai];
        model.vault[2].tags = vec![Tag::Archive];
        model.vault[3].tags = vec![Tag::Ai, Tag::Archive];
        // Default view hides anything archived.
        assert_eq!(model.visible(), vec![0, 1], "archived clippings drop off the list");
        // A content filter also hides archived matches.
        model.tag_filter = Some(Tag::Ai);
        assert_eq!(model.visible(), vec![1], "an archived AI note stays hidden under the AI filter");
        // The Archived view shows exactly the archived ones.
        model.tag_filter = Some(Tag::Archive);
        assert_eq!(model.visible(), vec![2, 3]);
    }

    #[test]
    fn the_archived_toggle_flips_the_tag_filter() {
        let mut model = ReaderModel::new(vault(3));
        // Off → on: the shelf's "Archived" row sets the archive filter.
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(Some(Tag::Archive)));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.tag_filter, Some(Tag::Archive));
        // On → off: tapping it again clears the filter (it now maps to SetTagFilter(None)).
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(None));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.tag_filter, None);
    }

    #[test]
    fn the_archive_button_writes_the_archive_tag_and_returns_to_the_list() {
        let mut model = ReaderModel::new(vault(3));
        open_sample(&mut model, "---\nauthor: X\n---\nBody."); // opens file_ix 0
        // The footer Archive pill archives AND drops straight back to the list.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleTag(Tag::Archive));
        assert_eq!(eff, vec![ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Archive] }]);
        assert!(matches!(model.screen, Screen::List { .. }), "dropped back to the list");
        assert!(model.busy);
        // The runtime writes it while we're off the doc → TagsMarked feedback updates the row.
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 0, tags: vec![Tag::Archive] }, &[], &fonts(), CANVAS);
        assert!(!model.busy);
        assert_eq!(model.vault[0].tags, vec![Tag::Archive]);
        assert!(!model.visible().contains(&0), "archived file is hidden from the default list");
    }

    #[test]
    fn favouriting_toggles_in_place_and_preserves_other_tags() {
        let mut model = ReaderModel::new(vault(3));
        model.vault[0].tags = vec![Tag::Ai];
        open_sample(&mut model, "---\ntags:\n  - ai\n---\nBody."); // opens file_ix 0
        // The header Favourite pill toggles the tag in place — unlike Archive, it does NOT drop to the
        // list, and it preserves the existing content tag (managed set rebuilt in ALL order).
        let eff = tap(&mut model, |t| *t == ReaderTarget::ToggleTag(Tag::Favourite));
        assert_eq!(eff, vec![ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Ai, Tag::Favourite] }]);
        assert!(matches!(model.screen, Screen::Reading(_)), "still reading after favouriting");
        assert!(model.busy);
    }

    #[test]
    fn the_fav_toggle_flips_the_favourite_filter() {
        let mut model = ReaderModel::new(vault(3));
        // Off → on: the shelf's "Favourites" row sets the favourite filter.
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(Some(Tag::Favourite)));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.tag_filter, Some(Tag::Favourite));
        // On → off: tapping it again clears the filter.
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(None));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.tag_filter, None);
    }

    #[test]
    fn the_menu_opens_the_shelf_and_tapping_outside_closes_it() {
        let mut model = ReaderModel::new(vault(6));
        assert!(!model.shelf_open, "shelf starts closed");
        // The header menu icon opens the shelf.
        let eff = tap(&mut model, |t| *t == ReaderTarget::OpenShelf);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(model.shelf_open, "menu opened the shelf");
        // The shelf's filters are now hit-testable; the status filters + sort + tag rows all appear.
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| matches!(t, ReaderTarget::SetFilter(_))), "status filters in the open shelf");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::CloseShelf), "a tap-outside scrim closes it");
        // Tapping the scrim (top-left corner, outside every row) closes the shelf.
        let scrim = h.iter().find(|(_, t)| *t == ReaderTarget::CloseShelf).unwrap().0;
        let eff = update(&mut model, ReaderEvent::Tap { x: scrim.x + 2, y: scrim.y + 2 }, &h, &fonts(), CANVAS);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(!model.shelf_open, "tapping the scrim closed the shelf");
    }

    #[test]
    fn favouriting_does_not_hide_from_the_list_but_the_fav_view_narrows_to_favourites() {
        let mut model = ReaderModel::new(vault(4)); // File 0..3
        model.vault[1].tags = vec![Tag::Favourite];
        model.vault[2].tags = vec![Tag::Favourite, Tag::Archive];
        // Favouriting keeps a clipping in the default list (unlike Archive); the archived one still drops.
        assert_eq!(model.visible(), vec![0, 1, 3], "favourited stays visible; only the archived one hides");
        // The Fav view shows every favourite — archived or not (a curated collection survives tidying).
        model.tag_filter = Some(Tag::Favourite);
        assert_eq!(model.visible(), vec![1, 2], "both favourites, including the archived one");
    }

    #[test]
    fn tag_filter_narrows_the_list_and_ands_with_status() {
        let mut model = ReaderModel::new(vault(6));
        model.vault[1].tags = vec![Tag::Ai];
        model.vault[2].tags = vec![Tag::Ai];
        model.vault[2].status = ReadStatus::Read;
        model.vault[4].tags = vec![Tag::Health];
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(Some(Tag::Ai)));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.tag_filter, Some(Tag::Ai));
        assert_eq!(model.visible(), vec![1, 2], "only AI-tagged");
        // AND with a status filter.
        model.filter = StatusFilter::Read;
        assert_eq!(model.visible(), vec![2], "AI AND read");
        // Clearing the tag filter restores the (status-only) set: the now-active AI topic row maps
        // to SetTagFilter(None).
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(None));
        assert_eq!(model.tag_filter, None);
        assert_eq!(model.visible(), vec![2], "still status-filtered to Read");
    }

    // ---- Author filter + picker ----

    #[test]
    fn authors_lists_distinct_bylines_most_clipped_first_excluding_archived() {
        let mut model = ReaderModel::new(search_vault());
        model.vault[0].byline = Some("Tokio".into()); // now two Tokio clippings
        model.vault[1].tags = vec![Tag::Archive]; // Real Python archived → excluded
        assert_eq!(model.authors(), vec![("Tokio".to_string(), 2)]);
        model.vault[1].tags = Vec::new();
        assert_eq!(model.authors(), vec![("Tokio".to_string(), 2), ("Real Python".to_string(), 1)]);
    }

    #[test]
    fn author_filter_taps_from_the_shelf_narrow_and_toggle_off() {
        // Ties sort A–Z, so the two inline shelf authors are "Real Python" and "The Rust Blog".
        let mut model = ReaderModel::new(search_vault());
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(Some("Real Python".into())));
        assert_eq!(model.author_filter.as_deref(), Some("Real Python"));
        assert!(!model.shelf_open, "applying an author filter closes the shelf");
        assert_eq!(model.visible(), vec![1], "only the Real Python clipping");
        // The active author's shelf row maps to SetAuthorFilter(None) — tapping it clears.
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(None));
        assert_eq!(model.author_filter, None);
        assert_eq!(model.visible().len(), 4, "back to the full list");
    }

    #[test]
    fn archiving_an_authors_whole_output_never_strands_the_filter() {
        // The original bug: filter to an author, select all, archive. Every one of that author's
        // clippings leaves `authors()`, so the shelf row that would toggle the filter off is gone.
        let mut model = ReaderModel::new(search_vault());
        model.vault[0].byline = Some("Tokio".into()); // Tokio now owns rows 0 and 2
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(Some("Tokio".into())));
        assert_eq!(model.visible(), vec![2, 0], "the two Tokio clippings");

        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&[2usize, 0][..]));
        tap(&mut model, |t| *t == ReaderTarget::ArchiveSelected);
        for ix in [2usize, 0] {
            update(&mut model, ReaderEvent::TagsMarked { file_ix: ix, tags: vec![Tag::Archive] }, &[], &fonts(), CANVAS);
        }

        // The filter emptied the library, so it goes — and only it: nothing else was narrowing.
        assert_eq!(model.author_filter, None, "the stale author filter cleared itself");
        assert_eq!(model.visible(), vec![3, 1], "the rest of the library is back");
        assert_eq!(model.flash.as_ref().map(|f| f.text.as_str()), Some("Author filter cleared"));
    }

    #[test]
    fn the_shelf_still_offers_a_toggle_off_for_an_all_archived_author() {
        // Belt to the auto-clear's braces: reach the state directly (a filter restored from prefs,
        // say) and the shelf must still draw the row that clears it.
        let mut model = ReaderModel::new(search_vault());
        model.vault[2].tags = vec![Tag::Archive]; // Tokio's only clipping
        model.author_filter = Some("Tokio".into());
        assert!(!model.authors().iter().any(|(n, _)| n == "Tokio"), "gone from the author list");
        model.shelf_open = true;
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::SetAuthorFilter(None)), "no way to clear the filter");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::OpenAuthors), "picker door still open");
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(None));
        assert_eq!(model.author_filter, None);
    }

    #[test]
    fn the_authors_shelf_section_survives_having_no_authors_left() {
        // The unrecoverable variant: no live clipping carries a byline at all, so `authors()` is
        // empty. The section used to collapse, taking the picker door with it.
        let mut model = ReaderModel::new(search_vault());
        for f in &mut model.vault {
            f.tags = vec![Tag::Archive];
        }
        model.author_filter = Some("Tokio".into());
        assert!(model.authors().is_empty());
        model.shelf_open = true;
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::SetAuthorFilter(None)), "stranded with no way out");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::OpenAuthors));
    }

    #[test]
    fn an_empty_filtered_list_offers_to_clear_the_filters() {
        let mut model = ReaderModel::new(search_vault());
        model.author_filter = Some("Nobody".into());
        model.filter = StatusFilter::Read;
        model.query = "zzz".into();
        assert!(model.visible().is_empty());
        tap(&mut model, |t| *t == ReaderTarget::ClearFilters);
        assert_eq!(model.author_filter, None);
        assert_eq!(model.filter, StatusFilter::All);
        assert!(model.query.is_empty());
        assert_eq!(model.visible().len(), 4, "the whole library back in one tap");

        // Empty because the vault is empty is not a filter problem — no chip.
        let bare = ReaderModel::new(Vec::new());
        assert!(!hits(&bare).iter().any(|(_, t)| *t == ReaderTarget::ClearFilters));
        // Nor when the list has rows.
        assert!(!hits(&model).iter().any(|(_, t)| *t == ReaderTarget::ClearFilters));
    }

    #[test]
    fn the_auto_clear_drops_only_as_much_as_it_has_to() {
        // Author + Unread: archiving that author's unread clippings should cost the author filter
        // and leave Unread standing. (The Read view is deliberately different — it keeps archived
        // clippings, being the reading history — so it can't empty this way.)
        let mut model = ReaderModel::new(search_vault());
        model.filter = StatusFilter::NotRead;
        model.author_filter = Some("The Rust Blog".into());
        assert_eq!(model.visible(), vec![0]);
        model.vault[0].tags = vec![Tag::Archive];
        assert_eq!(model.clear_filters_until_visible(), Some("Author filter cleared"));
        assert_eq!(model.filter, StatusFilter::NotRead, "Unread survived");
        assert_eq!(model.visible(), vec![3], "the other unread clipping");
        // Nothing to do when the view is already populated.
        assert_eq!(model.clear_filters_until_visible(), None);
    }

    #[test]
    fn a_saved_author_filter_needs_a_live_clipping_not_just_a_byline() {
        // The startup guard's predicate (bin/reader-app restores prefs through this).
        let mut model = ReaderModel::new(search_vault());
        assert!(model.author_has_live_clippings("Tokio"));
        model.vault[2].tags = vec![Tag::Archive];
        assert!(!model.author_has_live_clippings("Tokio"), "archived clippings don't count");
        assert!(!model.author_has_live_clippings("Nobody"));
    }

    #[test]
    fn shelf_swaps_the_active_author_inline_so_it_can_be_toggled_off() {
        // "Tokio" sorts outside the two inline shelf rows — but once active (applied via the
        // picker) the shelf swaps it in, so its row offers the SetAuthorFilter(None) toggle-off.
        let mut model = ReaderModel::new(search_vault());
        model.author_filter = Some("Tokio".into());
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(None));
        assert_eq!(model.author_filter, None);
    }

    #[test]
    fn author_filter_composes_with_the_status_filter() {
        let mut model = ReaderModel::new(search_vault());
        model.filter = StatusFilter::Read; // files 1 + 2
        model.author_filter = Some("Tokio".into()); // file 2 only
        assert_eq!(model.visible(), vec![2]);
    }

    #[test]
    fn author_picker_opens_from_the_shelf_applies_and_closes() {
        let mut model = ReaderModel::new(search_vault());
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::OpenAuthors);
        assert!(matches!(model.screen, Screen::Authors { scroll: 0 }), "shelf → full-screen picker");
        assert!(!model.shelf_open);
        // Tap an author row → the filter applies and we land back on the list at the top.
        tap(&mut model, |t| *t == ReaderTarget::SetAuthorFilter(Some("Real Python".into())));
        assert_eq!(model.author_filter.as_deref(), Some("Real Python"));
        assert!(matches!(model.screen, Screen::List { scroll: 0 }));
        assert_eq!(model.visible(), vec![1]);
        // Close (without picking) also returns to the list, keeping the filter.
        model.screen = Screen::Authors { scroll: 0 };
        tap(&mut model, |t| *t == ReaderTarget::CloseAuthors);
        assert!(matches!(model.screen, Screen::List { scroll: 0 }));
        assert_eq!(model.author_filter.as_deref(), Some("Real Python"), "closing keeps the filter");
    }

    #[test]
    fn added_sort_uses_created_falling_back_to_published() {
        let mut model = ReaderModel::new(search_vault());
        model.sort = SortKey::Added;
        model.vault[3].created = Some("2026-07-31T09:00".into()); // newest capture
        model.vault[0].created = Some("2026-07-30".into());
        // 1 + 2 predate the clipper template (no `created:`) → their published date keys them,
        // tie broken by title (Python basics before Rust async).
        assert_eq!(model.visible(), vec![3, 0, 1, 2]);
    }

    // ---- Library search (on-screen keyboard) ----

    fn search_vault() -> Vec<VaultFile> {
        let mk = |title: &str, byline: Option<&str>, status: ReadStatus| VaultFile {
            kind: crate::model::DocKind::Article,
            path: PathBuf::from(format!("/v/{title}.md")),
            title_lower: title.to_lowercase(),
            title: title.into(),
            has_highlights: false,
            published: Some("2025-01-01".into()),
            created: None,
            byline_lower: byline.map(str::to_lowercase),
            byline: byline.map(Into::into),
            status,
            tags: Vec::new(),
            minutes: 4,
        };
        vec![
            mk("Rust ownership", Some("The Rust Blog"), ReadStatus::NotRead), // 0
            mk("Python basics", Some("Real Python"), ReadStatus::Read),       // 1
            mk("Rust async", Some("Tokio"), ReadStatus::Read),                // 2
            mk("Gardening tips", None, ReadStatus::NotRead),                  // 3
        ]
    }
    fn type_query(model: &mut ReaderModel, q: &str) {
        for c in q.chars() {
            tap(model, |t| *t == ReaderTarget::Key(c));
        }
    }

    #[test]
    fn opening_search_shows_a_finger_sized_keyboard() {
        let mut model = ReaderModel::new(search_vault());
        let eff = tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert!(matches!(model.screen, Screen::Search { .. }));
        let h = hits(&model);
        let keys = h.iter().filter(|(_, t)| matches!(t, ReaderTarget::Key(_))).count();
        assert!(keys >= 27, "26 letters + space, got {keys}");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::Backspace));
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::ClearQuery));
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::CloseSearch));
        for (r, t) in &h {
            if matches!(t, ReaderTarget::Key(_)) {
                assert!(r.h >= crate::view::MIN_TAP_PX && r.w >= crate::view::MIN_TAP_PX, "key {t:?} too small: {r:?}");
            }
        }
    }

    #[test]
    fn a_key_tap_dispatches_key_char_and_builds_the_query() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        // Full stack: hit_test finds the key rect, the tap dispatches Key('r'), on_tap appends it.
        tap(&mut model, |t| *t == ReaderTarget::Key('r'));
        assert_eq!(model.query, "r");
        type_query(&mut model, "ust");
        assert_eq!(model.query, "rust");
        // Live filter: titles "Rust ownership" (0) and "Rust async" (2).
        let mut v = model.visible();
        v.sort();
        assert_eq!(v, vec![0, 2]);
    }

    #[test]
    fn backspace_and_clear_edit_the_query() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        type_query(&mut model, "rust");
        tap(&mut model, |t| *t == ReaderTarget::Backspace);
        assert_eq!(model.query, "rus");
        tap(&mut model, |t| *t == ReaderTarget::ClearQuery);
        assert_eq!(model.query, "");
        assert!(matches!(model.screen, Screen::Search { .. }), "clear keeps the keyboard up");
    }

    #[test]
    fn search_shift_capitalises_the_next_key_then_reverts() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        tap(&mut model, |t| *t == ReaderTarget::KbShift);
        // With Shift on, the letter keys carry uppercase (the shared keyboard pre-cases them).
        assert!(hits(&model).iter().any(|(_, t)| *t == ReaderTarget::Key('R')));
        tap(&mut model, |t| *t == ReaderTarget::Key('R'));
        assert_eq!(model.query, "R");
        assert!(!model.search_shift, "Shift is one-shot — it clears after a key");
        assert!(
            hits(&model).iter().any(|(_, t)| *t == ReaderTarget::Key('r')),
            "the board reverts to lowercase"
        );
    }

    #[test]
    fn search_layer_toggle_swaps_letters_for_digits() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        assert!(hits(&model).iter().any(|(_, t)| *t == ReaderTarget::Key('q')));
        tap(&mut model, |t| *t == ReaderTarget::KbLayer);
        let h = hits(&model);
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::Key('1')), "the symbols layer has digits");
        assert!(!h.iter().any(|(_, t)| *t == ReaderTarget::Key('q')), "letters are hidden on the symbols layer");
    }

    #[test]
    fn closing_search_clears_query_and_returns_to_list() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        type_query(&mut model, "rust");
        tap(&mut model, |t| *t == ReaderTarget::CloseSearch);
        assert!(matches!(model.screen, Screen::List { .. }));
        assert_eq!(model.query, "", "list returns to the full, unfiltered view");
    }

    #[test]
    fn back_from_a_search_result_returns_to_search() {
        let mut model = ReaderModel::new(search_vault());
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        type_query(&mut model, "rust");
        // Enter a result (the runtime's Opened event drives Reading; the query stays on the model).
        let src = "hello world";
        update(&mut model, ReaderEvent::Opened { file_ix: 0, source: src.into(), doc: parse(src), pages: layout(&parse(src), &fonts(), LayoutOpts::default()) }, &[], &fonts(), CANVAS);
        assert!(matches!(model.screen, Screen::Reading(_)));
        tap(&mut model, |t| *t == ReaderTarget::Back);
        assert!(matches!(model.screen, Screen::Search { .. }), "Back returns to the search results");
        assert_eq!(model.query, "rust");
    }

    #[test]
    fn search_query_ands_with_the_status_filter() {
        let mut model = ReaderModel::new(search_vault());
        model.query = "rust".into();
        let mut v = model.visible();
        v.sort();
        assert_eq!(v, vec![0, 2], "both Rust files");
        model.filter = StatusFilter::Read;
        assert_eq!(model.visible(), vec![2], "only the read Rust file");
    }

    #[test]
    fn query_matches_title_and_byline_case_insensitively() {
        let mut model = ReaderModel::new(search_vault());
        model.query = "TOKIO".into(); // byline of "Rust async" (2)
        assert_eq!(model.visible(), vec![2]);
        model.query = "python".into(); // title + byline of "Python basics" (1)
        assert_eq!(model.visible(), vec![1]);
        model.query = "  ".into(); // whitespace-only → no-op filter, shows all
        assert_eq!(model.visible().len(), 4);
    }

    #[test]
    fn refresh_button_starts_a_sync_and_flashes() {
        let mut model = ReaderModel::new(vault(3));
        let eff = tap(&mut model, |t| *t == ReaderTarget::SyncVault);
        assert!(eff.contains(&ReaderEffect::SyncVault), "runtime asked to run vaultsync");
        assert!(eff.contains(&ReaderEffect::Redraw(Dirty::Full)));
        assert_eq!(model.flash.as_ref().map(|f| f.text.as_str()), Some("Syncing…"));
    }

    #[test]
    fn update_checked_flashes_the_status_line() {
        let mut model = ReaderModel::new(vault(1));
        let eff = update(&mut model, ReaderEvent::UpdateChecked { msg: "Up to date (v0.9.2)".into() }, &[], &fonts(), CANVAS);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.flash.as_ref().map(|f| f.text.as_str()), Some("Up to date (v0.9.2)"));
    }

    #[test]
    fn vault_rescan_swaps_the_list_and_remaps_the_open_file() {
        let mut model = ReaderModel::new(vault(3));
        // Open /v/2.md (index 2 in the sorted list).
        model.busy = true;
        let src = "hello world";
        update(&mut model, ReaderEvent::Opened { file_ix: 2, source: src.into(), doc: parse(src), pages: layout(&parse(src), &fonts(), LayoutOpts::default()) }, &[], &fonts(), CANVAS);
        // A sync pulled a new clipping that sorts first — every index shifts by one.
        let mut files = vault(3);
        files.insert(
            0,
            VaultFile {
                kind: crate::model::DocKind::Article,
                path: PathBuf::from("/v/new.md"),
                title_lower: "aaa brand new".into(),
                title: "AAA brand new".into(),
                has_highlights: false,
                published: None,
                created: None,
                byline: None,
                byline_lower: None,
                status: ReadStatus::NotRead,
                tags: Vec::new(),
                minutes: 4,
            },
        );
        let eff = update(&mut model, ReaderEvent::VaultRescanned { files }, &[], &fonts(), CANVAS);
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.vault.len(), 4, "new clipping in the list");
        match &model.screen {
            Screen::Reading(r) => assert_eq!(r.file_ix, 3, "open file remapped by path after the shift"),
            _ => panic!("expected the reading screen to survive the rescan"),
        }
    }

    #[test]
    fn archived_favourites_stay_in_the_favourites_filter() {
        let mut model = ReaderModel::new(vault(3));
        model.vault[1].tags = vec![Tag::Favourite, Tag::Archive];
        // Archiving hides it from the main list…
        assert_eq!(model.visible(), vec![0, 2]);
        // …but a favourite stays in the Favourites collection…
        model.tag_filter = Some(Tag::Favourite);
        assert_eq!(model.visible(), vec![1]);
        // …and in Archived, and the shelf's Favourites count includes it.
        model.tag_filter = Some(Tag::Archive);
        assert_eq!(model.visible(), vec![1]);
        assert_eq!(model.library_counts().favourites, 1);
    }

    #[test]
    fn search_commits_to_a_full_screen_results_view_and_back_again() {
        let mut model = ReaderModel::new(vault(30));
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        for c in "file 1".chars() {
            update(&mut model, ReaderEvent::Tap { x: 0, y: 0 }, &[(Rect { x: -1, y: -1, w: 2, h: 2 }, ReaderTarget::Key(c))], &fonts(), CANVAS);
        }
        // The keyboard's "Search" commits: keyboard down, query + results kept, rows fill the screen.
        tap(&mut model, |t| *t == ReaderTarget::SubmitSearch);
        assert!(matches!(model.screen, Screen::Search { kb: false, .. }), "results view, keyboard down");
        assert_eq!(model.query, "file 1", "query survives the commit");
        let h = hits(&model);
        assert!(!h.iter().any(|(_, t)| matches!(t, ReaderTarget::Key(_))), "no keyboard in the results view");
        assert!(h.iter().any(|(_, t)| *t == ReaderTarget::EditQuery), "the query bar reopens the keyboard");
        assert!(search_rows_per_page(1696, false, false) > search_rows_per_page(1696, true, false), "results reclaim the keyboard's space");
        assert!(
            search_rows_per_page(1696, false, false) > search_rows_per_page(1696, false, true),
            "the select bar costs a result row"
        );
        // Tapping the query bar brings the keyboard back with the query intact.
        tap(&mut model, |t| *t == ReaderTarget::EditQuery);
        assert!(matches!(model.screen, Screen::Search { kb: true, .. }));
        assert_eq!(model.query, "file 1");
        assert!(hits(&model).iter().any(|(_, t)| matches!(t, ReaderTarget::Key(_))), "keyboard is back");
    }

    #[test]
    fn submitting_an_empty_search_just_closes_it() {
        let mut model = ReaderModel::new(vault(3));
        tap(&mut model, |t| *t == ReaderTarget::OpenSearch);
        tap(&mut model, |t| *t == ReaderTarget::SubmitSearch);
        assert!(matches!(model.screen, Screen::List { .. }), "nothing typed → back to the library");
    }

    #[test]
    fn clearing_in_the_results_view_reopens_the_keyboard() {
        let mut model = ReaderModel::new(vault(3));
        model.query = "file".into();
        model.screen = Screen::Search { scroll: 0, kb: false };
        tap(&mut model, |t| *t == ReaderTarget::ClearQuery);
        assert!(model.query.is_empty(), "query cleared");
        assert!(matches!(model.screen, Screen::Search { kb: true, .. }), "keyboard back for a fresh search");
    }

    #[test]
    fn back_from_a_search_result_returns_to_the_results_view() {
        let mut model = ReaderModel::new(vault(30));
        model.query = "file 1".into();
        model.screen = Screen::Search { scroll: 0, kb: false };
        let target = model.visible()[0];
        model.busy = true;
        let src = "hello world";
        update(&mut model, ReaderEvent::Opened { file_ix: target, source: src.into(), doc: parse(src), pages: layout(&parse(src), &fonts(), LayoutOpts::default()) }, &[], &fonts(), CANVAS);
        tap(&mut model, |t| *t == ReaderTarget::Back);
        assert!(matches!(model.screen, Screen::Search { kb: false, .. }), "back lands on the committed results, not the keyboard");
        assert_eq!(model.query, "file 1", "query still applied");
    }

    #[test]
    fn multi_select_archives_the_ticked_search_results() {
        let mut model = ReaderModel::new(vault(6));
        model.vault[1].tags = vec![Tag::Favourite]; // pre-existing tag must survive the archive
        model.query = "file".into();
        model.screen = Screen::Search { scroll: 0, kb: false };
        // Enter select mode: rows toggle instead of opening.
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        assert_eq!(model.selected.as_deref(), Some(&[][..]));
        let h = hits(&model);
        assert!(!h.iter().any(|(_, t)| matches!(t, ReaderTarget::OpenFile(_))), "rows no longer open");
        assert!(!h.iter().any(|(_, t)| *t == ReaderTarget::ArchiveSelected), "Archive inert with nothing ticked");
        // Tick two rows (toggle one off and on again to prove the toggle).
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(1));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(2));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(2));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(2));
        assert_eq!(model.selected.as_deref(), Some(&[1usize, 2][..]));
        // Archive: one SetTags per ticked file, Archive appended to existing tags, select mode left.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ArchiveSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetTags { file_ix: 1, tags: vec![Tag::Favourite, Tag::Archive] },
                ReaderEffect::SetTags { file_ix: 2, tags: vec![Tag::Archive] },
                ReaderEffect::Redraw(Dirty::Full),
            ]
        );
        assert!(model.selected.is_none(), "select mode exited");
        // The runtime writes + feeds back TagsMarked; the archived rows drop out of the results.
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 1, tags: vec![Tag::Favourite, Tag::Archive] }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 2, tags: vec![Tag::Archive] }, &[], &fonts(), CANVAS);
        assert_eq!(model.visible(), vec![0, 3, 4, 5], "archived results vanished");
        assert_eq!(model.flash.as_ref().map(|f| f.text.as_str()), Some("Archived"));
    }

    #[test]
    fn library_multi_select_archives_and_filter_changes_clear_it() {
        let mut model = ReaderModel::new(vault(5));
        // The library header's Select chip enters select mode; rows toggle instead of opening.
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        assert!(matches!(model.screen, Screen::List { .. }), "still in the library");
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(0));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(3));
        let eff = tap(&mut model, |t| *t == ReaderTarget::ArchiveSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Archive] },
                ReaderEffect::SetTags { file_ix: 3, tags: vec![Tag::Archive] },
                ReaderEffect::Redraw(Dirty::Full),
            ]
        );
        assert!(model.selected.is_none());
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 0, tags: vec![Tag::Archive] }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 3, tags: vec![Tag::Archive] }, &[], &fonts(), CANVAS);
        assert_eq!(model.visible(), vec![1, 2, 4], "archived rows left the library list");
        // A shelf filter change while selecting drops the ticks (rows change under them).
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(1));
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetFilter(StatusFilter::Read));
        assert!(model.selected.is_none(), "filter change cleared the selection");
    }

    #[test]
    fn select_all_ticks_the_page_and_toggles_back_off() {
        let mut model = ReaderModel::new(vault(25));
        let page: Vec<usize> = model.visible().into_iter().take(10).collect();
        assert_eq!(page.len(), list_rows_per_page(CANVAS.height), "one full library page");
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&page[..]), "every row on this page, and no more");
        // With the page full the same chip reads "None" and unticks it.
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&[][..]), "a second tap clears the page");
    }

    #[test]
    fn select_all_accumulates_across_pages() {
        let mut model = ReaderModel::new(vault(25));
        let visible = model.visible();
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        tap(&mut model, |t| *t == ReaderTarget::ListNext);
        assert!(matches!(model.screen, Screen::List { scroll: 10 }), "paged on with the ticks intact");
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&visible[..20]), "page two added to page one");
        // Unticking page two leaves page one's ticks alone.
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&visible[..10]));
    }

    #[test]
    fn bulk_read_marks_the_batch_then_flips_to_unread() {
        let mut model = ReaderModel::new(vault(5));
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        assert!(!hits(&model).iter().any(|(_, t)| *t == ReaderTarget::ReadSelected), "Read inert with nothing ticked");
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(1));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(3));
        let eff = tap(&mut model, |t| *t == ReaderTarget::ReadSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetStatus { file_ix: 1, status: ReadStatus::Read, quiet: false },
                ReaderEffect::SetStatus { file_ix: 3, status: ReadStatus::Read, quiet: false },
                ReaderEffect::Redraw(Dirty::Full),
            ]
        );
        assert_eq!(model.selected.as_deref(), Some(&[1usize, 3][..]), "the batch stays ticked, unlike Archive");
        update(&mut model, ReaderEvent::StatusMarked { file_ix: 1, status: ReadStatus::Read, quiet: false }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::StatusMarked { file_ix: 3, status: ReadStatus::Read, quiet: false }, &[], &fonts(), CANVAS);
        // Now they're all Read the chip undoes itself.
        let eff = tap(&mut model, |t| *t == ReaderTarget::ReadSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetStatus { file_ix: 1, status: ReadStatus::NotRead, quiet: false },
                ReaderEffect::SetStatus { file_ix: 3, status: ReadStatus::NotRead, quiet: false },
                ReaderEffect::Redraw(Dirty::Full),
            ]
        );
    }

    #[test]
    fn bulk_favourite_merges_into_existing_tags_then_flips_to_unfav() {
        let mut model = ReaderModel::new(vault(4));
        model.vault[0].tags = vec![Tag::Work]; // an unrelated tag must survive
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(0));
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(2));
        let eff = tap(&mut model, |t| *t == ReaderTarget::FavSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Work, Tag::Favourite] },
                ReaderEffect::SetTags { file_ix: 2, tags: vec![Tag::Favourite] },
                ReaderEffect::Redraw(Dirty::Full),
            ]
        );
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 0, tags: vec![Tag::Work, Tag::Favourite] }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::TagsMarked { file_ix: 2, tags: vec![Tag::Favourite] }, &[], &fonts(), CANVAS);
        assert_eq!(model.selected.as_deref(), Some(&[0usize, 2][..]), "still ticked");
        let eff = tap(&mut model, |t| *t == ReaderTarget::FavSelected);
        assert_eq!(
            eff,
            vec![
                ReaderEffect::SetTags { file_ix: 0, tags: vec![Tag::Work] },
                ReaderEffect::SetTags { file_ix: 2, tags: vec![] },
                ReaderEffect::Redraw(Dirty::Full),
            ],
            "Unfav strips only the favourite"
        );
    }

    #[test]
    fn select_all_in_search_covers_the_page_the_action_bar_left_room_for() {
        let mut model = ReaderModel::new(vault(25));
        model.query = "file".into();
        model.screen = Screen::Search { scroll: 0, kb: false };
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        let per = search_rows_per_page(CANVAS.height, false, true);
        assert_eq!(per, 9, "the select bar costs the results a row");
        let page: Vec<usize> = model.visible().into_iter().take(per).collect();
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&page[..]), "exactly the shortened page");
    }

    #[test]
    fn bulk_read_in_the_unread_view_drops_the_ticks_that_left_it() {
        let mut model = ReaderModel::new(vault(4));
        model.filter = StatusFilter::NotRead;
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::SelectAll);
        assert_eq!(model.selected.as_deref(), Some(&[0usize, 1, 2, 3][..]));
        tap(&mut model, |t| *t == ReaderTarget::ReadSelected);
        for ix in 0..3 {
            update(&mut model, ReaderEvent::StatusMarked { file_ix: ix, status: ReadStatus::Read, quiet: false }, &[], &fonts(), CANVAS);
        }
        // Those three left the Unread view, so their ticks go with them — you can't untick a row
        // you can't see, and Archive would otherwise still catch them.
        assert_eq!(model.selected.as_deref(), Some(&[3usize][..]));
        assert_eq!(model.visible(), vec![3]);
    }

    #[test]
    fn cancel_and_mode_changes_leave_select_mode() {
        let mut model = ReaderModel::new(vault(4));
        model.query = "file".into();
        model.screen = Screen::Search { scroll: 0, kb: false };
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(1));
        tap(&mut model, |t| *t == ReaderTarget::CancelSelect);
        assert!(model.selected.is_none(), "Cancel drops the ticks without archiving");
        assert_eq!(model.vault[1].tags, Vec::<Tag>::new(), "nothing archived");
        // Reopening the keyboard also clears any selection.
        tap(&mut model, |t| *t == ReaderTarget::SelectMode);
        tap(&mut model, |t| *t == ReaderTarget::ToggleSelect(1));
        tap(&mut model, |t| *t == ReaderTarget::EditQuery);
        assert!(model.selected.is_none(), "mode change clears the selection");
    }

    #[test]
    fn opening_a_collection_clears_a_leftover_status_filter() {
        let mut model = ReaderModel::new(vault(4));
        model.vault[1].tags = vec![Tag::Favourite]; // not-read favourite
        model.vault[2].tags = vec![Tag::Favourite];
        model.vault[2].status = ReadStatus::Read; // read favourite
        model.filter = StatusFilter::Reading; // leftover status filter — would hide BOTH favourites
        // Tap Favourites in the shelf: the collection shows ALL favourites, not Reading ∩ Fav.
        model.shelf_open = true;
        let eff = tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(Some(Tag::Favourite)));
        assert_eq!(eff, vec![ReaderEffect::Redraw(Dirty::Full)]);
        assert_eq!(model.filter, StatusFilter::All, "status filter reset by the collection");
        assert_eq!(model.visible(), vec![1, 2], "every favourite, whatever its status");
        // A content tag stays a narrowing filter: it ANDs with the status filter as before.
        model.filter = StatusFilter::Read;
        model.vault[3].tags = vec![Tag::Ai];
        model.shelf_open = true;
        tap(&mut model, |t| *t == ReaderTarget::SetTagFilter(Some(Tag::Ai)));
        assert_eq!(model.filter, StatusFilter::Read, "content tags don't touch the status filter");
        assert!(model.visible().is_empty(), "AI ∩ Read is empty (file 3 is not-read)");
    }

    #[test]
    fn read_and_archived_articles_stay_in_the_read_view() {
        let mut model = ReaderModel::new(vault(4));
        model.vault[1].status = ReadStatus::Read; // read, live
        model.vault[2].status = ReadStatus::Read;
        model.vault[2].tags = vec![Tag::Archive]; // read + archived (the usual tidy-up)
        model.vault[3].tags = vec![Tag::Archive]; // archived but never read
        // The main list hides both archived clippings…
        assert_eq!(model.visible(), vec![0, 1]);
        // …but Read is the reading history: archiving a finished article doesn't erase it.
        model.filter = StatusFilter::Read;
        assert_eq!(model.visible(), vec![1, 2], "read+archived stays; the unread archived one doesn't");
        // Unread/Reading views still hide archived clippings.
        model.filter = StatusFilter::NotRead;
        assert_eq!(model.visible(), vec![0], "archived unread stays hidden outside Read/Archived");
        // The shelf's Read count matches the Read view.
        assert_eq!(model.library_counts().read, 2);
    }

    #[test]
    fn vault_rescan_clamps_a_now_out_of_range_scroll() {
        let mut model = ReaderModel::new(vault(200));
        tap(&mut model, |t| *t == ReaderTarget::ListNext); // scroll to page 2
        update(&mut model, ReaderEvent::VaultRescanned { files: vault(3) }, &[], &fonts(), CANVAS);
        assert!(matches!(model.screen, Screen::List { scroll: 0 }), "scroll clamped into the shorter list");
        assert_eq!(model.vault.len(), 3);
    }

    // ---- notes on highlights -------------------------------------------------

    // Open a doc that already carries `==quick brown==`, tap the highlighted span to raise the menu,
    // and return the highlight's inner byte range (for anchor assertions).
    fn open_with_highlight(model: &mut ReaderModel) -> (String, std::ops::Range<usize>) {
        let src = "The ==quick brown== fox jumps.".to_string();
        open_sample(model, &src);
        let inner = src.find("quick").unwrap()..src.find("== fox").unwrap();
        long_press(model, |t| matches!(t, ReaderTarget::TapHighlight { .. }));
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "menu opens on a highlight long-press");
        (src, inner)
    }

    #[test]
    fn a_quick_tap_on_a_highlight_does_not_open_the_menu() {
        // The core decoupling: a quick tap navigates (the hit-test sees through the highlight); only a
        // long-press raises the menu. Same point, two gestures, two outcomes.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        let (rect, _) = h.iter().find(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })).expect("highlight present");
        let (cx, cy) = rect.center();
        update(&mut model, ReaderEvent::Tap { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_none()), "a quick tap does NOT open the menu");
        update(&mut model, ReaderEvent::LongPress { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "a long-press at the very same point opens it");
    }

    #[test]
    fn a_long_press_opens_the_menu_in_focus_mode_too() {
        // Regression: the focus view used to expose no TapHighlight hits, so a long-press on a
        // highlight silently no-oped in full-screen mode (and a pen-scrub-opened menu was invisible).
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        tap(&mut model, |t| *t == ReaderTarget::ToggleFocus);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.focus), "in focus");
        long_press(&mut model, |t| matches!(t, ReaderTarget::TapHighlight { .. }));
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "menu opens on a focus long-press");
        // Dismiss works in focus as well (the pill's scrim target exists).
        tap(&mut model, |t| *t == ReaderTarget::HighlightMenuDismiss);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_none()), "menu dismisses in focus");
    }

    #[test]
    fn a_long_press_just_off_a_highlight_still_opens_the_menu() {
        // A finger misses the thin one-line target by a bit; the vertical tolerance still opens the menu.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        let (rect, _) = h.iter().find(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })).expect("highlight present");
        let (cx, _) = rect.center();
        update(&mut model, ReaderEvent::LongPress { x: cx, y: rect.y - 30 }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "a near-miss long-press (~30px above) still opens the menu");
    }

    #[test]
    fn a_long_press_far_from_any_highlight_does_nothing() {
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        // Deep in the body, far below the single first-line highlight.
        let eff = update(&mut model, ReaderEvent::LongPress { x: 477, y: 1400 }, &h, &fonts(), CANVAS);
        assert!(eff.is_empty(), "long-press off any highlight emits nothing, got {eff:?}");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_none()), "no menu opened");
    }

    #[test]
    fn a_pen_scrub_over_a_highlight_opens_its_menu() {
        // The pen's menu path: a drag that stays within an existing highlight opens it (a drag onto new
        // text still highlights; a quick pen tap navigates).
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        let (rect, _) = h.iter().find(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })).expect("highlight present");
        let (cx, cy) = rect.center();
        let ax = rect.x + 4;
        update(&mut model, ReaderEvent::PenDown { x: ax, y: cy }, &h, &fonts(), CANVAS);
        update(&mut model, ReaderEvent::PenMove { x: cx, y: cy }, &h, &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "a pen scrub over a highlight opens its menu");
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::Highlight { .. })), "and never re-highlights, got {eff:?}");
    }

    #[test]
    fn a_stylus_dwell_opens_the_menu_and_clears_the_stroke_so_the_lift_no_ops() {
        // The stylus long-press (backend-timed dwell → LongPress mid-stroke) must open the menu AND
        // clear the in-flight pen stroke, so the trailing PenUp doesn't tap-navigate or dismiss it.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        let (rect, _) = h.iter().find(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })).expect("highlight present");
        let (cx, cy) = rect.center();
        // Pen touches down on the highlight → a stroke is in flight.
        update(&mut model, ReaderEvent::PenDown { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.pen_start.is_some() && r.selection.is_some()), "pen stroke in flight");
        // The dwell fires a LongPress at the same spot → menu opens, stroke cleared.
        update(&mut model, ReaderEvent::LongPress { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some() && r.pen_start.is_none() && r.selection.is_none()), "menu opens and the pen stroke is cleared");
        // The trailing lift now no-ops (doesn't navigate or dismiss); the menu stays open.
        let eff = update(&mut model, ReaderEvent::PenUp { x: cx, y: cy }, &h, &fonts(), CANVAS);
        assert!(eff.is_empty(), "the lift after a dwell no-ops, got {eff:?}");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "menu stays open after the lift");
    }

    #[test]
    fn a_dwell_over_plain_text_leaves_the_stroke_alive_so_the_drag_still_highlights() {
        // The "highlight needs three tries" bug: resting the nib before dragging fired the dwell,
        // which used to clear the in-flight stroke even with no highlight under it — the drag then
        // silently produced nothing. An off-highlight dwell must leave the selection alive.
        let mut model = ReaderModel::new(vault(1));
        let src = "The quick brown fox jumps over the lazy dog and keeps on running for a while.";
        open_sample(&mut model, src);
        let (ax, ay, bx, by) = if let Screen::Reading(r) = &model.screen {
            let page = &r.pages[0];
            let first = page.runs().find(|r| r.text == "quick").unwrap();
            let last = page.runs().find(|r| r.text == "fox").unwrap();
            ((first.x + 1.0) as i32, (first.baseline - 5.0) as i32, (last.x + last.w - 1.0) as i32, (last.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        let h = hits(&model);
        update(&mut model, ReaderEvent::PenDown { x: ax, y: ay }, &h, &fonts(), CANVAS);
        // The nib rests long enough that the backend fires a dwell — over plain text (no highlight).
        let eff = update(&mut model, ReaderEvent::LongPress { x: ax, y: ay }, &h, &fonts(), CANVAS);
        assert!(eff.is_empty(), "an off-highlight dwell emits nothing, got {eff:?}");
        assert!(
            matches!(&model.screen, Screen::Reading(r) if r.pen_start.is_some() && r.selection.is_some() && r.pen_dwelled),
            "the stroke stays alive (and the dwell is remembered)"
        );
        // The drag then completes normally and still highlights.
        update(&mut model, ReaderEvent::PenMove { x: bx, y: by }, &h, &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: bx, y: by }, &h, &fonts(), CANVAS);
        match &eff[..] {
            [ReaderEffect::Highlight { range, add, .. }] => {
                assert!(*add);
                assert_eq!(&src[range.clone()], "quick brown fox");
            }
            other => panic!("expected one add-highlight effect after the dwell, got {other:?}"),
        }
    }

    #[test]
    fn a_dwell_over_plain_text_then_a_still_lift_neither_taps_nor_highlights() {
        // A nib that rested (dwelled) and lifted in place: the user paused, then thought better of
        // it. That must not become a tap (→ focus toggle / page turn) or a stray highlight.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "hello there world of words");
        let (x, y) = if let Screen::Reading(r) = &model.screen {
            let run = r.pages[0].runs().next().unwrap();
            ((run.x + 2.0) as i32, (run.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        let h = hits(&model);
        update(&mut model, ReaderEvent::PenDown { x, y }, &h, &fonts(), CANVAS);
        update(&mut model, ReaderEvent::LongPress { x, y }, &h, &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x, y }, &h, &fonts(), CANVAS);
        assert!(eff.is_empty(), "a dwelled still lift no-ops, got {eff:?}");
        assert!(
            matches!(&model.screen, Screen::Reading(r) if !r.focus && r.page_ix == 0 && !r.pen_dwelled),
            "no navigation happened and the dwell flag is consumed"
        );
        assert!(!model.busy);
    }

    #[test]
    fn a_short_pen_drag_highlights_instead_of_paging() {
        // A deliberate drag across a short word travels well under the old 24px threshold; it must
        // produce a highlight, not be reclassified as a tap (which could turn the page).
        let mut model = ReaderModel::new(vault(1));
        let src = "hello there world of words";
        open_sample(&mut model, src);
        let (x, y) = if let Screen::Reading(r) = &model.screen {
            let run = r.pages[0].runs().next().unwrap();
            ((run.x + 1.0) as i32, (run.baseline - 5.0) as i32)
        } else {
            unreachable!()
        };
        update(&mut model, ReaderEvent::PenDown { x, y }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::PenMove { x: x + 14, y }, &[], &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: x + 14, y }, &[], &fonts(), CANVAS);
        match &eff[..] {
            [ReaderEffect::Highlight { range, add, .. }] => {
                assert!(*add);
                assert_eq!(&src[range.clone()], "hello", "the short word is highlighted");
            }
            other => panic!("expected one add-highlight effect for a 14px drag, got {other:?}"),
        }
        assert!(matches!(&model.screen, Screen::Reading(r) if r.page_ix == 0), "no page turn");
    }

    #[test]
    fn a_long_press_beside_a_highlights_line_start_opens_the_menu() {
        // Pressing the indent just before the highlighted text (or past its end) still counts — the
        // horizontal tolerance is generous now that a quick tap does the navigating.
        let mut model = ReaderModel::new(vault(1));
        open_sample(&mut model, "The ==quick brown== fox jumps.");
        let h = hits(&model);
        let (rect, _) = h.iter().find(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })).expect("highlight present");
        let cy = rect.y + rect.h / 2;
        update(&mut model, ReaderEvent::LongPress { x: rect.x - 40, y: cy }, &h, &fonts(), CANVAS);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_some()), "a press 40px left of the span still opens the menu");
    }

    #[test]
    fn tapping_a_highlight_opens_the_menu_then_note_editor_saves() {
        let mut model = ReaderModel::new(vault(1));
        let (_src, inner) = open_with_highlight(&mut model);
        // Tap "Note" → the editor opens empty (this highlight has no note yet); the menu closes.
        tap(&mut model, |t| *t == ReaderTarget::HighlightAddNote);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.as_ref().is_some_and(|n| n.buf.is_empty() && !n.had_note) && r.highlight_menu.is_none()), "note editor opens empty");
        // Type "hi", Save → AddNote anchored inside the highlight with the typed text.
        tap(&mut model, |t| *t == ReaderTarget::NoteKey('h'));
        tap(&mut model, |t| *t == ReaderTarget::NoteKey('i'));
        let eff = tap(&mut model, |t| *t == ReaderTarget::NoteSave);
        assert!(
            eff.iter().any(|e| matches!(e, ReaderEffect::AddNote { anchor: a, text, .. } if inner.contains(a) && text == "hi")),
            "Save emits AddNote inside the highlight + typed text, got {eff:?}"
        );
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.is_none()), "sheet closes on Save");
    }

    #[test]
    fn a_fresh_empty_note_just_closes_without_writing() {
        let mut model = ReaderModel::new(vault(1));
        open_with_highlight(&mut model);
        tap(&mut model, |t| *t == ReaderTarget::HighlightAddNote);
        let eff = tap(&mut model, |t| *t == ReaderTarget::NoteSave);
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::AddNote { .. })), "no write for a blank new note");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.is_none()), "sheet still closes");
    }

    #[test]
    fn re_tapping_a_noted_highlight_prefills_and_clearing_writes_an_empty_note() {
        // A highlight that already carries a note: the menu labels it as noted, the editor prefills the
        // existing text, and clearing it + saving writes an (empty) AddNote so the runtime drops the note.
        let mut model = ReaderModel::new(vault(1));
        let src = "The ==quick brown==[^1] fox jumps.\n\n[^1]: my note\n";
        open_sample(&mut model, src);
        long_press(&mut model, |t| matches!(t, ReaderTarget::TapHighlight { .. }));
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.as_ref().is_some_and(|m| m.has_note)), "menu knows the highlight has a note");
        tap(&mut model, |t| *t == ReaderTarget::HighlightAddNote);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.as_ref().is_some_and(|n| n.buf == "my note" && n.had_note)), "editor prefilled with the existing note");
        // Clear it, Save → an empty-text AddNote (the runtime's upsert drops the footnote).
        tap(&mut model, |t| *t == ReaderTarget::NoteBackspace); // remove one char to prove edits stick…
        if let Screen::Reading(r) = &mut model.screen {
            if let Some(n) = &mut r.note { n.buf.clear(); }
        }
        let eff = tap(&mut model, |t| *t == ReaderTarget::NoteSave);
        assert!(eff.iter().any(|e| matches!(e, ReaderEffect::AddNote { text, .. } if text.is_empty())), "clearing an existing note writes an empty note, got {eff:?}");
    }

    #[test]
    fn cancelling_the_note_leaves_no_write() {
        let mut model = ReaderModel::new(vault(1));
        open_with_highlight(&mut model);
        tap(&mut model, |t| *t == ReaderTarget::HighlightAddNote);
        tap(&mut model, |t| *t == ReaderTarget::NoteKey('x'));
        let eff = tap(&mut model, |t| *t == ReaderTarget::NoteCancel);
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::AddNote { .. })), "cancel never writes");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.is_none()), "sheet closed");
    }

    #[test]
    fn a_pen_drag_while_the_note_sheet_is_open_makes_no_highlight() {
        let mut model = ReaderModel::new(vault(1));
        open_with_highlight(&mut model);
        tap(&mut model, |t| *t == ReaderTarget::HighlightAddNote);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.is_some()));
        // A stray drag on the article text above the sheet must not start a new highlight.
        update(&mut model, ReaderEvent::PenDown { x: 120, y: 420 }, &[], &fonts(), CANVAS);
        update(&mut model, ReaderEvent::PenMove { x: 520, y: 420 }, &[], &fonts(), CANVAS);
        let eff = update(&mut model, ReaderEvent::PenUp { x: 520, y: 420 }, &[], &fonts(), CANVAS);
        assert!(!eff.iter().any(|e| matches!(e, ReaderEffect::Highlight { .. })), "no highlight while a note is open");
        assert!(matches!(&model.screen, Screen::Reading(r) if r.note.is_some()), "note sheet stays open");
    }

    #[test]
    fn deleting_a_highlight_from_the_menu_requests_removal() {
        let mut model = ReaderModel::new(vault(1));
        let (_src, inner) = open_with_highlight(&mut model);
        let eff = tap(&mut model, |t| *t == ReaderTarget::HighlightDelete);
        assert!(
            eff.iter().any(|e| matches!(e, ReaderEffect::Highlight { range, add, .. } if !*add && *range == inner)),
            "Delete emits a remove-highlight for the tapped span, got {eff:?}"
        );
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_none()), "menu closes on Delete");
        assert!(model.busy);
    }

    #[test]
    fn tapping_outside_the_menu_dismisses_it() {
        let mut model = ReaderModel::new(vault(1));
        open_with_highlight(&mut model);
        let eff = tap(&mut model, |t| *t == ReaderTarget::HighlightMenuDismiss);
        assert!(matches!(&model.screen, Screen::Reading(r) if r.highlight_menu.is_none()), "menu dismissed");
        assert!(!eff.is_empty());
    }

    #[test]
    fn note_added_swaps_the_source() {
        let mut model = ReaderModel::new(vault(1));
        let src = "==hi== there";
        open_sample(&mut model, src);
        let noted = "==hi==[^1] there\n\n[^1]: a note\n";
        let doc = parse(noted);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        update(&mut model, ReaderEvent::NoteAdded { source: noted.into(), doc, pages }, &[], &fonts(), CANVAS);
        match &model.screen {
            Screen::Reading(r) => {
                assert_eq!(r.source, noted, "source swapped to the noted version");
                assert!(r.note.is_none());
            }
            _ => panic!("still reading"),
        }
    }
}
