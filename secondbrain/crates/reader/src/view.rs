//! Draw the reader's two screens to a frame + a hit map (what you tap is what you see).
//!
//! Chrome (header/footer) is painted in the margins the layout engine already reserves
//! (`content_top`/`content_bottom`), so the reading body from [`crate::draw_page`] slots between
//! a title/page header and a prev/next footer without overlap.

use crate::model::{DocKind, NoteEntry, ReadStatus, Reading, ReaderModel, Screen, SortKey, StatusFilter, Tag};
use crate::paint::draw_page_with_images;
use image::{Rgb, RgbImage};
use std::collections::HashMap;
use std::ops::Range;
use render::draw::{checkbox, checkbox_checked, disc, draw_text, draw_text_tracked, fill_rect, hline, icon, measure, rect_outline, truncate};
use render::ui::{draw_bottom_sheet, draw_keyboard, even_slots, icon_btn, wrap, Icon, KeyCap, CHIP, GOLD, KB_H, SHEET_HEADER_H, TINT};
use render::{Fonts, FAINT, INK, MUTED, PAPER};

/// The shared tap-target [`Rect`], icon set (`Icon`/`icon_btn`), and accent colours
/// (`GOLD`/`CHIP`/`SCRIM`/`TINT`) now live in [`render::ui`] — one copy across the Reader + Agenda.
/// The Reader maps the rects these helpers return to its own [`ReaderTarget`]; only Reader-specific
/// glue (panels, editorial rows, search, masthead) stays in this file.
pub use render::ui::Rect;

/// What a tap maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderTarget {
    OpenFile(usize),
    ListPrev,
    ListNext,
    PrevPage,
    NextPage,
    Back,
    Exit,
    /// Reading view: step the text size down / up.
    TextSmaller,
    TextLarger,
    /// Set `vault[file_ix]`'s status directly. Emitted by the reading-view picker (each segment
    /// carries its own state) and by a list row's status dot (which supplies the *next* state).
    SetStatusFor(usize, ReadStatus),
    /// List view: apply a status filter / set the sort order.
    SetFilter(StatusFilter),
    SetSort(SortKey),
    /// List view: narrow to a tag (or `None` = all tags). ANDs with the status filter.
    SetTagFilter(Option<Tag>),
    /// List view: narrow to an author byline (or `None` = all authors). ANDs with status/tags.
    SetAuthorFilter(Option<String>),
    /// Shelf "All authors…" → the full-screen author picker; its ‹ › page it and Close returns.
    OpenAuthors,
    CloseAuthors,
    AuthorsPrev,
    AuthorsNext,
    /// List view: enter/leave the Books collection (the read-only Kindle-highlight library).
    SetBooks(bool),
    /// List view: open / close the library shelf (the slide-in filter panel).
    OpenShelf,
    CloseShelf,
    /// Reading view: open / close the "More" sheet (status + tags).
    OpenMore,
    CloseMore,
    /// Reading view: open / close the Contents (table-of-contents) panel.
    OpenContents,
    CloseContents,
    /// Reading view (Contents): jump to the page holding a heading.
    JumpToHeading(usize),
    /// Reading view (More sheet): toggle a managed tag on the open clipping.
    ToggleTag(Tag),
    /// Reading view: toggle focus (full-screen) mode — a centre tap hides/shows all chrome.
    ToggleFocus,
    /// Library ↺: pull new clippings (vault sync + rescan).
    SyncVault,
    /// List → open the search screen; Search → close it (clearing the query).
    OpenSearch,
    CloseSearch,
    /// Keyboard "Search": commit the query — hide the keyboard so the results fill the screen
    /// (an empty query just closes search instead).
    SubmitSearch,
    /// Results view: tap the query bar to bring the keyboard back and refine the query.
    EditQuery,
    /// Results view: enter multi-select (row taps then tick clippings instead of opening them).
    SelectMode,
    /// Select mode: toggle a clipping's tick.
    ToggleSelect(usize),
    /// Select mode: tick every row on the page — or untick them all if they're already ticked (the
    /// chip reads "None" then). Ticks made on other pages survive either way.
    SelectAll,
    /// Select mode: archive every ticked clipping, then leave select mode.
    ArchiveSelected,
    /// Select mode: mark every ticked clipping Read — or NotRead when they all already are.
    /// Keeps the selection (the rows stay put), unlike archiving.
    ReadSelected,
    /// Select mode: favourite every ticked clipping — or unfavourite when they all already are.
    /// Keeps the selection.
    FavSelected,
    /// Select mode: leave without archiving.
    CancelSelect,
    /// Empty library list: drop every filter at once (status, tag/collection, author, search).
    ClearFilters,
    /// Search: append a character to the query / delete the last char / clear it entirely.
    Key(char),
    Backspace,
    ClearQuery,
    /// Search keyboard: toggle one-shot Shift / switch the letter↔number-symbol layer (shared
    /// [`render::ui::draw_keyboard`], same as the Agenda quick-add).
    KbShift,
    KbLayer,
    /// Note sheet (opened from the tap-a-highlight menu): the keyboard, mirroring the search keyboard,
    /// but editing the note buffer. `NoteSave` writes `==…==[^n]` + `[^n]: note`; `NoteCancel` discards.
    NoteKey(char),
    NoteBackspace,
    NoteShift,
    NoteLayer,
    NoteSave,
    NoteCancel,
    /// Reading view: a tap landed on the highlighted span with this inner byte range → open the
    /// floating action menu (Note / Delete) over it.
    TapHighlight { range: Range<usize> },
    /// Highlight menu: open the note editor for the tapped highlight (prefilled if it has a note).
    HighlightAddNote,
    /// Highlight menu: delete the tapped highlight (and any note it carries).
    HighlightDelete,
    /// Highlight menu: a tap outside the pill dismisses it.
    HighlightMenuDismiss,
}

pub type HitMap = Vec<(Rect, ReaderTarget)>;

pub struct ViewOutput {
    pub frame: RgbImage,
    pub hits: HitMap,
}

/// Last-drawn target wins (top layer), matching draw order.
pub fn hit_test(hits: &[(Rect, ReaderTarget)], x: i32, y: i32) -> Option<&(Rect, ReaderTarget)> {
    hits.iter().rev().find(|(r, _)| r.contains(x, y))
}

pub const HEADER_H: i32 = 112;
pub const FOOTER_H: i32 = 96;
/// The reading view's slim top chrome: a thin progress bar + "X min left" (~80px). Hand-matched to
/// layout `content_top` (88) so body text never runs under it. The article title/byline/date live in
/// the page-1 body masthead (reserved via `LayoutOpts.first_page_extra`), not here — see `view_reading`.
pub const READING_HEADER_H: i32 = 80;
/// The library list draws ~1.35× larger than the reading chrome, so clipping titles are easy to
/// browse at arm's length. List-only knob — the reading view is unaffected.
const LIST_TS: f32 = 1.35;
const LIST_ROW_H: i32 = (110.0 * LIST_TS) as i32; // ~148 — three lines: source eyebrow, serif title, reading-time · date
const MARGIN: i32 = 44;
/// Gap between neighbouring chips in a chip row (the select bar).
const CHIP_GAP: i32 = 12;
pub const MIN_TAP_PX: i32 = 60;
/// Search screen: the query bar height, and the docked on-screen keyboard's total height.
const SEARCH_QBAR_H: i32 = 84;
/// The docked search keyboard height — the shared [`render::ui::KB_H`]. `pub` so the runtime/update
/// can compute the keep-static region (repaint above the keyboard on each keystroke).
pub const SEARCH_KB_H: i32 = KB_H;
/// The note-entry sheet: a text field between the sheet header and the docked keyboard (which reuses
/// the search keyboard's height, so the note keyboard sits exactly where the search one does).
const NOTE_FIELD_H: i32 = 132;
const NOTE_SHEET_H: i32 = SHEET_HEADER_H + NOTE_FIELD_H + KB_H;

/// Render whichever screen the model is showing.
pub fn view(model: &ReaderModel, fonts: &Fonts, w: u32, h: u32) -> ViewOutput {
    view_with_images(model, fonts, w, h, &HashMap::new())
}

/// As [`view`], but blits the open clipping's prepared images (keyed by URL, from [`crate::imaging`])
/// into the reading page in place of the placeholder boxes.
pub fn view_with_images(
    model: &ReaderModel,
    fonts: &Fonts,
    w: u32,
    h: u32,
    images: &HashMap<String, RgbImage>,
) -> ViewOutput {
    match &model.screen {
        Screen::List { scroll } => view_list(model, *scroll, fonts, w, h),
        Screen::Reading(r) => view_reading(model, r, fonts, w, h, images),
        Screen::Search { scroll, kb } => view_search(model, *scroll, *kb, fonts, w, h),
        Screen::Authors { scroll } => view_authors(model, *scroll, fonts, w, h),
    }
}

/// How many author rows fit on the picker screen between its header and footer.
pub fn authors_rows_per_page(h: u32) -> usize {
    ((h as i32 - HEADER_H - 8 - FOOTER_H) / SHELF_ROW_H).max(1) as usize
}

/// How many list rows fit between the header (+ filter bar) and footer.
pub fn list_rows_per_page(h: u32) -> usize {
    ((h as i32 - HEADER_H - FOOTER_H) / LIST_ROW_H).max(1) as usize
}

/// How many result rows fit on the search screen (header + query bar above; the keyboard docked
/// below while it's up — committed results reclaim its space). `select` is whether the multi-select
/// action bar is showing: the search screen has no footer otherwise, so the bar costs a row.
pub fn search_rows_per_page(h: u32, kb: bool, select: bool) -> usize {
    let kb_h = if kb { SEARCH_KB_H } else { 0 };
    let bar_h = if select { FOOTER_H } else { 0 };
    ((h as i32 - HEADER_H - 8 - SEARCH_QBAR_H - 8 - kb_h - bar_h) / LIST_ROW_H).max(1) as usize
}

fn view_list(model: &ReaderModel, scroll: usize, fonts: &Fonts, w: u32, h: u32) -> ViewOutput {
    let mut img = RgbImage::from_pixel(w, h, PAPER);
    let mut hits: HitMap = Vec::new();
    let iw = w as i32;
    let visible = model.visible();

    // TOP (static): the serif "Library" masthead, an active-filter subtitle, and the "1–10 of 316"
    // page range right-aligned. Every clickable control (shelf, search, exit, paging) lives in the
    // bottom bar — the top's only controls are the multi-select chips (Select, or Cancel while
    // selecting), shared with the search results view. The bulk actions themselves live in the
    // bottom select bar, where five chips fit and the header's three wouldn't.
    let selection = model.selected.as_deref();
    let per = list_rows_per_page(h);
    let shown_end = (scroll + per).min(visible.len());
    draw_text(&mut img, &fonts.serif_bold, 46.0, MARGIN as f32, 72.0, "Library", INK);
    let sub = if let Some(sel) = selection {
        match sel.len() {
            0 => "Tap clippings to select".to_string(),
            1 => "1 selected".to_string(),
            n => format!("{n} selected"),
        }
    } else {
        let mut active: Vec<String> = Vec::new();
        if model.books {
            // The Books collection ignores the status/tag filters, so it's the only label.
            active.push("Books".to_string());
        } else {
            if model.filter != StatusFilter::All {
                active.push(model.filter.label().to_string());
            }
            match model.tag_filter {
                Some(Tag::Favourite) => active.push("Favourites".to_string()),
                Some(Tag::Archive) => active.push("Archived".to_string()),
                Some(t) => active.push(t.label().to_string()),
                None => {}
            }
            if let Some(a) = model.author_filter.as_deref() {
                active.push(a.to_string());
            }
        }
        if active.is_empty() {
            format!("{} clippings", visible.len())
        } else {
            format!("{} · {}", active.join(" · "), visible.len())
        }
    };
    draw_text(&mut img, &fonts.sans, LIST_TS * 22.0, MARGIN as f32, 100.0, &sub, MUTED);
    let mut chips_left = iw - MARGIN; // the select chips stack right-to-left from here
    if selection.is_some() {
        let cancel = chip(&mut img, fonts, "Cancel", chips_left - 148, 34, 148, 60);
        hits.push((cancel, ReaderTarget::CancelSelect));
        chips_left -= 148 + 12;
    } else if !visible.is_empty() && !model.books {
        // No Select chip in Books: multi-select exists to bulk-archive, and books are read-only.
        let select = chip(&mut img, fonts, "Select", chips_left - 148, 34, 148, 60);
        hits.push((select, ReaderTarget::SelectMode));
        chips_left -= 148 + 12;
    }
    if !visible.is_empty() {
        let range = format!("{}–{} of {}", scroll + 1, shown_end, visible.len());
        let rw = measure(&fonts.mono, 28.0, &range);
        draw_text(&mut img, &fonts.mono, 28.0, (chips_left as f32 - 16.0) - rw, 72.0, &range, MUTED);
    }
    hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, (HEADER_H - 2) as f32, FAINT, 2);

    // Rows over the filtered + sorted vault indices (filters live in the shelf).
    let rows_top = HEADER_H + 8;
    if visible.is_empty() {
        let msg = if model.vault.is_empty() {
            "No clippings found — check the vault path."
        } else {
            "Nothing matches this filter."
        };
        draw_text(&mut img, &fonts.sans, LIST_TS * 26.0, MARGIN as f32, (rows_top + 80) as f32, msg, MUTED);
        // A filtered-to-nothing list must offer its own way out: the shelf control that set the
        // filter can be gone (an author whose clippings were all archived), and hunting for the one
        // that isn't is no way to spend a tap on e-ink.
        if model.any_filter_active() {
            let r = chip(&mut img, fonts, "Clear filters", MARGIN, rows_top + 120, 264, 72);
            hits.push((r, ReaderTarget::ClearFilters));
        }
    }
    draw_rows(&mut img, fonts, model, &visible, scroll, per, rows_top, iw, selection, &mut hits);

    // BOTTOM bar. While selecting it's the bulk-action bar (the nav icons would be dead ends
    // anyway — you can't open, search or exit mid-selection), keeping prev/next so a selection can
    // be built across pages.
    if selection.is_some() {
        let page = &visible[scroll.min(visible.len())..shown_end];
        draw_select_bar(&mut img, fonts, model, page, iw, h as i32, Some((scroll > 0, shown_end < visible.len())), &mut hits);
    } else {
        // Otherwise: ‹ prev · ≡ shelf · ⌕ search · ⭲ exit · next › — five icons evenly
        // spread across the width, so prev/next land on the corners (faint + inert at an edge) while the
        // actions space out between. The list has no swipe paging, so it keeps visible chevrons.
        let icell = MIN_TAP_PX;
        let fy = h as i32 - FOOTER_H;
        let icon_y = fy + 18;
        hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, fy as f32, FAINT, 1);
        // Evenly distribute N=6 cells; slots 0 and 5 fall on the corners (shared with the Agenda bar).
        let slots = even_slots(iw, MARGIN, icell, 6);
        let bx = |k: i32| slots[k as usize];
        if scroll > 0 {
            hits.push((icon_btn(&mut img, fonts, Icon::ChevronLeft, bx(0), icon_y, icell, 60, INK), ReaderTarget::ListPrev));
        } else {
            icon_btn(&mut img, fonts, Icon::ChevronLeft, bx(0), icon_y, icell, 60, FAINT);
        }
        hits.push((icon_btn(&mut img, fonts, Icon::Menu, bx(1), icon_y, icell, 60, INK), ReaderTarget::OpenShelf));
        hits.push((icon_btn(&mut img, fonts, Icon::Search, bx(2), icon_y, icell, 60, INK), ReaderTarget::OpenSearch));
        // ↺ doubles as "refresh" (same glyph the Agenda's refresh uses): pull new clippings via a sync.
        hits.push((icon_btn(&mut img, fonts, Icon::Undo, bx(3), icon_y, icell, 60, INK), ReaderTarget::SyncVault));
        hits.push((icon_btn(&mut img, fonts, Icon::Exit, bx(4), icon_y, icell, 60, INK), ReaderTarget::Exit));
        if shown_end < visible.len() {
            hits.push((icon_btn(&mut img, fonts, Icon::ChevronRight, bx(5), icon_y, icell, 60, INK), ReaderTarget::ListNext));
        } else {
            icon_btn(&mut img, fonts, Icon::ChevronRight, bx(5), icon_y, icell, 60, FAINT);
        }
    }

    // The library shelf overlays the list (drawn + hit-tested last) when open.
    if model.shelf_open {
        draw_shelf(&mut img, fonts, model, iw, h as i32, &mut hits);
    }
    flash(&mut img, model, fonts, w, h);
    ViewOutput { frame: img, hits }
}

/// Draw `per` two-line list rows (title, then byline · date) starting at `rows_top`, pushing each
/// row's OpenFile hit + status-gutter hit. Shared by the library list and the search results.
fn draw_rows(img: &mut RgbImage, fonts: &Fonts, model: &ReaderModel, visible: &[usize], scroll: usize, per: usize, rows_top: i32, iw: i32, selected: Option<&[usize]>, hits: &mut HitMap) {
    let eyebrow_fs = LIST_TS * 17.0; // the SOURCE caps eyebrow above the title
    let title_fs = LIST_TS * 30.0;
    let meta_fs = LIST_TS * 21.0; // the "reading-time · date" data line below the title
    for (row, &i) in visible.iter().enumerate().skip(scroll).take(per) {
        let file = &model.vault[i];
        let y = rows_top + (row - scroll) as i32 * LIST_ROW_H;
        let rect = Rect { x: MARGIN, y, w: iw - 2 * MARGIN, h: LIST_ROW_H };
        let cy = y + LIST_ROW_H / 2;
        let (eyebrow_base, title_base, meta_base) = (y as f32 + 36.0, y as f32 + 86.0, y as f32 + 124.0);
        // Select mode swaps the status dot for a checkbox and the whole row toggles the tick.
        // Books carry no reading status (read-only), so their rows lead with a book glyph instead.
        if let Some(sel) = selected {
            let (bx, by, bs) = (MARGIN as f32, (cy - 18) as f32, 36.0);
            if sel.contains(&i) {
                checkbox_checked(img, bx, by, bs, INK);
            } else {
                checkbox(img, bx, by, bs, MUTED);
            }
        } else if file.kind == DocKind::Book {
            icon(img, &fonts.icons, Icon::Book.ch(), MARGIN as f32 + 12.0, cy as f32, 30.0, MUTED);
        } else {
            draw_status_marker(img, file.status, MARGIN, cy);
        }
        let text_x = MARGIN + 64; // clear of the status tap gutter
        let right = (rect.x + rect.w) as f32;
        // A favourited clipping gets a gold star at the row's right edge; reserve its width so the
        // title/eyebrow never run under it.
        let fav = file.tags.contains(&Tag::Favourite);
        let content_right = if fav {
            icon(img, &fonts.icons, Icon::StarFill.ch(), right - 15.0, (y + 62) as f32, 30.0, GOLD);
            right - 52.0
        } else {
            right
        };
        // Eyebrow — the source (publisher/author) in letter-spaced caps, the editorial signal.
        if let Some(b) = file.byline.as_deref().filter(|b| !b.is_empty()) {
            let caps = b.to_uppercase();
            let e = truncate(&fonts.sans_semibold, eyebrow_fs, &caps, content_right - text_x as f32);
            draw_text_tracked(img, &fonts.sans_semibold, eyebrow_fs, text_x as f32, eyebrow_base, &e, MUTED, 1.5);
        }
        // Title — serif, for a book-like reading list.
        let title = truncate(&fonts.serif_bold, title_fs, &file.title, (content_right - text_x as f32).max(80.0));
        draw_text(img, &fonts.serif_bold, title_fs, text_x as f32, title_base, &title, INK);
        // Meta — "{n} min · {date}" in mono; a gold square leads the line when the clipping holds
        // highlights. Reading-time is a size-derived estimate (VaultFile.minutes).
        let mut mx = text_x as f32;
        if file.has_highlights {
            let d = LIST_TS * 11.0;
            fill_rect(img, mx, meta_base - d - 1.0, d, d, GOLD);
            mx += d + 12.0;
        }
        let date = file.published.as_deref().unwrap_or("");
        let mins = if file.minutes > 0 { format!("{} min", file.minutes) } else { String::new() };
        let meta = match (mins.is_empty(), date.is_empty()) {
            (false, false) => format!("{mins} · {date}"),
            (false, true) => mins,
            (true, false) => date.to_string(),
            (true, true) => String::new(),
        };
        if !meta.is_empty() {
            let m = truncate(&fonts.mono, meta_fs, &meta, right - mx);
            draw_text(img, &fonts.mono, meta_fs, mx, meta_base, &m, MUTED);
        }
        hline(img, MARGIN as f32, (iw - MARGIN) as f32, (y + LIST_ROW_H) as f32, FAINT, 1);
        if selected.is_some() {
            hits.push((rect, ReaderTarget::ToggleSelect(i)));
        } else {
            hits.push((rect, ReaderTarget::OpenFile(i)));
            // The status-cycling gutter only exists for articles — a book has no status to cycle.
            if file.kind != DocKind::Book {
                let gutter = Rect { x: MARGIN - 10, y, w: 72, h: LIST_ROW_H };
                hits.push((gutter, ReaderTarget::SetStatusFor(i, file.status.next())));
            }
        }
    }
}

/// A marker at the left of a shelf row: an icon glyph, a reading-status dot, or nothing (for "All").
enum ShelfMarker {
    Icon(Icon),
    Status(ReadStatus),
    None,
}

const SHELF_ROW_H: i32 = 78;

/// Width of every slide-in panel (library shelf, Contents, More) — a left-docked column over a scrim.
const PANEL_W: i32 = 624;

/// Draw the shared chrome for a left-docked slide-in panel: a grey scrim (tap-outside → `close`), a
/// full-height white column with a hairline right edge, a serif masthead, and a Close (X) button.
/// Returns the panel width; callers lay rows with [`shelf_row`] and section headers with
/// [`panel_eyebrow`]. Reused by the library shelf, the reading Contents panel, and the More sheet — so
/// every panel in the app reveals the same way (instantly; e-ink can't animate a slide).
fn draw_panel_chrome(img: &mut RgbImage, fonts: &Fonts, title: &str, close: ReaderTarget, iw: i32, ih: i32, hits: &mut HitMap) -> i32 {
    // Chrome (scrim + white column + serif masthead + Close X) is the shared [`render::ui::draw_panel`];
    // the scrim is the tap-outside-to-close (pushed first so the panel's own rows, pushed after, win
    // inside the column).
    let (scrim, close_btn) = render::ui::draw_panel(img, fonts, title, iw, ih);
    hits.push((scrim, close.clone()));
    hits.push((close_btn, close));
    PANEL_W
}

/// A letter-spaced caps section header inside a panel (e.g. "READING STATUS", "TAGS") — the shared
/// [`render::ui::panel_eyebrow`] voice, at the panel's left margin with its baseline at `y + 34`.
fn panel_eyebrow(img: &mut RgbImage, fonts: &Fonts, y: i32, label: &str) {
    render::ui::panel_eyebrow(img, fonts, label, MARGIN, y + 34);
}

/// The library shelf — a full-height panel down the left over a scrim, holding every filter that used
/// to crowd the list header: reading status, favourites/archived collections, topic tags, and sort.
/// An instant reveal; tapping a row applies its filter (and the runtime closes the shelf), tapping
/// outside the panel closes it. Drawn + hit-tested last so it shadows the list beneath.
fn draw_shelf(img: &mut RgbImage, fonts: &Fonts, model: &ReaderModel, iw: i32, ih: i32, hits: &mut HitMap) {
    let pw = draw_panel_chrome(img, fonts, "Library", ReaderTarget::CloseShelf, iw, ih, hits);
    let c = model.library_counts();
    let mut y = 124;
    panel_eyebrow(img, fonts, y, "READING STATUS");
    y += 52;
    for (m, label, count, filt) in [
        (ShelfMarker::None, "All", c.all, StatusFilter::All),
        (ShelfMarker::Status(ReadStatus::NotRead), "Unread", c.unread, StatusFilter::NotRead),
        (ShelfMarker::Status(ReadStatus::Reading), "Reading", c.reading, StatusFilter::Reading),
        (ShelfMarker::Status(ReadStatus::Read), "Read", c.read, StatusFilter::Read),
    ] {
        shelf_row(img, fonts, pw, y, m, label, Some(count), model.filter == filt, ReaderTarget::SetFilter(filt), hits);
        y += SHELF_ROW_H;
    }
    y = shelf_divider(img, pw, y);

    panel_eyebrow(img, fonts, y, "COLLECTIONS");
    y += 52;
    let fav_on = model.tag_filter == Some(Tag::Favourite);
    let fav_ic = if fav_on { Icon::StarFill } else { Icon::Star };
    let fav_next = if fav_on { None } else { Some(Tag::Favourite) };
    shelf_row(img, fonts, pw, y, ShelfMarker::Icon(fav_ic), "Favourites", Some(c.favourites), fav_on, ReaderTarget::SetTagFilter(fav_next), hits);
    y += SHELF_ROW_H;
    let arch_on = model.tag_filter == Some(Tag::Archive);
    let arch_ic = if arch_on { Icon::ArchiveFill } else { Icon::Archive };
    let arch_next = if arch_on { None } else { Some(Tag::Archive) };
    shelf_row(img, fonts, pw, y, ShelfMarker::Icon(arch_ic), "Archived", Some(c.archived), arch_on, ReaderTarget::SetTagFilter(arch_next), hits);
    y += SHELF_ROW_H;
    // Books — the read-only Kindle-highlight library (Readwise exports synced from Obsidian).
    let book_ic = if model.books { Icon::BookFill } else { Icon::Book };
    shelf_row(img, fonts, pw, y, ShelfMarker::Icon(book_ic), "Books", Some(c.books), model.books, ReaderTarget::SetBooks(!model.books), hits);
    y += SHELF_ROW_H;
    y = shelf_divider(img, pw, y);

    panel_eyebrow(img, fonts, y, "TOPICS");
    y += 52;
    for (t, n) in &c.tags {
        let on = model.tag_filter == Some(*t);
        let next = if on { None } else { Some(*t) };
        shelf_row(img, fonts, pw, y, ShelfMarker::Icon(Icon::Tag), t.label(), Some(*n), on, ReaderTarget::SetTagFilter(next), hits);
        y += SHELF_ROW_H;
    }
    y = shelf_divider(img, pw, y);

    // AUTHORS — the most-clipped bylines inline (the active author always swapped in so it can be
    // toggled off), with the full A–Z picker one tap away. Two inline rows keep the whole shelf
    // inside the 1696px canvas alongside the sort row below.
    let authors = model.authors();
    // Drawn whenever an author filter is live, even with no authors left to list: this section owns
    // the only two controls that can clear that filter (an active author's row, and the "All
    // authors…" door to the picker). Gating it on `authors` alone once let a filter outlive both —
    // archive every article by one author and they leave `authors()`, stranding the library empty.
    if !authors.is_empty() || model.author_filter.is_some() {
        panel_eyebrow(img, fonts, y, "AUTHORS");
        y += 52;
        let mut inline: Vec<(String, usize)> = authors.iter().take(SHELF_AUTHORS).cloned().collect();
        if let Some(a) = model.author_filter.as_deref() {
            if !inline.iter().any(|(n, _)| n == a) {
                // The active author, swapped in so it can be toggled off — synthesised at zero when
                // `authors()` no longer knows them (every clipping of theirs archived).
                let act = authors.iter().find(|(n, _)| n == a).cloned().unwrap_or_else(|| (a.to_string(), 0));
                inline.pop();
                inline.push(act);
            }
        }
        for (name, n) in inline {
            let on = model.author_filter.as_deref() == Some(name.as_str());
            let next = if on { None } else { Some(name.clone()) };
            shelf_row(img, fonts, pw, y, ShelfMarker::None, &name, Some(n), on, ReaderTarget::SetAuthorFilter(next), hits);
            y += SHELF_ROW_H;
        }
        shelf_row(img, fonts, pw, y, ShelfMarker::Icon(Icon::More), "All authors…", None, false, ReaderTarget::OpenAuthors, hits);
        y += SHELF_ROW_H;
        y = shelf_divider(img, pw, y);
    }

    // SORT — one row: the sort glyph, then a chip per order (the active one ink-filled). Chips keep
    // every order one explicit tap away without spending three full rows of a full shelf.
    let cy = y + SHELF_ROW_H / 2;
    icon(img, &fonts.icons, Icon::Sort.ch(), (MARGIN + 18) as f32, cy as f32, 34.0, INK);
    let mut cx = MARGIN + 56;
    for k in SortKey::ALL {
        let label = k.label();
        let cw = measure(&fonts.sans, 30.0, label) as i32 + 40;
        let r = Rect { x: cx, y: y + (SHELF_ROW_H - 60) / 2, w: cw, h: 60 };
        let on = model.sort == k;
        let (bg, fg) = if on { (INK, PAPER) } else { (CHIP, INK) };
        fill_rect(img, r.x as f32, r.y as f32, r.w as f32, r.h as f32, bg);
        let tw = measure(&fonts.sans, 30.0, label);
        draw_text(img, &fonts.sans, 30.0, r.x as f32 + (r.w as f32 - tw) / 2.0, r.y as f32 + 42.0, label, fg);
        hits.push((r, ReaderTarget::SetSort(k)));
        cx += cw + 16;
    }
}

/// How many authors the shelf shows inline before "All authors…" hands off to the picker.
const SHELF_AUTHORS: usize = 2;

/// A hairline between shelf groups; returns the next content `y`.
fn shelf_divider(img: &mut RgbImage, pw: i32, y: i32) -> i32 {
    let y = y + 10;
    hline(img, MARGIN as f32, (pw - MARGIN) as f32, y as f32, FAINT, 1);
    y + 16
}

/// Draw one shelf row (marker + label + right-aligned count + active check) and push its full-width
/// tap target. `active` tints the row `CHIP` and adds a check on the right.
#[allow(clippy::too_many_arguments)]
fn shelf_row(img: &mut RgbImage, fonts: &Fonts, pw: i32, y: i32, marker: ShelfMarker, label: &str, count: Option<usize>, active: bool, target: ReaderTarget, hits: &mut HitMap) {
    if active {
        fill_rect(img, 0.0, y as f32, pw as f32, SHELF_ROW_H as f32, CHIP);
    }
    let cy = y + SHELF_ROW_H / 2;
    match marker {
        ShelfMarker::Icon(ic) => icon(img, &fonts.icons, ic.ch(), (MARGIN + 18) as f32, cy as f32, 34.0, INK),
        ShelfMarker::Status(st) => draw_status_marker(img, st, MARGIN + 6, cy),
        ShelfMarker::None => {}
    }
    draw_text(img, &fonts.sans, 34.0, (MARGIN + 56) as f32, (cy + 12) as f32, label, INK);
    let mut right = pw - MARGIN;
    if active {
        icon(img, &fonts.icons, Icon::Check.ch(), (right - 16) as f32, cy as f32, 30.0, INK);
        right -= 48;
    }
    if let Some(n) = count {
        let s = n.to_string();
        let sw = measure(&fonts.mono, 32.0, &s);
        draw_text(img, &fonts.mono, 32.0, right as f32 - sw, (cy + 11) as f32, &s, MUTED);
    }
    hits.push((Rect { x: 0, y, w: pw, h: SHELF_ROW_H }, target));
}

/// The full-screen author picker: a serif "Authors" masthead over full-width rows — "All authors"
/// first (clears the filter), then every distinct byline with its clipping count, most-clipped
/// first. Rows reuse [`shelf_row`] at the full screen width; the bottom bar is ‹ back › (paging on
/// the corners, back in the middle), mirroring the list's layout.
fn view_authors(model: &ReaderModel, scroll: usize, fonts: &Fonts, w: u32, h: u32) -> ViewOutput {
    let mut img = RgbImage::from_pixel(w, h, PAPER);
    let mut hits: HitMap = Vec::new();
    let iw = w as i32;
    let authors = model.authors();
    // Row 0 is the "All authors" clear row; the byline rows follow. Paged as one flat list.
    let total = authors.len() + 1;
    let per = authors_rows_per_page(h);

    draw_text(&mut img, &fonts.serif_bold, 46.0, MARGIN as f32, 72.0, "Authors", INK);
    let sub = format!("{} authors", authors.len());
    draw_text(&mut img, &fonts.sans, LIST_TS * 22.0, MARGIN as f32, 100.0, &sub, MUTED);
    let shown_end = (scroll + per).min(total);
    if total > 0 {
        let range = format!("{}–{} of {}", scroll + 1, shown_end, total);
        let rw = measure(&fonts.mono, 28.0, &range);
        draw_text(&mut img, &fonts.mono, 28.0, (iw - MARGIN) as f32 - rw, 72.0, &range, MUTED);
    }
    hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, (HEADER_H - 2) as f32, FAINT, 2);

    let rows_top = HEADER_H + 8;
    for row in scroll..shown_end {
        let y = rows_top + (row - scroll) as i32 * SHELF_ROW_H;
        if row == 0 {
            let on = model.author_filter.is_none();
            shelf_row(&mut img, fonts, iw, y, ShelfMarker::None, "All authors", None, on, ReaderTarget::SetAuthorFilter(None), &mut hits);
        } else {
            let (name, n) = &authors[row - 1];
            let on = model.author_filter.as_deref() == Some(name.as_str());
            let next = if on { None } else { Some(name.clone()) };
            shelf_row(&mut img, fonts, iw, y, ShelfMarker::None, name, Some(*n), on, ReaderTarget::SetAuthorFilter(next), &mut hits);
        }
    }

    // BOTTOM bar: ‹ prev on the left corner, Close in the middle, next › on the right corner.
    let icell = MIN_TAP_PX;
    let fy = h as i32 - FOOTER_H;
    let icon_y = fy + 18;
    hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, fy as f32, FAINT, 1);
    let slots = even_slots(iw, MARGIN, icell, 6);
    if scroll > 0 {
        hits.push((icon_btn(&mut img, fonts, Icon::ChevronLeft, slots[0], icon_y, icell, 60, INK), ReaderTarget::AuthorsPrev));
    } else {
        icon_btn(&mut img, fonts, Icon::ChevronLeft, slots[0], icon_y, icell, 60, FAINT);
    }
    hits.push((icon_btn(&mut img, fonts, Icon::Close, slots[2], icon_y, icell, 60, INK), ReaderTarget::CloseAuthors));
    if shown_end < total {
        hits.push((icon_btn(&mut img, fonts, Icon::ChevronRight, slots[5], icon_y, icell, 60, INK), ReaderTarget::AuthorsNext));
    } else {
        icon_btn(&mut img, fonts, Icon::ChevronRight, slots[5], icon_y, icell, 60, FAINT);
    }
    flash(&mut img, model, fonts, w, h);
    ViewOutput { frame: img, hits }
}

/// The library search screen: header (Search + result count + Done + result pager) → query bar →
/// live results (`draw_rows`) → docked QWERTY keyboard. The query lives on `model.query`, which
/// `visible()` already applies — so `model.visible()` here is the filtered result set.
fn view_search(model: &ReaderModel, scroll: usize, kb: bool, fonts: &Fonts, w: u32, h: u32) -> ViewOutput {
    let mut img = RgbImage::from_pixel(w, h, PAPER);
    let mut hits: HitMap = Vec::new();
    let iw = w as i32;
    let visible = model.visible();
    let q = model.query.trim();

    // Only the committed-results view (keyboard down) carries a selection; the keyboard modes
    // never do (selection is cleared on every search-mode change).
    let selection = if kb { None } else { model.selected.as_deref() };
    let per = search_rows_per_page(h, kb, selection.is_some());

    // Header: title + result count (left); the mode's actions + result pager (right).
    draw_text(&mut img, &fonts.serif_bold, 46.0, MARGIN as f32, 72.0, "Search", INK);
    let count = if let Some(sel) = selection {
        match sel.len() {
            0 => "Tap results to select".to_string(),
            1 => "1 selected".to_string(),
            n => format!("{n} selected"),
        }
    } else if q.is_empty() {
        format!("{} clippings", model.vault.len())
    } else if visible.is_empty() {
        "No matches".to_string()
    } else if visible.len() == 1 {
        "1 result".to_string()
    } else {
        format!("{} results", visible.len())
    };
    draw_text(&mut img, &fonts.sans, LIST_TS * 24.0, MARGIN as f32, 100.0, &count, MUTED);
    let mut chips_left = iw - MARGIN; // chips stack right-to-left from here
    if selection.is_some() {
        // Select mode: Cancel replaces Select/Done; the bulk actions are in the bottom select bar.
        let cancel = chip(&mut img, fonts, "Cancel", chips_left - 148, 34, 148, 60);
        hits.push((cancel, ReaderTarget::CancelSelect));
        chips_left -= 148 + 12;
    } else {
        let done = chip(&mut img, fonts, "Done", chips_left - 132, 34, 132, 60);
        hits.push((done, ReaderTarget::CloseSearch));
        chips_left -= 132 + 12;
        // Committed results can be batch-archived: "Select" enters multi-select.
        if !kb && !visible.is_empty() {
            let select = chip(&mut img, fonts, "Select", chips_left - 148, 34, 148, 60);
            hits.push((select, ReaderTarget::SelectMode));
            chips_left -= 148 + 12;
        }
    }
    let shown_end = (scroll + per).min(visible.len());
    if visible.len() > per {
        let bw = 64;
        let nx = chips_left - bw;
        let px = nx - 10 - bw;
        if scroll > 0 {
            hits.push((chip(&mut img, fonts, "‹", px, 34, bw, 60), ReaderTarget::ListPrev));
        }
        if shown_end < visible.len() {
            hits.push((chip(&mut img, fonts, "›", nx, 34, bw, 60), ReaderTarget::ListNext));
        }
    }
    hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, (HEADER_H - 2) as f32, FAINT, 2);

    // Query bar (its ✕ clears the query), then the results (or an empty-state hint). In the
    // committed-results view (keyboard down) the bar itself reopens the keyboard to refine — pushed
    // before the ✕ so the ✕ wins their overlap (hit-testing is last-drawn-first).
    if !kb {
        let bar = Rect { x: MARGIN, y: HEADER_H + 8, w: iw - 2 * MARGIN, h: SEARCH_QBAR_H };
        hits.push((bar, ReaderTarget::EditQuery));
    }
    let clear = draw_query_bar(&mut img, fonts, &model.query, iw);
    hits.push((clear, ReaderTarget::ClearQuery));
    let rows_top = HEADER_H + 8 + SEARCH_QBAR_H + 8;
    if !q.is_empty() && visible.is_empty() {
        draw_text(&mut img, &fonts.sans, LIST_TS * 26.0, MARGIN as f32, (rows_top + 80) as f32, "No clippings match your search.", MUTED);
    }
    draw_rows(&mut img, fonts, model, &visible, scroll, per, rows_top, iw, selection, &mut hits);

    // The shared on-screen keyboard — identical to the Agenda quick-add (QWERTY + 123/symbols +
    // Shift), drawn only while it's up (committed results reclaim its space). Map each returned
    // KeyCap to the search screen's own tap targets; "Search" commits the query into the
    // full-screen results view.
    if selection.is_some() {
        let page = &visible[scroll.min(visible.len())..shown_end];
        draw_select_bar(&mut img, fonts, model, page, iw, h as i32, None, &mut hits);
    }
    if kb {
        let kb_top = h as i32 - SEARCH_KB_H;
        for (rect, cap) in draw_keyboard(&mut img, fonts, kb_top, iw, model.search_layer, model.search_shift, "Search") {
            let target = match cap {
                KeyCap::Char(c) => ReaderTarget::Key(c),
                KeyCap::Space => ReaderTarget::Key(' '),
                KeyCap::Backspace => ReaderTarget::Backspace,
                KeyCap::Shift => ReaderTarget::KbShift,
                KeyCap::Layer => ReaderTarget::KbLayer,
                KeyCap::Enter => ReaderTarget::SubmitSearch,
            };
            hits.push((rect, target));
        }
    }
    flash(&mut img, model, fonts, w, h);
    ViewOutput { frame: img, hits }
}

/// The search text field: an outlined bar showing the typed query with a static caret (or a faint
/// placeholder when empty) and a Close (✕) at the right that clears the query. Long queries are
/// front-truncated so the tail being typed stays visible. Returns the ✕'s tap rect (→ `ClearQuery`).
fn draw_query_bar(img: &mut RgbImage, fonts: &Fonts, query: &str, iw: i32) -> Rect {
    let (x, y, w, hh) = (MARGIN, HEADER_H + 8, iw - 2 * MARGIN, SEARCH_QBAR_H);
    fill_rect(img, x as f32, y as f32, w as f32, hh as f32, PAPER);
    rect_outline(img, x as f32, y as f32, w as f32, hh as f32, INK, 2);
    let clear = icon_btn(img, fonts, Icon::Close, x + w - hh, y, hh, hh, MUTED);
    let text_x = (x + 20) as f32;
    let base = (y + hh / 2) as f32 + 14.0;
    let fs = 40.0;
    let max_w = (w - 40 - hh) as f32; // leave room for the ✕
    if query.is_empty() {
        draw_text(img, &fonts.sans, fs, text_x, base, "Search titles & sources…", FAINT);
        fill_rect(img, text_x, base - 40.0, 3.0, 50.0, INK); // caret at the start
    } else {
        let chars: Vec<char> = query.chars().collect();
        let mut start = 0;
        while start < chars.len() && measure(&fonts.sans, fs, &chars[start..].iter().collect::<String>()) > max_w {
            start += 1;
        }
        let shown: String = chars[start..].iter().collect();
        let tw = measure(&fonts.sans, fs, &shown);
        draw_text(img, &fonts.sans, fs, text_x, base, &shown, INK);
        fill_rect(img, text_x + tw + 3.0, base - 40.0, 3.0, 50.0, INK); // caret after the text
    }
    clear
}

/// Page-1 reading masthead geometry (article title + `BYLINE · DATE`, rendered inside the body).
/// Shared by the height reservation [`reading_masthead_height`] and [`draw_reading_masthead`] so the
/// space the layout reserves always matches what the view draws.
const MH_PAD_TOP: i32 = 22; // content_top → title cap
const MH_TITLE_PX: f32 = 34.0;
const MH_TITLE_LH: i32 = 44; // per serif title line
const MH_TITLE_BASE: i32 = 32; // baseline offset within a title line
const MH_BYLINE_PX: f32 = 24.0;
const MH_BYLINE_BAND: i32 = 40; // byline line + the gap above it
const MH_BYLINE_BASE: i32 = 28; // baseline offset within the byline band
const MH_PAD_BOT: i32 = 28; // masthead → article body
/// Body text column left margin — matches `layout::LayoutOpts::margin_x` (56) so the masthead lines up
/// with the article body beneath it.
const BODY_MARGIN: i32 = 56;
/// Where the page-1 masthead (and the article body) begin — hand-matched to `layout` `content_top`
/// (88 = `READING_HEADER_H` + the 8px gap under the top rule).
const MASTHEAD_TOP: i32 = READING_HEADER_H + 8;

/// Height (px) the page-1 reading masthead occupies, for the layout to reserve via
/// `LayoutOpts.first_page_extra`. `content_w` = the body text column width (`page_w − 2·margin_x`); the
/// title wraps to 1–2 lines exactly as [`draw_reading_masthead`] will draw it.
pub fn reading_masthead_height(fonts: &Fonts, title: &str, has_byline: bool, content_w: f32) -> i32 {
    let lines = wrap(&fonts.serif_bold, MH_TITLE_PX, title, content_w, 2).len() as i32;
    MH_PAD_TOP + lines * MH_TITLE_LH + if has_byline { MH_BYLINE_BAND } else { 0 } + MH_PAD_BOT
}

/// Draw the page-1 reading masthead — the article title over ≤2 serif lines + a `BYLINE · DATE` eyebrow
/// — starting at `top`, aligned to the body text margin. Non-interactive (the caller pushes no hits),
/// so it never enters the highlight text-run stream (mirrors how `PageItem::Image` is excluded).
fn draw_reading_masthead(img: &mut RgbImage, fonts: &Fonts, title: &str, byline: &str, date: &str, top: i32) {
    let content_w = (img.width() as i32 - 2 * BODY_MARGIN) as f32;
    let x = BODY_MARGIN as f32;
    let y0 = top + MH_PAD_TOP;
    let title_lines = wrap(&fonts.serif_bold, MH_TITLE_PX, title, content_w, 2);
    draw_text(img, &fonts.serif_bold, MH_TITLE_PX, x, (y0 + MH_TITLE_BASE) as f32, &title_lines[0], INK);
    if let Some(t2) = title_lines.get(1) {
        draw_text(img, &fonts.serif_bold, MH_TITLE_PX, x, (y0 + MH_TITLE_LH + MH_TITLE_BASE) as f32, t2, INK);
    }
    let lines = title_lines.len() as i32;
    let src = match (byline.is_empty(), date.is_empty()) {
        (false, false) => format!("{byline} · {date}"),
        (false, true) => byline.to_string(),
        (true, false) => date.to_string(),
        (true, true) => String::new(),
    };
    if !src.is_empty() {
        let yb = y0 + lines * MH_TITLE_LH + MH_BYLINE_BASE;
        let b = truncate(&fonts.sans_semibold, MH_BYLINE_PX, &src.to_uppercase(), content_w);
        draw_text_tracked(img, &fonts.sans_semibold, MH_BYLINE_PX, x, yb as f32, &b, MUTED, 1.5);
    }
}

/// The vertical extent of a page's laid-out content — min top / max bottom over ALL items (text,
/// images, rules, bullets). Anchors the focus preview bands, so a preview can never overlap the
/// page's own content even when an image sits flush against a content edge.
fn content_edges(page: &crate::layout::Page) -> Option<(f32, f32)> {
    use crate::layout::PageItem;
    let mut edges: Option<(f32, f32)> = None;
    for item in &page.items {
        let (t, b) = match item {
            PageItem::Text(r) => (r.line_top, r.line_top + r.line_h),
            PageItem::Image { y, h, .. } => (*y, *y + *h),
            PageItem::Rule { y, .. } => (*y - 1.0, *y + 1.0),
            PageItem::Bullet { baseline, px, .. } => (*baseline - *px, *baseline),
        };
        edges = Some(edges.map_or((t, b), |(et, eb)| (et.min(t), eb.max(b))));
    }
    edges
}

/// Focus TOP band: offset + clip that land the previous page's LAST text line with its bottom
/// exactly on `band_end` (the current page's top content edge) — consecutive lines share an edge in
/// the layout (`line_top + line_h` = next `line_top`), so the grid reads as continuous prose.
fn top_band(prev: &crate::layout::Page, band_end: f32) -> Option<(f32, Range<f32>)> {
    let (line_top, line_h) = prev.runs().map(|r| (r.line_top, r.line_h)).max_by(|a, b| a.0.total_cmp(&b.0))?;
    Some((band_end - (line_top + line_h), 0.0..band_end))
}

/// Focus BOTTOM band: offset + clip that land the next page's FIRST text line with its top exactly
/// on `band_start` (the current page's bottom content edge), continuing the grid downward until the
/// canvas runs out.
fn bottom_band(next: &crate::layout::Page, band_start: f32, canvas_h: f32) -> Option<(f32, Range<f32>)> {
    let line_top = next.runs().map(|r| r.line_top).min_by(f32::total_cmp)?;
    Some((band_start - line_top, band_start..canvas_h))
}

/// The source byte extent actually visible on the focus ENTRY screen — the central page plus every
/// preview-band line the view draws (via the exact [`crate::paint::runs_in_clip`] filter, so the
/// model and the pixels can never disagree). The first in-focus page turn continues from this
/// window's edge, which is what makes the transition seamless: no repeat, no gap.
pub fn focus_entry_window(pages: &[crate::layout::Page], page_ix: usize, canvas_h: f32) -> Range<usize> {
    let Some(page) = pages.get(page_ix) else {
        return 0..0;
    };
    let (top_edge, bottom_edge) = content_edges(page).unwrap_or((MASTHEAD_TOP as f32, canvas_h - FOOTER_H as f32));
    let mut start = page.runs().map(|g| g.src.start).min().unwrap_or(0);
    let mut end = page.runs().map(|g| g.src.end).max().unwrap_or(0);
    if page_ix > 0 {
        if let Some(prev) = pages.get(page_ix - 1) {
            if let Some((dy, clip)) = top_band(prev, top_edge) {
                if let Some(s) = crate::paint::runs_in_clip(prev, dy, &clip).map(|g| g.src.start).min() {
                    start = start.min(s);
                }
            }
        }
    }
    if let Some(next) = pages.get(page_ix + 1) {
        if let Some((dy, clip)) = bottom_band(next, bottom_edge, canvas_h) {
            if let Some(e) = crate::paint::runs_in_clip(next, dy, &clip).map(|g| g.src.end).max() {
                end = end.max(e);
            }
        }
    }
    start..end
}

fn view_reading(model: &ReaderModel, r: &Reading, fonts: &Fonts, w: u32, h: u32, images: &HashMap<String, RgbImage>) -> ViewOutput {
    // A book is read-only: no Fav/Archive in the bar, no status/tags in More, no highlight targets.
    let is_book = model.vault.get(r.file_ix).map(|f| f.kind == DocKind::Book).unwrap_or(false);
    let empty = crate::layout::Page::default();
    let page = r.page().unwrap_or(&empty);
    let mut img = draw_page_with_images(page, fonts, w, h, images);
    let mut hits: HitMap = Vec::new();
    let iw = w as i32;

    // Body tap zones tile the page horizontally: left third → previous page, centre third → focus
    // toggle, right third → next page. Pushed first so chrome / the tag panel shadow them on overlap;
    // they stay live in focus mode too (the early-return below keeps them), so pages turn full-screen.
    // Edge swipes still work as well (see `on_swipe`). Full height in focus (no chrome), the body
    // between header and footer otherwise.
    let total = r.pages.len();
    let third = iw / 3;
    let (body_top, body_bot) = if r.focus { (0, h as i32) } else { (READING_HEADER_H, h as i32 - FOOTER_H) };
    let body_h = body_bot - body_top;
    if r.page_ix > 0 {
        hits.push((Rect { x: 0, y: body_top, w: third, h: body_h }, ReaderTarget::PrevPage));
    }
    hits.push((Rect { x: third, y: body_top, w: third, h: body_h }, ReaderTarget::ToggleFocus));
    if r.page_ix + 1 < total {
        hits.push((Rect { x: iw - third, y: body_top, w: third, h: body_h }, ReaderTarget::NextPage));
    }

    // Only the chrome differs between the modes; the overlays after this branch (highlight hits, the
    // Note/Delete pill, the note editor, panels) are shared — a highlight must be long-pressable and
    // its menu visible in focus too.
    if r.focus {
        if r.focus_paginated {
            // Focus's own full-screen pagination (entered on the first in-focus page turn): the body
            // IS the whole screen and turns are continuous — no line ever repeats. Chrome-free bar
            // the page-1 masthead (drawn at the focus grid's content top +8; hand-matched to the
            // runtime's FOCUS_TOP, like READING_HEADER_H/content_top).
            if r.page_ix == 0 {
                const FOCUS_MASTHEAD_TOP: i32 = 14 + 8;
                let vf = model.vault.get(r.file_ix);
                let by = vf.and_then(|f| f.byline.clone()).unwrap_or_default();
                let date = vf.and_then(|f| f.published.clone()).unwrap_or_default();
                draw_reading_masthead(&mut img, fonts, &r.title, &by, &date, FOCUS_MASTHEAD_TOP);
            }
        } else {
            // Entry overlay: pagination untouched (toggling moved NOTHING — the central text is
            // pixel-identical to normal mode), with the freed chrome bands filled by the adjacent
            // pages' lines on a continuous grid. Preview lines are draw-only — no hits; the first
            // page turn switches to the focus pagination above, continuing after these very lines.
            let (top_edge, bottom_edge) = content_edges(page).unwrap_or((MASTHEAD_TOP as f32, (h as i32 - FOOTER_H) as f32));
            if r.page_ix == 0 {
                // No previous page — and the masthead space is reserved either way, so keep the
                // title/byline drawn: page 1 doesn't jump when toggling.
                let vf = model.vault.get(r.file_ix);
                let by = vf.and_then(|f| f.byline.clone()).unwrap_or_default();
                let date = vf.and_then(|f| f.published.clone()).unwrap_or_default();
                draw_reading_masthead(&mut img, fonts, &r.title, &by, &date, MASTHEAD_TOP);
            } else if let Some(prev) = r.pages.get(r.page_ix - 1) {
                if let Some((dy, clip)) = top_band(prev, top_edge) {
                    crate::paint::draw_text_runs_offset(&mut img, prev, fonts, dy, &clip);
                }
            }
            if let Some(next) = r.pages.get(r.page_ix + 1) {
                if let Some((dy, clip)) = bottom_band(next, bottom_edge, h as f32) {
                    crate::paint::draw_text_runs_offset(&mut img, next, fonts, dy, &clip);
                }
            }
        }
        // Remaining reading time, bottom-right, over a small paper knockout so nearby text (a
        // bottom preview line / the full-screen body) never collides with the label.
        let mins = r.page_minutes.get(r.page_ix).copied().unwrap_or(0);
        if mins > 0 {
            let label = format!("{mins} min left");
            let tw = measure(&fonts.mono, 26.0, &label);
            let lx = w as f32 - MARGIN as f32 - tw;
            fill_rect(&mut img, lx - 12.0, h as f32 - 48.0, tw + 24.0, 40.0, PAPER);
            draw_text(&mut img, &fonts.mono, 26.0, lx, h as f32 - 18.0, &label, MUTED);
        }
    } else {
        // Slim top chrome (all pages): a thin progress bar with "X min left" right-aligned on the same
        // line, then a hairline. No buttons up here, so taps in this band fall through (the body zones
        // start at READING_HEADER_H). The title/byline/date live in the page-1 body masthead below.
        let mins = r.page_minutes.get(r.page_ix).copied().unwrap_or(0);
        let time = if mins > 0 { format!("{mins} min left") } else { String::new() };
        let bar_y = 48;
        let tw = if time.is_empty() { 0.0 } else { measure(&fonts.mono, 26.0, &time) };
        let bar_x1 = if time.is_empty() { (iw - MARGIN) as f32 } else { (iw - MARGIN) as f32 - tw - 24.0 };
        hline(&mut img, MARGIN as f32, bar_x1, bar_y as f32, FAINT, 4);
        let frac = (r.page_ix + 1) as f32 / total.max(1) as f32;
        let fillw = ((bar_x1 - MARGIN as f32) * frac).round();
        if fillw > 0.0 {
            hline(&mut img, MARGIN as f32, MARGIN as f32 + fillw, bar_y as f32, INK, 4);
        }
        if !time.is_empty() {
            draw_text(&mut img, &fonts.mono, 26.0, (iw - MARGIN) as f32 - tw, (bar_y + 9) as f32, &time, MUTED);
        }
        hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, (READING_HEADER_H - 2) as f32, FAINT, 1);

        // Page-1 article masthead — the title + byline·date rendered inside the body (page 1 only), in
        // the space `LayoutOpts.first_page_extra` reserved. Non-interactive (pushes no hits). Pages 2+
        // start straight into the body.
        if r.page_ix == 0 {
            let vf = model.vault.get(r.file_ix);
            let by = vf.and_then(|f| f.byline.clone()).unwrap_or_default();
            let date = vf.and_then(|f| f.published.clone()).unwrap_or_default();
            draw_reading_masthead(&mut img, fonts, &r.title, &by, &date, MASTHEAD_TOP);
        }

        // Bottom action bar — every reading control, evenly spaced across the full width (portrait:
        // thumb-reachable). No page-turn arrows: the body thirds + edge swipes page. Reading status,
        // content tags and text size live behind ⋯ More; progress moved to the top chrome. Highlights
        // are removed by tapping them (the floating menu below), so there's no Undo control here.
        let icell = MIN_TAP_PX;
        let fy = h as i32 - FOOTER_H;
        let icon_y = fy + 18;
        hline(&mut img, MARGIN as f32, (iw - MARGIN) as f32, fy as f32, FAINT, 1);
        // Evenly distribute N=5 cells: slot 0 on the left margin, slot 4 on the right, equal gaps
        // between (shared [`even_slots`] with the Agenda bar).
        if is_book {
            // Books are read-only, so the bar slims to navigation: ‹ back · ☰ contents · ⋯ more.
            let slots = even_slots(iw, MARGIN, icell, 3);
            let bx = |k: i32| slots[k as usize];
            hits.push((icon_btn(&mut img, fonts, Icon::ChevronLeft, bx(0), icon_y, icell, 60, INK), ReaderTarget::Back));
            hits.push((icon_btn(&mut img, fonts, Icon::Menu, bx(1), icon_y, icell, 60, INK), ReaderTarget::OpenContents));
            hits.push((icon_btn(&mut img, fonts, Icon::More, bx(2), icon_y, icell, 60, INK), ReaderTarget::OpenMore));
        } else {
            let slots = even_slots(iw, MARGIN, icell, 5);
            let bx = |k: i32| slots[k as usize];
            hits.push((icon_btn(&mut img, fonts, Icon::ChevronLeft, bx(0), icon_y, icell, 60, INK), ReaderTarget::Back));
            let fav_on = model.vault.get(r.file_ix).map(|f| f.tags.contains(&Tag::Favourite)).unwrap_or(false);
            let (fav_ic, fav_ink) = if fav_on { (Icon::StarFill, GOLD) } else { (Icon::Star, INK) };
            hits.push((icon_btn(&mut img, fonts, fav_ic, bx(1), icon_y, icell, 60, fav_ink), ReaderTarget::ToggleTag(Tag::Favourite)));
            let archived = model.vault.get(r.file_ix).map(|f| f.tags.contains(&Tag::Archive)).unwrap_or(false);
            let arch_ic = if archived { Icon::ArchiveFill } else { Icon::Archive };
            hits.push((icon_btn(&mut img, fonts, arch_ic, bx(2), icon_y, icell, 60, INK), ReaderTarget::ToggleTag(Tag::Archive)));
            hits.push((icon_btn(&mut img, fonts, Icon::Menu, bx(3), icon_y, icell, 60, INK), ReaderTarget::OpenContents));
            hits.push((icon_btn(&mut img, fonts, Icon::More, bx(4), icon_y, icell, 60, INK), ReaderTarget::OpenMore));
        }
    }

    // Make every highlighted span tappable (opens the floating menu) and mark noted ones with a small
    // glyph. Pushed after the body zones + toolbar so a tap on a highlight wins over the page-turn thirds.
    // Not for books: read-only, so there's no highlight menu to open (and no `==` spans to find).
    if !is_book {
        highlight_spans(&mut img, fonts, page, &r.source, iw, &mut hits);
    }

    // Slide-in panels (drawn + hit-tested last so they shadow the page + toolbar). The runtime keeps
    // the two flags mutually exclusive; Contents wins if both are somehow set.
    if r.contents_open {
        draw_contents(&mut img, fonts, r, iw, h as i32, &mut hits);
    } else if r.more_open {
        draw_more_sheet(&mut img, fonts, model, r, iw, h as i32, &mut hits);
    }
    // The note-entry sheet (opened from the highlight menu) shadows everything, including the toolbar.
    if let Some(note) = &r.note {
        draw_note_sheet(&mut img, fonts, note, iw, h as i32, &mut hits);
    }
    // The floating highlight action menu (Note / Delete) — drawn last so it shadows the page + bars.
    if let Some(menu) = &r.highlight_menu {
        draw_highlight_toolbar(&mut img, fonts, menu, iw, h as i32, &mut hits);
    }

    flash(&mut img, model, fonts, w, h);
    ViewOutput { frame: img, hits }
}

/// Walk the page's glyph runs, grouping consecutive highlighted runs into spans. For each span, push a
/// `TapHighlight` hit per run box (so a tap anywhere on the highlight opens the menu, even across a line
/// wrap), and — when the span carries a note — draw a small note glyph in the right margin, aligned to
/// the highlight's last line (a per-line "this highlight has a note" marker that never collides with the
/// body text that may follow the highlight mid-sentence). The range is the span's inner byte range, the
/// same one `remove_highlight_and_note` / `note_for_highlight` expect.
fn highlight_spans(img: &mut RgbImage, fonts: &Fonts, page: &crate::layout::Page, source: &str, iw: i32, hits: &mut HitMap) {
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
        let span = &runs[start..i];
        let range: Range<usize> = span[0].src.start..span[span.len() - 1].src.end;
        for run in span {
            let rect = Rect { x: run.x as i32, y: run.line_top as i32, w: run.w.ceil() as i32, h: run.line_h.ceil() as i32 };
            hits.push((rect, ReaderTarget::TapHighlight { range: range.clone() }));
        }
        if crate::parse::note_for_highlight(source, range.start).is_some() {
            let last = span[span.len() - 1];
            let cx = (iw - MARGIN / 2) as f32; // centred in the right margin, clear of the text column
            let cy = last.line_top + last.line_h / 2.0;
            icon(img, &fonts.icons, Icon::Note.ch(), cx, cy, 24.0, MUTED);
        }
    }
}

/// The floating highlight action menu — a small dark rounded pill centred above the tapped highlight
/// with two cells: **Note** (or "Edit note" when the highlight already has one) and **Delete**, each a
/// PAPER glyph over a PAPER label on the INK pill. A full-screen dismiss rect underneath closes it on a
/// tap-out. Constructive action first, destructive last (fewer mis-taps). Drawn last so it shadows all.
/// The floating pill's cell/geometry constants — shared with [`highlight_pill_rect`] so the
/// update layer can compute a partial-refresh region matching exactly what gets drawn.
const PILL_PAD: f32 = 18.0;
const PILL_CELL_W: f32 = 150.0;
const PILL_CELLS: usize = 2;
const PILL_H: f32 = 108.0;

/// Where the highlight pill lands for a menu anchored at `(x, y)` on a canvas `iw` wide —
/// centre above the tap, clamp on-screen, flip below if it would clip the top chrome. Pure, so
/// `update` can emit a `Dirty::Region` for menu open/close instead of a whole-panel flash.
pub(crate) fn highlight_pill_rect(x: i32, y: i32, iw: i32) -> Rect {
    let pill_w = PILL_CELL_W * PILL_CELLS as f32 + PILL_PAD * 2.0;
    let px = (x as f32 - pill_w / 2.0).clamp(MARGIN as f32, (iw - MARGIN) as f32 - pill_w);
    let above = y as f32 - PILL_H - 24.0;
    let py = if above < READING_HEADER_H as f32 { y as f32 + 36.0 } else { above };
    Rect { x: px as i32, y: py as i32, w: pill_w.ceil() as i32, h: PILL_H.ceil() as i32 }
}

fn draw_highlight_toolbar(img: &mut RgbImage, fonts: &Fonts, menu: &crate::model::HighlightMenu, iw: i32, ih: i32, hits: &mut HitMap) {
    // Tap-out closes (pushed first; the pill's own cells, pushed after, win where they overlap).
    hits.push((Rect { x: 0, y: 0, w: iw, h: ih }, ReaderTarget::HighlightMenuDismiss));

    let note_label = if menu.has_note { "Edit note" } else { "Note" };
    let cells: [(Icon, &str, ReaderTarget); 2] = [
        (Icon::Note, note_label, ReaderTarget::HighlightAddNote),
        (Icon::Trash, "Delete", ReaderTarget::HighlightDelete),
    ];
    let pad = PILL_PAD;
    let cell_w = PILL_CELL_W;
    let rect = highlight_pill_rect(menu.x, menu.y, iw);
    let (px, py) = (rect.x as f32, rect.y as f32);
    let pill_w = rect.w as f32;
    let pill_h = PILL_H;

    // Rounded INK pill: a horizontal + vertical bar inset by the radius, corners filled with discs.
    let rad = 16.0;
    fill_rect(img, px + rad, py, pill_w - 2.0 * rad, pill_h, INK);
    fill_rect(img, px, py + rad, pill_w, pill_h - 2.0 * rad, INK);
    for (dx, dy) in [(rad, rad), (pill_w - rad, rad), (rad, pill_h - rad), (pill_w - rad, pill_h - rad)] {
        disc(img, px + dx, py + dy, rad, INK);
    }

    for (k, (ic, label, target)) in cells.iter().enumerate() {
        let cell_x = px + pad + cell_w * k as f32;
        let cx = cell_x + cell_w / 2.0;
        icon(img, &fonts.icons, ic.ch(), cx, py + 40.0, 34.0, PAPER); // glyph, centred, upper half
        let lw = measure(&fonts.sans, 24.0, label);
        draw_text(img, &fonts.sans, 24.0, cx - lw / 2.0, py + pill_h - 22.0, label, PAPER);
        hits.push((Rect { x: cell_x as i32, y: py as i32, w: cell_w as i32, h: pill_h as i32 }, target.clone()));
    }
}

/// The note-entry sheet — opened from the tap-a-highlight menu. A bottom sheet with a text field
/// showing the typed note and the shared on-screen keyboard (Enter labelled "Save"; mirrors the
/// Agenda quick-add). Drawn + hit-tested last so it shadows the page and toolbar.
fn draw_note_sheet(img: &mut RgbImage, fonts: &Fonts, note: &NoteEntry, iw: i32, ih: i32, hits: &mut HitMap) {
    let top = ih - NOTE_SHEET_H;
    // Tapping the page above the sheet cancels (pushed first; the sheet's own targets, pushed after,
    // win where they overlap — hit-testing is last-drawn-first).
    hits.push((Rect { x: 0, y: 0, w: iw, h: top }, ReaderTarget::NoteCancel));
    let (_body, close) = draw_bottom_sheet(img, fonts, "Add note", NOTE_SHEET_H, iw, ih);

    // The note text field, between the sheet header and the docked keyboard.
    let fy = top + SHEET_HEADER_H + 16;
    let fw = iw - 2 * MARGIN;
    let fh = NOTE_FIELD_H - 32;
    fill_rect(img, MARGIN as f32, fy as f32, fw as f32, fh as f32, PAPER);
    rect_outline(img, MARGIN as f32, fy as f32, fw as f32, fh as f32, INK, 2);
    let text_x = (MARGIN + 20) as f32;
    let base = (fy + fh / 2) as f32 + 14.0;
    let fs = 40.0;
    let max_w = (fw - 40) as f32;
    if note.buf.is_empty() {
        draw_text(img, &fonts.sans, fs, text_x, base, "Type a note…", FAINT);
        fill_rect(img, text_x, base - 40.0, 3.0, 50.0, INK); // caret at the start
    } else {
        // Front-truncate so the tail being typed stays visible (mirrors `draw_query_bar`).
        let chars: Vec<char> = note.buf.chars().collect();
        let mut start = 0;
        while start < chars.len() && measure(&fonts.sans, fs, &chars[start..].iter().collect::<String>()) > max_w {
            start += 1;
        }
        let shown: String = chars[start..].iter().collect();
        let tw = measure(&fonts.sans, fs, &shown);
        draw_text(img, &fonts.sans, fs, text_x, base, &shown, INK);
        fill_rect(img, text_x + tw + 3.0, base - 40.0, 3.0, 50.0, INK); // caret after the text
    }
    hits.push((close, ReaderTarget::NoteCancel));

    // The docked keyboard (Enter labelled "Save"), sitting exactly where the search keyboard does.
    let kb_top = ih - SEARCH_KB_H;
    for (rect, cap) in draw_keyboard(img, fonts, kb_top, iw, note.layer, note.shift, "Save") {
        let target = match cap {
            KeyCap::Char(c) => ReaderTarget::NoteKey(c),
            KeyCap::Space => ReaderTarget::NoteKey(' '),
            KeyCap::Backspace => ReaderTarget::NoteBackspace,
            KeyCap::Shift => ReaderTarget::NoteShift,
            KeyCap::Layer => ReaderTarget::NoteLayer,
            KeyCap::Enter => ReaderTarget::NoteSave,
        };
        hits.push((rect, target));
    }
}

/// The "More" sheet — a left-docked panel over the reading view holding the secondary controls that
/// aren't on the bottom action bar: text size (Aa−/Aa+), the reading-status picker, and the managed
/// content tags. Each row acts on the open clipping in place; the sheet stays open for repeated taps,
/// closing on the Close button or a tap outside. Drawn + hit-tested last so it shadows the page.
fn draw_more_sheet(img: &mut RgbImage, fonts: &Fonts, model: &ReaderModel, r: &Reading, iw: i32, ih: i32, hits: &mut HitMap) {
    let pw = draw_panel_chrome(img, fonts, "More", ReaderTarget::CloseMore, iw, ih, hits);
    let current = model.vault.get(r.file_ix).map(|f| f.tags.as_slice()).unwrap_or(&[]);
    let status = model.reading_status();
    let mut y = 124;
    // Text size — folded in from the old toolbar Aa−/Aa+ (the most-used reading control, so first).
    panel_eyebrow(img, fonts, y, "TEXT SIZE");
    y += 52;
    shelf_row(img, fonts, pw, y, ShelfMarker::None, "Aa−   Smaller", None, false, ReaderTarget::TextSmaller, hits);
    y += SHELF_ROW_H;
    shelf_row(img, fonts, pw, y, ShelfMarker::None, "Aa+   Larger", None, false, ReaderTarget::TextLarger, hits);
    y += SHELF_ROW_H;
    y = shelf_divider(img, pw, y);
    // A book is read-only — no status to set, no tags to assign; the sheet is just text size.
    if model.reading_is_book() {
        return;
    }
    panel_eyebrow(img, fonts, y, "READING STATUS");
    y += 52;
    for st in [ReadStatus::NotRead, ReadStatus::Reading, ReadStatus::Read] {
        shelf_row(img, fonts, pw, y, ShelfMarker::Status(st), st.label(), None, status == st, ReaderTarget::SetStatusFor(r.file_ix, st), hits);
        y += SHELF_ROW_H;
    }
    y = shelf_divider(img, pw, y);
    panel_eyebrow(img, fonts, y, "TAGS");
    y += 52;
    for t in Tag::CONTENT {
        let on = current.contains(&t);
        shelf_row(img, fonts, pw, y, ShelfMarker::Icon(Icon::Tag), t.label(), None, on, ReaderTarget::ToggleTag(t), hits);
        y += SHELF_ROW_H;
    }
}

/// The Contents panel — a left-docked table of contents built from the open document's headings.
/// Tapping a heading jumps to the page it starts on; the heading(s) on the current page are marked.
/// Drawn + hit-tested last so it shadows the page beneath.
fn draw_contents(img: &mut RgbImage, fonts: &Fonts, r: &Reading, iw: i32, ih: i32, hits: &mut HitMap) {
    let pw = draw_panel_chrome(img, fonts, "Contents", ReaderTarget::CloseContents, iw, ih, hits);
    let toc = doc_headings(&r.pages);
    if toc.is_empty() {
        draw_text(img, &fonts.sans, 34.0, MARGIN as f32, 190.0, "No headings in this article.", MUTED);
        return;
    }
    let avail = (pw - 2 * MARGIN - 56 - 56) as f32; // label start → panel edge, less the active check
    let mut y = 124;
    for (pi, text) in toc {
        if y + SHELF_ROW_H > ih {
            break; // a very long TOC just fills the panel; no scroll (panels are single-screen)
        }
        let label = truncate(&fonts.sans, 34.0, &text, avail);
        shelf_row(img, fonts, pw, y, ShelfMarker::None, &label, None, pi == r.page_ix, ReaderTarget::JumpToHeading(pi), hits);
        y += SHELF_ROW_H;
    }
}

/// Extract a flat table of contents from laid-out pages: for each page, the text of any heading that
/// starts on it, paired with the page index. Reads the `heading` flag the layouter sets on glyph
/// runs, so it always reflects the current text size (pages re-flow on Aa±).
fn doc_headings(pages: &[crate::layout::Page]) -> Vec<(usize, String)> {
    let mut toc = Vec::new();
    for (pi, page) in pages.iter().enumerate() {
        let mut cur = String::new();
        for run in page.runs() {
            if run.heading {
                let t = run.text.trim();
                if !t.is_empty() {
                    if !cur.is_empty() {
                        cur.push(' ');
                    }
                    cur.push_str(t);
                }
            } else if !cur.is_empty() {
                toc.push((pi, std::mem::take(&mut cur)));
            }
        }
        if !cur.is_empty() {
            toc.push((pi, std::mem::take(&mut cur)));
        }
    }
    toc
}

/// A pale rounded-ish chip with centered label; returns its rect (a tap target).
fn chip(img: &mut RgbImage, fonts: &Fonts, label: &str, x: i32, y: i32, wdt: i32, hgt: i32) -> Rect {
    chip_ink(img, fonts, label, x, y, wdt, hgt, INK)
}

/// [`chip`] with the label colour spelled out — MUTED marks a chip that's drawn but inert (no hit
/// pushed), the way the select bar's actions look until something is ticked.
fn chip_ink(img: &mut RgbImage, fonts: &Fonts, label: &str, x: i32, y: i32, wdt: i32, hgt: i32, ink: Rgb<u8>) -> Rect {
    fill_rect(img, x as f32, y as f32, wdt as f32, hgt as f32, CHIP);
    rect_outline(img, x as f32, y as f32, wdt as f32, hgt as f32, FAINT, 1);
    let tw = measure(&fonts.sans, 30.0, label);
    draw_text(img, &fonts.sans, 30.0, x as f32 + (wdt as f32 - tw) / 2.0, y as f32 + hgt as f32 * 0.66, label, ink);
    Rect { x, y, w: wdt, h: hgt }
}

/// The multi-select action bar, in the footer strip of the library list and the committed search
/// results: `‹  All  Archive  Read  Fav  ›`. Four evenly spread chips, with the pager chevrons on
/// the corners where the list's nav bar keeps them (`pager` = whether prev/next are live; `None` on
/// the search screen, whose header carries its own pager).
///
/// Three of the chips flip label with the selection — "None" once every row on this page is ticked,
/// "Unread"/"Unfav" once every ticked clipping already carries the thing — so one chip covers both
/// directions and the bar stays at four. `page` is the vault indices drawn on this page, i.e. what
/// "All" ticks. Archive/Read/Fav are drawn MUTED and push no hit while nothing is ticked.
fn draw_select_bar(img: &mut RgbImage, fonts: &Fonts, model: &ReaderModel, page: &[usize], iw: i32, ih: i32, pager: Option<(bool, bool)>, hits: &mut HitMap) {
    let sel = model.selected.as_deref().unwrap_or(&[]);
    let fy = ih - FOOTER_H;
    let y = fy + 18;
    hline(img, MARGIN as f32, (iw - MARGIN) as f32, fy as f32, FAINT, 1);

    let (mut left, mut right) = (MARGIN, iw - MARGIN);
    if let Some((prev, next)) = pager {
        let cell = MIN_TAP_PX;
        let (pink, nink) = (if prev { INK } else { FAINT }, if next { INK } else { FAINT });
        let pr = icon_btn(img, fonts, Icon::ChevronLeft, left, y, cell, 60, pink);
        let nr = icon_btn(img, fonts, Icon::ChevronRight, right - cell, y, cell, 60, nink);
        if prev {
            hits.push((pr, ReaderTarget::ListPrev));
        }
        if next {
            hits.push((nr, ReaderTarget::ListNext));
        }
        left += cell + CHIP_GAP;
        right -= cell + CHIP_GAP;
    }

    let cw = (right - left - 3 * CHIP_GAP) / 4;
    let all_ticked = !page.is_empty() && page.iter().all(|i| sel.contains(i));
    let live = !sel.is_empty();
    let actions = [
        (if all_ticked { "None" } else { "All" }, !page.is_empty(), ReaderTarget::SelectAll),
        ("Archive", live, ReaderTarget::ArchiveSelected),
        (if model.selection_all_read() { "Unread" } else { "Read" }, live, ReaderTarget::ReadSelected),
        (if model.selection_all_fav() { "Unfav" } else { "Fav" }, live, ReaderTarget::FavSelected),
    ];
    for (k, (label, enabled, target)) in actions.into_iter().enumerate() {
        let x = left + k as i32 * (cw + CHIP_GAP);
        let ink = if enabled { INK } else { MUTED };
        let rect = chip_ink(img, fonts, label, x, y, cw, 60, ink);
        if enabled {
            hits.push((rect, target));
        }
    }
}

/// A small status marker for a list row (left of the title): filled = read, amber = reading,
/// hollow ring = not read.
fn draw_status_marker(img: &mut RgbImage, status: ReadStatus, x: i32, cy: i32) {
    let (cx, cy) = (x as f32 + 12.0, cy as f32);
    match status {
        ReadStatus::Read => disc(img, cx, cy, 12.0, INK),
        ReadStatus::Reading => disc(img, cx, cy, 12.0, TINT),
        ReadStatus::NotRead => {
            disc(img, cx, cy, 12.0, FAINT);
            disc(img, cx, cy, 8.0, PAPER);
        }
    }
}

/// A one-line notice centered just above the footer.
fn flash(img: &mut RgbImage, model: &ReaderModel, fonts: &Fonts, w: u32, h: u32) {
    let Some(f) = &model.flash else { return };
    let y = h as i32 - FOOTER_H - 56;
    let tw = measure(&fonts.sans_semibold, 30.0, &f.text);
    let pad = 24.0;
    let bx = (w as f32 - tw) / 2.0 - pad;
    fill_rect(img, bx, y as f32 - 44.0, tw + 2.0 * pad, 60.0, Rgb([40, 40, 40]));
    draw_text(img, &fonts.sans_semibold, 30.0, (w as f32 - tw) / 2.0, y as f32, &f.text, PAPER);
}

/// Remaining reading minutes for `text` at ~300 wpm (≈5 chars/word ⇒ 1500 alphanumeric chars/min),
/// matching the "Note Reading Progress" Obsidian plugin. Rounds up; 0 only when nothing's left.
fn reading_minutes(text: &str) -> u32 {
    let chars = text.chars().filter(|c| c.is_alphanumeric()).count();
    if chars == 0 {
        0
    } else {
        (chars as f32 / 1500.0).ceil() as u32
    }
}

/// "N min left" per page — [`reading_minutes`] of the source from each page's first byte on, exactly
/// as the reading chrome used to compute per redraw. Precomputed whenever `source`/`pages` install
/// (see [`crate::model::Reading::page_minutes`]) so a redraw is a lookup, not a full-document scan.
pub fn page_minutes(source: &str, pages: &[crate::layout::Page]) -> Vec<u32> {
    pages
        .iter()
        .map(|page| {
            let top = page.runs().map(|g| g.src.start).min().unwrap_or(0);
            reading_minutes(source.get(top..).unwrap_or(""))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadStatus, Reading, ReaderModel, Screen, Tag, VaultFile};
    use crate::{layout, parse, LayoutOpts};
    use std::path::PathBuf;

    fn fonts() -> Fonts {
        Fonts::embedded()
    }
    fn vault(n: usize) -> Vec<VaultFile> {
        (0..n)
            .map(|i| VaultFile {
                kind: crate::model::DocKind::Article,
                path: PathBuf::from(format!("/v/{i}.md")),
                title_lower: format!("clipping number {i}"),
                title: format!("Clipping number {i}"),
                has_highlights: i % 3 == 0,
                published: Some(format!("2025-01-{:02}", (i % 28) + 1)),
                created: None,
                byline_lower: Some(format!("source {i}")),
                byline: Some(format!("Source {i}")),
                status: match i % 3 {
                    0 => ReadStatus::Read,
                    1 => ReadStatus::Reading,
                    _ => ReadStatus::NotRead,
                },
                tags: match i % 5 {
                    0 => vec![Tag::Ai],
                    1 => vec![Tag::Health, Tag::Work],
                    _ => Vec::new(),
                },
                minutes: 2 + (i as u32 % 12),
            })
            .collect()
    }

    #[test]
    fn list_shows_rows_and_paging_targets() {
        let model = ReaderModel::new(vault(100));
        let out = view(&model, &fonts(), 954, 1696);
        assert_eq!(out.frame.dimensions(), (954, 1696));
        let opens = out.hits.iter().filter(|(_, t)| matches!(t, ReaderTarget::OpenFile(_))).count();
        assert!(opens > 5 && opens <= list_rows_per_page(1696), "a screenful of rows: {opens}");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::ListNext), "next-page present");
        assert!(!out.hits.iter().any(|(_, t)| *t == ReaderTarget::ListPrev), "no prev on first page");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::Exit));
    }

    #[test]
    fn reading_has_back_and_page_nav() {
        let src = "# Title\n\n".to_string() + &"word ".repeat(1200);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(pages.len() > 2);
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src, title: "Title".into(), doc, pages, page_ix: 1, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::Back));
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::PrevPage), "mid-doc has prev");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::NextPage), "mid-doc has next");
    }

    /// The focus-paginated content grid (must match the runtime's FocusPaginate arm in main.rs:
    /// FOCUS_TOP = 14, bottom inset 56 reserving the "N min left" corner).
    fn focus_opts() -> LayoutOpts {
        LayoutOpts { content_top: 14.0, content_bottom: 1696.0 - 56.0, ..LayoutOpts::default() }
    }

    /// A Reading screen over `src` at `page_ix`. `focus: true` = the ENTRY overlay: pagination is
    /// the normal one (the toggle moves nothing); the view adds the preview bands.
    fn reading_model(src: &str, page_ix: usize, focus: bool) -> ReaderModel {
        let doc = parse(src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(page_ix < pages.len(), "fixture page exists");
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(src, &pages), file_ix: 0, source: src.to_string(), title: "Title".into(), doc, pages, page_ix, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus, focus_paginated: false, note: None, highlight_menu: None });
        model
    }

    /// A Reading screen in the focus-PAGINATED sub-state: focus's own full-screen grid, as after the
    /// first in-focus page turn.
    fn reading_model_paginated(src: &str, page_ix: usize) -> ReaderModel {
        let doc = parse(src);
        let pages = layout(&doc, &fonts(), focus_opts());
        assert!(page_ix < pages.len(), "fixture page exists");
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(src, &pages), file_ix: 0, source: src.to_string(), title: "Title".into(), doc, pages, page_ix, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: true, focus_paginated: true, note: None, highlight_menu: None });
        model
    }

    fn ink_in_rows(img: &image::RgbImage, y0: u32, y1: u32) -> usize {
        (y0..y1.min(img.height()))
            .flat_map(|y| (0..img.width()).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = img.get_pixel(x, y);
                (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384
            })
            .count()
    }

    #[test]
    fn the_focus_entry_overlay_keeps_the_central_text_identical_and_fills_the_bands() {
        // The zero-jump guarantee: toggling focus changes NO pagination, so the page's own content
        // rows are pixel-identical across the toggle; the freed bands gain adjacent-page text.
        let src = "word ".repeat(1200); // several pages
        let model = reading_model(&src, 1, false);
        let (top, bot) = {
            let Screen::Reading(r) = &model.screen else { unreachable!() };
            let (t, b) = content_edges(r.page().unwrap()).unwrap();
            (t.floor() as u32, b.ceil() as u32)
        };
        let normal = view(&model, &fonts(), 954, 1696);
        let entry = view(&reading_model(&src, 1, true), &fonts(), 954, 1696);
        for y in top..bot {
            for x in 0..954u32 {
                assert_eq!(normal.frame.get_pixel(x, y), entry.frame.get_pixel(x, y), "central text moved at ({x},{y})");
            }
        }
        assert!(ink_in_rows(&entry.frame, 0, top) > 50, "top band shows the previous page's tail");
        assert!(ink_in_rows(&entry.frame, bot, 1696) > 50, "bottom band shows the next page's head");
    }

    #[test]
    fn focus_paginated_pages_are_full_bleed_and_tile_without_repeats() {
        // After the first in-focus turn: full-screen pages on the focus grid — text above the normal
        // content top, pages tiling the source with no overlap, and the body structurally clear of
        // the "N min left" corner (content_bottom leaves the label band empty).
        let src = "word ".repeat(1200);
        let model = reading_model_paginated(&src, 1);
        let out = view(&model, &fonts(), 954, 1696);
        assert!(ink_in_rows(&out.frame, 14, 88) > 50, "focus text fills the old top-chrome band");
        let Screen::Reading(r) = &model.screen else { unreachable!() };
        let last_bottom = r.pages[1].runs().map(|g| g.line_top + g.line_h).fold(0.0f32, f32::max);
        assert!(last_bottom > 1600.0, "focus lines extend past the normal content bottom: {last_bottom}");
        assert!(last_bottom <= 1640.5, "body text stays clear of the reading-time corner: {last_bottom}");
        for w in r.pages.windows(2) {
            let end = w[0].runs().map(|g| g.src.end).max().unwrap_or(0);
            let next = w[1].runs().map(|g| g.src.start).min().unwrap_or(usize::MAX);
            assert!(next >= end, "a line repeats across a focus page turn: {next} < {end}");
        }
        // Design constraint: nothing but the (right-aligned) time label below the body — the left
        // side of the label band stays pure paper.
        let left_ink = (1648..1696u32)
            .flat_map(|y| (0..600u32).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = out.frame.get_pixel(x, y);
                (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384
            })
            .count();
        assert_eq!(left_ink, 0, "no body ink in the reading-time band");
    }

    #[test]
    fn focus_page_one_keeps_the_masthead_in_both_sub_states() {
        let src = "word ".repeat(1200);
        // Entry overlay: masthead exactly where normal mode draws it (nothing moves on toggle).
        let doc = parse(&src);
        let mut opts = LayoutOpts::default();
        opts.first_page_extra = 180.0; // stand-in for the runtime's masthead_extra reservation
        let pages = layout(&doc, &fonts(), opts);
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc, pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: true, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        assert_eq!(ink_in_rows(&out.frame, 0, 88), 0, "nothing above the masthead on the entry page 1");
        assert!(ink_in_rows(&out.frame, MASTHEAD_TOP as u32, MASTHEAD_TOP as u32 + 80) > 100, "the title stays drawn on entry");
        // Paginated: masthead at the focus grid's top (+8), body clear of the reservation.
        let doc = parse(&src);
        let mut fopts = focus_opts();
        fopts.first_page_extra = 180.0;
        let pages = layout(&doc, &fonts(), fopts);
        if let Screen::Reading(r) = &mut model.screen {
            r.page_minutes = page_minutes(&src, &pages);
            r.doc = doc;
            r.pages = pages;
            r.focus_paginated = true;
        }
        let out = view(&model, &fonts(), 954, 1696);
        assert!(ink_in_rows(&out.frame, 22, 22 + 80) > 100, "the title is drawn on paginated page 1");
        let Screen::Reading(r) = &model.screen else { unreachable!() };
        let first_line = r.pages[0].runs().map(|g| g.line_top as i32).min().unwrap();
        assert!(first_line >= 14 + 180, "body clears the masthead reservation: {first_line}");
    }

    #[test]
    fn focus_hits_are_only_the_nav_thirds() {
        let src = "word ".repeat(1200);
        let model = reading_model(&src, 0, true);
        let out = view(&model, &fonts(), 954, 1696);
        // With no highlights on the page (and no open overlays), the focus hit map is exactly the nav
        // thirds — no chrome buttons.
        assert!(out.hits.iter().all(|(_, t)| matches!(t, ReaderTarget::PrevPage | ReaderTarget::NextPage | ReaderTarget::ToggleFocus)), "focus hits are only the tap thirds: {:?}", out.hits.iter().map(|(_, t)| t).collect::<Vec<_>>());
    }

    #[test]
    fn focus_mode_exposes_highlight_hits_and_draws_the_menu_pill() {
        // The focus-mode long-press bug: the old focus early-return skipped highlight_spans, the pill,
        // and the note sheet — a long-press found no TapHighlight target and a pen-scrub-opened menu
        // was invisible and undismissable. All three overlays are shared across the modes now.
        let src = "The ==quick brown== fox. ".repeat(60);
        let mut model = reading_model(&src, 0, true);
        let out = view(&model, &fonts(), 954, 1696);
        assert!(
            out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })),
            "highlights are long-pressable in focus"
        );
        // Open the menu on the first highlight: the pill draws with its three targets, in focus.
        let range = out
            .hits
            .iter()
            .find_map(|(_, t)| match t {
                ReaderTarget::TapHighlight { range } => Some(range.clone()),
                _ => None,
            })
            .unwrap();
        if let Screen::Reading(r) = &mut model.screen {
            r.highlight_menu = Some(crate::model::HighlightMenu { range: range.clone(), has_note: false, x: 477, y: 400 });
        }
        let out = view(&model, &fonts(), 954, 1696);
        for t in [ReaderTarget::HighlightAddNote, ReaderTarget::HighlightDelete] {
            assert!(out.hits.iter().any(|(_, tt)| *tt == t), "{t:?} tappable in focus");
        }
        assert!(
            out.hits.iter().any(|(_, t)| *t == ReaderTarget::HighlightMenuDismiss),
            "an open menu is dismissable in focus"
        );
        // And the note editor opens in focus too.
        if let Screen::Reading(r) = &mut model.screen {
            r.highlight_menu = None;
            r.note = Some(NoteEntry { buf: String::new(), anchor: range.start, layer: render::ui::KbLayer::Letters, shift: false, had_note: false });
        }
        let out = view(&model, &fonts(), 954, 1696);
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::NoteSave), "note sheet usable in focus");
    }

    #[test]
    fn page_minutes_matches_per_page_recomputation() {
        // The precompute must equal what the chrome used to compute per redraw: reading_minutes of
        // the source from each page's first byte (0 for a page with no text runs).
        let src = "word ".repeat(4000);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        assert!(pages.len() > 2);
        let mins = page_minutes(&src, &pages);
        assert_eq!(mins.len(), pages.len());
        for (i, page) in pages.iter().enumerate() {
            let top = page.runs().map(|g| g.src.start).min().unwrap_or(0);
            assert_eq!(mins[i], reading_minutes(src.get(top..).unwrap_or("")), "page {i}");
        }
        assert!(mins[0] >= mins[pages.len() - 1], "remaining time never grows with progress");
        assert!(mins[0] > 0);
    }

    #[test]
    fn the_select_bar_holds_five_controls_without_overlapping() {
        // Library: the action chips share the footer with the pager, so the bar carries all five.
        let mut model = ReaderModel::new(vault(40));
        model.selected = Some(vec![model.visible()[0]]);
        let out = view(&model, &fonts(), 954, 1696);
        let bar: Vec<&(Rect, ReaderTarget)> = out
            .hits
            .iter()
            .filter(|(r, _)| r.y >= 1696 - FOOTER_H)
            .collect();
        for want in [
            ReaderTarget::SelectAll,
            ReaderTarget::ArchiveSelected,
            ReaderTarget::ReadSelected,
            ReaderTarget::FavSelected,
            ReaderTarget::ListNext,
        ] {
            assert!(bar.iter().any(|(_, t)| *t == want), "{want:?} missing from the select bar");
        }
        for (r, t) in &bar {
            assert!(r.h >= MIN_TAP_PX, "select bar {t:?} too short: {r:?}");
            assert!(r.x >= MARGIN && r.x + r.w <= 954 - MARGIN, "select bar {t:?} off the page: {r:?}");
        }
        for (i, (a, ta)) in bar.iter().enumerate() {
            for (b, tb) in bar.iter().skip(i + 1) {
                assert!(a.x + a.w <= b.x || b.x + b.w <= a.x, "{ta:?} and {tb:?} overlap: {a:?} / {b:?}");
            }
        }
        // Cancel stays in the header; Archive left it for the bar.
        assert!(out.hits.iter().any(|(r, t)| *t == ReaderTarget::CancelSelect && r.y < HEADER_H));
        assert!(!out.hits.iter().any(|(r, t)| *t == ReaderTarget::ArchiveSelected && r.y < HEADER_H));
        // The nav icons stand down while selecting — they'd be dead ends mid-batch.
        assert!(!out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::OpenShelf | ReaderTarget::OpenSearch | ReaderTarget::Exit)));

        // Nothing ticked: All stays live, the three actions are drawn but inert.
        model.selected = Some(Vec::new());
        let idle = view(&model, &fonts(), 954, 1696);
        assert!(idle.hits.iter().any(|(_, t)| *t == ReaderTarget::SelectAll));
        for dead in [ReaderTarget::ArchiveSelected, ReaderTarget::ReadSelected, ReaderTarget::FavSelected] {
            assert!(!idle.hits.iter().any(|(_, t)| *t == dead), "{dead:?} live with nothing ticked");
        }
    }

    #[test]
    fn interactive_targets_are_finger_sized() {
        let model = ReaderModel::new(vault(40));
        let out = view(&model, &fonts(), 954, 1696);
        for (r, t) in &out.hits {
            // Body page-turn thirds are intentionally large; nav chips must clear the min.
            if matches!(t, ReaderTarget::PrevPage | ReaderTarget::NextPage) {
                continue;
            }
            assert!(r.h >= MIN_TAP_PX, "{t:?} too short: {r:?}");
        }
        // The filters live in the library shelf now; opening it exposes the four status filters + a
        // sort toggle, every shelf control finger-sized.
        let mut open = ReaderModel::new(vault(40));
        open.shelf_open = true;
        let out = view(&open, &fonts(), 954, 1696);
        for (r, t) in &out.hits {
            if matches!(t, ReaderTarget::PrevPage | ReaderTarget::NextPage) {
                continue;
            }
            assert!(r.h >= MIN_TAP_PX, "shelf {t:?} too short: {r:?}");
        }
        assert!(out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::SetFilter(_))));
        assert!(out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::SetSort(_))));
    }

    #[test]
    fn shelf_fits_the_canvas_with_authors_and_sort_chips() {
        // The view fixture gives every clipping a distinct byline, so the AUTHORS section renders.
        let mut model = ReaderModel::new(vault(40));
        model.shelf_open = true;
        let out = view(&model, &fonts(), 954, 1696);
        // All three sorts are one explicit tap (chips), and the author section is present.
        for k in SortKey::ALL {
            let hit = out.hits.iter().find(|(_, t)| *t == ReaderTarget::SetSort(k)).expect("sort chip");
            assert!(hit.0.h >= MIN_TAP_PX, "sort chip {k:?} finger-sized");
        }
        let authors: Vec<_> = out.hits.iter().filter(|(_, t)| matches!(t, ReaderTarget::SetAuthorFilter(_))).collect();
        assert_eq!(authors.len(), 2, "two inline author rows");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::OpenAuthors), "All authors… row");
        // The whole shelf — down to the sort chips — stays inside the canvas.
        for (r, t) in &out.hits {
            if matches!(t, ReaderTarget::SetSort(_) | ReaderTarget::SetAuthorFilter(_) | ReaderTarget::OpenAuthors) {
                assert!(r.y >= 0 && r.y + r.h <= 1696, "{t:?} off-canvas: {r:?}");
            }
        }
    }

    #[test]
    fn author_picker_screen_lists_pages_and_clears() {
        let mut model = ReaderModel::new(vault(40)); // 40 distinct bylines → paginated
        model.author_filter = Some("Source 1".into()); // sorts near the top → on the first page
        model.screen = Screen::Authors { scroll: 0 };
        let out = view(&model, &fonts(), 954, 1696);
        // Row 0 clears; the active author row toggles off; every row finger-sized and on-canvas.
        let rows: Vec<_> = out.hits.iter().filter(|(_, t)| matches!(t, ReaderTarget::SetAuthorFilter(_))).collect();
        assert!(rows.len() > 2, "author rows present");
        assert!(rows.iter().filter(|(_, t)| *t == ReaderTarget::SetAuthorFilter(None)).count() >= 2, "the All-authors row and the active row both clear");
        for (r, t) in &rows {
            assert!(r.h >= MIN_TAP_PX && r.y >= 0 && r.y + r.h <= 1696, "{t:?} bad rect {r:?}");
        }
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::CloseAuthors), "close control");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::AuthorsNext), "41 rows page");
        assert!(!out.hits.iter().any(|(_, t)| *t == ReaderTarget::AuthorsPrev), "no prev on page 1");
    }

    #[test]
    fn reading_view_exposes_size_and_a_three_way_status_picker() {
        let src = "# Title\n\n".to_string() + &"word ".repeat(300);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src, title: "Title".into(), doc, pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        // Aa−/Aa+ and the status picker now live in the ⋯ More sheet — neither shows while it's closed.
        let out = view(&model, &fonts(), 954, 1696);
        assert!(
            !out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::TextSmaller | ReaderTarget::TextLarger)),
            "no size controls until the More sheet opens"
        );
        // Open the sheet: the two size controls + all three status states are directly tappable (a
        // picker, not a cycle) for the open file, each finger-sized.
        if let Screen::Reading(r) = &mut model.screen {
            r.more_open = true;
        }
        let out = view(&model, &fonts(), 954, 1696);
        for t in [ReaderTarget::TextSmaller, ReaderTarget::TextLarger] {
            let hit = out.hits.iter().find(|(_, tt)| *tt == t).unwrap_or_else(|| panic!("{t:?} present"));
            assert!(hit.0.h >= MIN_TAP_PX, "{t:?} finger-sized: {:?}", hit.0);
        }
        let segs: Vec<_> =
            out.hits.iter().filter(|(r, t)| matches!(t, ReaderTarget::SetStatusFor(0, _)) && r.h >= MIN_TAP_PX).collect();
        assert_eq!(segs.len(), 3, "three finger-sized status rows for the open file");
    }

    #[test]
    fn shelf_has_finger_sized_filter_rows() {
        let mut model = ReaderModel::new(vault(40));
        model.shelf_open = true;
        let out = view(&model, &fonts(), 954, 1696);
        // Collections + topics: five content tags + the Archived + Favourites rows = seven SetTagFilter
        // rows, each a finger-sized shelf row (status filters + sort are separate targets).
        let tag_pills: Vec<_> =
            out.hits.iter().filter(|(r, t)| matches!(t, ReaderTarget::SetTagFilter(_)) && r.h >= MIN_TAP_PX).collect();
        assert_eq!(tag_pills.len(), 7, "five topics + Archived + Favourites, each finger-sized");
        for tg in Tag::CONTENT {
            assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::SetTagFilter(Some(tg))), "{tg:?} topic present");
        }
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::SetTagFilter(Some(Tag::Favourite))), "Favourites row");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::SetTagFilter(Some(Tag::Archive))), "Archived row");
        // The four reading-status filters are present and finger-sized too.
        let status_rows: Vec<_> =
            out.hits.iter().filter(|(r, t)| matches!(t, ReaderTarget::SetFilter(_)) && r.h >= MIN_TAP_PX).collect();
        assert_eq!(status_rows.len(), 4, "All / Unread / Reading / Read");
    }

    #[test]
    fn reading_toolbar_more_button_opens_a_finger_sized_more_sheet() {
        let src = "# Title\n\n".to_string() + &"word ".repeat(300);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(3));
        model.vault[0].tags = vec![Tag::Ai];
        // Closed: the toolbar offers a ⋯ More button; no sheet targets yet.
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc: parse(&src), pages: pages.clone(), page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        let more = out.hits.iter().find(|(_, t)| *t == ReaderTarget::OpenMore).expect("More button present");
        assert!(more.0.h >= MIN_TAP_PX, "More button finger-sized");
        // The one-tap Favourite + Archive toolbar toggles are always present; but no *content-tag*
        // toggles nor a status picker until the sheet opens.
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::ToggleTag(Tag::Favourite)), "toolbar Favourite");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::ToggleTag(Tag::Archive)), "toolbar Archive");
        assert!(
            !out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::ToggleTag(t2) if Tag::CONTENT.contains(t2))),
            "no content-tag toggles while the sheet is closed"
        );
        assert!(!out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::SetStatusFor(..))), "no status picker while closed");
        // Open: a status row per state + a toggle row per content tag (each finger-sized) + a close target.
        if let Screen::Reading(r) = &mut model.screen {
            r.more_open = true;
        }
        let out = view(&model, &fonts(), 954, 1696);
        for tg in Tag::CONTENT {
            let row = out.hits.iter().find(|(_, t)| *t == ReaderTarget::ToggleTag(tg)).unwrap_or_else(|| panic!("{tg:?} row"));
            assert!(row.0.h >= MIN_TAP_PX, "{tg:?} row finger-sized");
        }
        let status: Vec<_> = out.hits.iter().filter(|(rr, t)| matches!(t, ReaderTarget::SetStatusFor(0, _)) && rr.h >= MIN_TAP_PX).collect();
        assert_eq!(status.len(), 3, "three finger-sized status rows in the More sheet");
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::CloseMore), "a close target (Close/scrim)");
    }

    #[test]
    fn reading_contents_panel_lists_headings_and_offers_jump_targets() {
        // A doc with two headings; the body between pushes the second onto a later page.
        let src = "# Alpha\n\n".to_string() + &"word ".repeat(400) + "\n\n# Beta\n\n" + &"word ".repeat(50);
        let doc = parse(&src);
        let pages = layout(&doc, &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(1));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Alpha".into(), doc: parse(&src), pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: true, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        // Both headings appear as finger-sized JumpToHeading rows, plus a close target.
        let jumps: Vec<_> = out.hits.iter().filter(|(rr, t)| matches!(t, ReaderTarget::JumpToHeading(_)) && rr.h >= MIN_TAP_PX).collect();
        assert!(jumps.len() >= 2, "both headings listed, got {}", jumps.len());
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::CloseContents), "a close target");
    }

    #[test]
    fn the_reading_bar_has_no_undo() {
        let src = "# Title\n\n".to_string() + &"word ".repeat(300);
        let pages = layout(&parse(&src), &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(1));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc: parse(&src), pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        // The 5-slot reading bar carries Back / Fav / Archive / Contents / More — and no Undo anywhere.
        let out = view(&model, &fonts(), 954, 1696);
        for t in [ReaderTarget::Back, ReaderTarget::ToggleTag(Tag::Favourite), ReaderTarget::ToggleTag(Tag::Archive), ReaderTarget::OpenContents, ReaderTarget::OpenMore] {
            assert!(out.hits.iter().any(|(_, tt)| *tt == t), "{t:?} in the reading bar");
        }
    }

    #[test]
    fn tapping_a_highlight_opens_a_note_delete_menu() {
        let src = "The ==quick brown== fox jumps over it.".to_string();
        let pages = layout(&parse(&src), &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(1));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc: parse(&src), pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        // The highlighted span is tappable, carrying its inner byte range.
        let inner = src.find("quick").unwrap()..src.find("== fox").unwrap();
        let out = view(&model, &fonts(), 954, 1696);
        assert!(out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::TapHighlight { range } if *range == inner)), "highlight span is a tap target");
        // With the menu open, the pill offers Note + Delete + a dismiss target, each finger-sized.
        if let Screen::Reading(r) = &mut model.screen {
            r.highlight_menu = Some(crate::model::HighlightMenu { range: inner.clone(), has_note: false, x: 300, y: 600 });
        }
        let out = view(&model, &fonts(), 954, 1696);
        for t in [ReaderTarget::HighlightAddNote, ReaderTarget::HighlightDelete, ReaderTarget::HighlightMenuDismiss] {
            let hit = out.hits.iter().find(|(_, tt)| *tt == t).unwrap_or_else(|| panic!("{t:?} present"));
            assert!(hit.0.h >= MIN_TAP_PX, "{t:?} finger-sized");
        }
    }

    #[test]
    fn a_noted_highlight_shows_an_indicator_glyph() {
        // Just a smoke test that rendering a noted highlight doesn't panic and the span stays tappable.
        let src = "The ==quick brown==[^1] fox jumps.\n\n[^1]: a note\n".to_string();
        let pages = layout(&parse(&src), &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(1));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc: parse(&src), pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        assert!(out.hits.iter().any(|(_, t)| matches!(t, ReaderTarget::TapHighlight { .. })), "noted highlight still tappable");
    }

    #[test]
    fn favourite_has_a_one_tap_reading_toggle_and_a_list_filter() {
        // Reading view: a finger-sized "Fav" toggle in the header that writes the favourite tag.
        let src = "# Title\n\nbody".to_string();
        let pages = layout(&parse(&src), &fonts(), LayoutOpts::default());
        let mut model = ReaderModel::new(vault(3));
        model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&src, &pages), file_ix: 0, source: src.clone(), title: "Title".into(), doc: parse(&src), pages, page_ix: 0, selection: None, pen_start: None, pen_dwelled: false, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
        let out = view(&model, &fonts(), 954, 1696);
        let fav = out.hits.iter().find(|(_, t)| *t == ReaderTarget::ToggleTag(Tag::Favourite)).expect("Favourite toggle in the reading header");
        assert!(fav.0.h >= MIN_TAP_PX, "Favourite toggle finger-sized");

        // List view: the shelf's Favourites row sets the favourite filter; when active it clears it.
        let mut m = ReaderModel::new(vault(20));
        m.shelf_open = true;
        let out = view(&m, &fonts(), 954, 1696);
        let f = out.hits.iter().find(|(r, t)| *t == ReaderTarget::SetTagFilter(Some(Tag::Favourite)) && r.h >= MIN_TAP_PX);
        assert!(f.is_some(), "a finger-sized Favourites row in the shelf");
        let mut active = ReaderModel::new(vault(20));
        active.shelf_open = true;
        active.tag_filter = Some(Tag::Favourite);
        let out = view(&active, &fonts(), 954, 1696);
        assert!(out.hits.iter().any(|(_, t)| *t == ReaderTarget::SetTagFilter(None)), "the active Favourites row clears the filter");
    }

    #[test]
    fn reading_minutes_estimates_at_300_wpm() {
        assert_eq!(reading_minutes(""), 0, "nothing left");
        assert_eq!(reading_minutes("!!! ... ,,,"), 0, "punctuation + spaces don't count");
        assert_eq!(reading_minutes(&"a".repeat(1500)), 1, "≈1500 alphanumeric chars ≈ 1 min");
        assert_eq!(reading_minutes(&"a".repeat(1501)), 2, "rounds up");
    }
}
