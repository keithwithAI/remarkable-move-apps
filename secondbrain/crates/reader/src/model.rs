//! The reader app's Elm-style state: a pure [`ReaderModel`] updated by [`crate::update`] and
//! drawn by [`crate::view`]. All I/O (reading files, writing highlights, git sync, the
//! framebuffer, pen/touch input) lives in the `reader-app` binary.

use crate::layout::Page;
use crate::parse::Doc;
use render::ui::KbLayer;
use std::ops::Range;
use std::path::PathBuf;

/// A clipping's reading status, from its `status:` front matter (absent = [`ReadStatus::NotRead`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadStatus {
    #[default]
    NotRead,
    Reading,
    Read,
}

impl ReadStatus {
    /// Interpret a front-matter `status:` value (unknown/empty → NotRead).
    pub fn from_meta(s: Option<&str>) -> Self {
        match s.map(|x| x.trim().to_lowercase()).as_deref() {
            Some("reading") => ReadStatus::Reading,
            Some("read") => ReadStatus::Read,
            _ => ReadStatus::NotRead,
        }
    }
    /// The value to write into the front matter, or `None` to remove the field (NotRead).
    pub fn as_frontmatter(self) -> Option<&'static str> {
        match self {
            ReadStatus::NotRead => None,
            ReadStatus::Reading => Some("reading"),
            ReadStatus::Read => Some("read"),
        }
    }
    /// The next state when the user taps the cycling status chip.
    pub fn next(self) -> Self {
        match self {
            ReadStatus::NotRead => ReadStatus::Reading,
            ReadStatus::Reading => ReadStatus::Read,
            ReadStatus::Read => ReadStatus::NotRead,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ReadStatus::NotRead => "Not read",
            ReadStatus::Reading => "Reading",
            ReadStatus::Read => "Read",
        }
    }
}

/// The library list's status filter (the tabs across the top).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusFilter {
    #[default]
    All,
    NotRead,
    Reading,
    Read,
}

impl StatusFilter {
    /// The four tabs in display order.
    pub const TABS: [StatusFilter; 4] =
        [StatusFilter::All, StatusFilter::NotRead, StatusFilter::Reading, StatusFilter::Read];

    pub fn matches(self, s: ReadStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::NotRead => s == ReadStatus::NotRead,
            StatusFilter::Reading => s == ReadStatus::Reading,
            StatusFilter::Read => s == ReadStatus::Read,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::NotRead => "Not read",
            StatusFilter::Reading => "Reading",
            StatusFilter::Read => "Read",
        }
    }
}

/// How the library list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Title,
    /// Newest `published:` date first.
    Recent,
    /// Newest `created:` (capture date) first — "what just arrived in the vault". Falls back to
    /// `published:` for clippings from before the Web Clipper template stamped a capture date.
    Added,
}

impl SortKey {
    /// Every sort, in shelf chip order.
    pub const ALL: [SortKey; 3] = [SortKey::Title, SortKey::Recent, SortKey::Added];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Title => "A–Z",
            SortKey::Recent => "Published",
            SortKey::Added => "Added",
        }
    }
}

/// A managed tag the reader can assign to a clipping. Stored in the note's YAML `tags:` as an
/// Obsidian-native lowercase slug, so it stays clickable/filterable in Obsidian too. Any tag outside
/// this set (authored in Obsidian) is preserved on write and ignored by the list filter.
///
/// [`Tag::Archive`] and [`Tag::Favourite`] are special (not content topics): each has a one-tap
/// toggle in the reading view and its own library filter, rather than a panel checkbox. Archiving
/// *hides* a clipping from the list (reachable only via the "Archived" view); favouriting does not
/// hide it — it just marks it, surfaced by the "Fav" filter. The middle five are content tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Ai,
    Health,
    Life,
    Work,
    Others,
    Archive,
    Favourite,
}

impl Tag {
    /// Every managed tag. Preserved on write and used by [`from_slug`](Tag::from_slug); the reading
    /// panel only toggles [`CONTENT`](Tag::CONTENT), while Archive/Favourite have their own controls.
    pub const ALL: [Tag; 7] = [Tag::Ai, Tag::Health, Tag::Life, Tag::Work, Tag::Others, Tag::Archive, Tag::Favourite];
    /// The content tags shown as library filter pills + reading-panel checkboxes — everything except
    /// the special [`Tag::Archive`] / [`Tag::Favourite`] (which have their own toggles).
    pub const CONTENT: [Tag; 5] = [Tag::Ai, Tag::Health, Tag::Life, Tag::Work, Tag::Others];

    /// Human label for a pill / toggle row.
    pub fn label(self) -> &'static str {
        match self {
            Tag::Ai => "AI",
            Tag::Health => "Health",
            Tag::Life => "Life",
            Tag::Work => "Work",
            Tag::Others => "Others",
            Tag::Archive => "Archive",
            Tag::Favourite => "Favourite",
        }
    }
    /// The lowercase YAML value written into `tags:` (Obsidian-native).
    pub fn slug(self) -> &'static str {
        match self {
            Tag::Ai => "ai",
            Tag::Health => "health",
            Tag::Life => "life",
            Tag::Work => "work",
            Tag::Others => "others",
            Tag::Archive => "archive",
            Tag::Favourite => "favourite",
        }
    }
    /// Map a front-matter tag value back to a managed [`Tag`] (case-insensitive); `None` for any
    /// unmanaged Obsidian tag.
    pub fn from_slug(s: &str) -> Option<Tag> {
        let s = s.trim().to_lowercase();
        Tag::ALL.into_iter().find(|t| t.slug() == s)
    }
}

/// What kind of document a vault file is. Articles are the Web-Clipper clippings at the vault root
/// (the original library — highlightable, status/tag-managed). Books are Readwise Kindle-highlight
/// exports from the [`crate::vault::BOOKS_SUBDIR`] subfolder: **read-only** on device (Readwise may
/// regenerate the files, so the reader never writes into them — no pen highlights, no status/tags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocKind {
    #[default]
    Article,
    Book,
}

/// A `.md` file in the vault list.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultFile {
    pub path: PathBuf,
    /// Article (vault root) or read-only Book (the Kindle Highlights subfolder).
    pub kind: DocKind,
    pub title: String,
    /// Whether the file already contains `==highlights==` (shown as a dot). Best-effort.
    pub has_highlights: bool,
    /// `published:` front-matter date (ISO `YYYY-MM-DD` in ~all clippings) — the "Published" sort key.
    pub published: Option<String>,
    /// `created:` front-matter (when the clipping was captured — see [`crate::parse::Meta::created`]).
    /// The "Recently added" sort key; ISO-ish strings compare lexically.
    pub created: Option<String>,
    /// Display "byline" for the list row — publisher (from the web link) else author, derived once at
    /// scan time via [`crate::byline::derive_byline`]. `None` when nothing usable; also matched by search.
    pub byline: Option<String>,
    /// `title.to_lowercase()`, precomputed at scan: the sort comparator and the search filter used
    /// to lowercase inside every comparison (~3,000 String allocs per keystroke on a full vault).
    pub title_lower: String,
    /// `byline.to_lowercase()`, precomputed for the search filter (see `title_lower`).
    pub byline_lower: Option<String>,
    /// Reading status from the `status:` front matter (absent = NotRead).
    pub status: ReadStatus,
    /// Managed tags parsed from the `tags:` front matter (unmanaged tags are ignored here).
    pub tags: Vec<Tag>,
    /// Rough reading-time in minutes, estimated from the file's byte size at scan (the full text isn't
    /// read during a scan). A list-row glance only — the reading view still shows exact "N min left".
    pub minutes: u32,
}

/// A pen selection in progress: byte offsets into the open file's source. `anchor` is where the
/// pen went down, `caret` follows it; `range()` normalises them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub caret: usize,
}

impl Selection {
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.caret)..self.anchor.max(self.caret)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.caret
    }
}

/// A note being typed for a highlight (stored as an Obsidian inline footnote `==text==[^n]` +
/// `[^n]: note`). `anchor` is a byte offset inside the target highlight's inner text, so the runtime
/// can find its closing `==` to splice the reference after. The on-screen keyboard mirrors the
/// library search keyboard ([`render::ui::draw_keyboard`]).
#[derive(Debug, Clone)]
pub struct NoteEntry {
    pub buf: String,
    pub anchor: usize,
    pub layer: KbLayer,
    pub shift: bool,
    /// Whether the highlight already had a note when this editor opened. Lets Save distinguish
    /// "cleared an existing note" (empty text → write, dropping the footnote) from "a brand-new note
    /// left blank" (no write). Set from the tapped highlight's `has_note` when the editor opens.
    pub had_note: bool,
}

/// The open document, paginated, with the reading position and any in-flight pen selection.
pub struct Reading {
    pub file_ix: usize,
    /// The raw `.md` bytes — the source of truth we rewrite when a highlight is made.
    pub source: String,
    pub doc: Doc,
    pub pages: Vec<Page>,
    pub page_ix: usize,
    pub selection: Option<Selection>,
    /// Where the pen last went down (screen px). Lets pen-up tell a *tap* (barely moved → activates
    /// the UI button under it, like a touch) from a *drag* (→ highlight). `None` between strokes.
    pub pen_start: Option<(i32, i32)>,
    /// Set when a stylus dwell (LongPress) fired mid-stroke over plain text: the stroke stays alive
    /// so the drag can still highlight, but a still lift must then no-op (not become a tap → page
    /// turn). Reset on the next pen-down.
    pub pen_dwelled: bool,
    /// "N min left" per page, precomputed by [`crate::view::page_minutes`] whenever `source`/`pages`
    /// install — the view reads `page_minutes[page_ix]` instead of rescanning the remaining source
    /// (a full-document char count) on every redraw.
    pub page_minutes: Vec<u32>,
    pub title: String,
    /// Whether the "More" sheet (status + tags) is open over this document.
    pub more_open: bool,
    /// Whether the Contents (table-of-contents) panel is open over this document.
    pub contents_open: bool,
    /// Focus (full-screen) mode: hide all header/footer chrome. Toggled by a centre tap; page turns
    /// stay on the edge swipes.
    pub focus: bool,
    /// Which focus sub-state the view is in. `false` = the "entry overlay": pagination is untouched
    /// (nothing moved when focus toggled) and the freed chrome bars show drawn preview lines from
    /// the adjacent pages. `true` = focus's own full-screen pagination, entered on the first
    /// in-focus page turn (the turn refreshes the whole panel anyway): pages are continuous — the
    /// next page starts exactly after the last line the entry screen showed. Reset by toggling.
    pub focus_paginated: bool,
    /// A note being typed for a highlight (opened from the tap-a-highlight menu). `Some` = sheet open.
    pub note: Option<NoteEntry>,
    /// The floating action menu shown when an existing highlight is tapped (`Some` = open). See
    /// [`HighlightMenu`]. Deleting or adding a note is done from here; a tap outside dismisses it.
    pub highlight_menu: Option<HighlightMenu>,
}

/// The small floating toolbar shown when a highlight is tapped (Kindle-style): Note + Delete. Carries
/// the tapped highlight's inner byte `range` (for the delete/note write), whether it already has a note
/// (`has_note`, so the Note action reads "Edit note"), and the tap `(x, y)` so the view can float the
/// pill just above the tapped span.
#[derive(Debug, Clone)]
pub struct HighlightMenu {
    pub range: Range<usize>,
    pub has_note: bool,
    pub x: i32,
    pub y: i32,
}

impl Reading {
    pub fn page(&self) -> Option<&Page> {
        self.pages.get(self.page_ix)
    }
}

/// Which screen is showing.
pub enum Screen {
    /// The vault browser. `scroll` is the index of the first visible row.
    List { scroll: usize },
    Reading(Reading),
    /// On-screen-keyboard search over the library. `scroll` pages the live results; the query text
    /// itself lives on [`ReaderModel::query`] (a peer of the other filters). `kb` is whether the
    /// keyboard is up: typing shows it; the keyboard's "Search" commits to a full-screen results
    /// view (`kb: false`) where tapping the query bar brings it back.
    Search { scroll: usize, kb: bool },
    /// Full-screen author picker (from the shelf's "All authors…" row): every distinct byline with
    /// its count, paginated. Tapping one applies the author filter and returns to the list.
    Authors { scroll: usize },
}

/// A transient one-line notice ("Highlighted", "Synced"), counted down by `Tick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub ticks_left: u8,
}

pub struct ReaderModel {
    pub vault: Vec<VaultFile>,
    pub screen: Screen,
    pub flash: Option<Flash>,
    /// A file open / highlight write is in flight (the runtime is doing I/O).
    pub busy: bool,
    /// Library list controls. Kept on the model (not inside `Screen::List`) so they survive the
    /// open→Back excursion; the runtime seeds + persists them via the prefs file.
    pub filter: StatusFilter,
    pub sort: SortKey,
    /// List tag filter: `None` shows every tag, `Some(t)` narrows to clippings carrying `t`. ANDs
    /// with `filter`. Reading-view tag assignment is separate; this only filters the list.
    pub tag_filter: Option<Tag>,
    /// List author filter: `Some(byline)` narrows to clippings whose byline matches exactly (the
    /// display string — bylines are derived consistently at scan, so exact match is safe). ANDs
    /// with the status/tag filters; ignored in the Books collection. Persisted via prefs.
    pub author_filter: Option<String>,
    /// Library search text: a case-insensitive substring AND'd into `visible()` over title + byline.
    /// Empty = no-op (so the List screen is unaffected); only `Screen::Search` grows it. Transient —
    /// cleared on close, never persisted to prefs.
    pub query: String,
    /// The "Books" shelf collection is active: the list shows the Kindle-highlight books instead of
    /// the article clippings. Books ignore the status/tag filters (they're read-only and untagged);
    /// search and sort still apply. Persisted via prefs like the other filters.
    pub books: bool,
    /// The library shelf (the slide-in filter panel) is open. Transient — like a modal, and never
    /// persisted to prefs.
    pub shelf_open: bool,
    /// Search on-screen-keyboard state — which layer (letters/symbols) is showing + one-shot Shift.
    /// Transient, reset when search opens. The keyboard is the shared [`render::ui::draw_keyboard`],
    /// identical to the Agenda's quick-add keyboard.
    pub search_layer: KbLayer,
    pub search_shift: bool,
    /// Multi-select over the library list or the committed search results (`Some` = select mode;
    /// the vault indices ticked so far). Row taps toggle membership; Archive applies to all.
    /// Transient — cleared on every filter/search-mode change and vault rescan, never persisted.
    pub selected: Option<Vec<usize>>,
}

/// Per-view clipping counts shown next to each library-shelf filter row.
pub struct LibraryCounts {
    pub all: usize,
    pub unread: usize,
    pub reading: usize,
    pub read: usize,
    pub favourites: usize,
    pub archived: usize,
    /// Kindle-highlight books (the read-only Books collection).
    pub books: usize,
    /// Content tags in [`Tag::CONTENT`] order, each with its non-archived count.
    pub tags: Vec<(Tag, usize)>,
}

impl ReaderModel {
    pub fn new(vault: Vec<VaultFile>) -> Self {
        ReaderModel {
            vault,
            screen: Screen::List { scroll: 0 },
            flash: None,
            busy: false,
            filter: StatusFilter::All,
            sort: SortKey::Title,
            tag_filter: None,
            author_filter: None,
            query: String::new(),
            books: false,
            shelf_open: false,
            search_layer: KbLayer::Letters,
            search_shift: false,
            selected: None,
        }
    }

    pub fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash { text: text.into(), ticks_left: FLASH_TICKS });
    }

    /// Vault indices to show in the list: the status filter AND the tag filter applied, then ordered
    /// by the current sort (title, or `published` newest-first with missing dates last). Archived
    /// clippings are hidden from every view except the dedicated "Archived" one (`tag_filter ==
    /// Some(Tag::Archive)`), the "Favourites" one, and the "Read" status view — favourites are a
    /// curated collection and Read is the reading *history*, so archiving (the usual tidy-up after
    /// finishing an article) drops it from the main list without erasing it from either.
    pub fn visible(&self) -> Vec<usize> {
        let archived = |i: usize| self.vault[i].tags.contains(&Tag::Archive);
        let q = self.query.trim().to_lowercase();
        // The Books collection is its own place: only books, with the status/tag filters ignored
        // (books are read-only — no status, no tags). Every other view shows only articles.
        let want_kind = if self.books { DocKind::Book } else { DocKind::Article };
        let mut ix: Vec<usize> = (0..self.vault.len())
            .filter(|&i| self.vault[i].kind == want_kind)
            .filter(|&i| self.books || self.filter.matches(self.vault[i].status))
            .filter(|&i| {
                self.books
                    || match self.tag_filter {
                        Some(Tag::Archive) => archived(i),
                        Some(Tag::Favourite) => self.vault[i].tags.contains(&Tag::Favourite),
                        Some(t) => self.vault[i].tags.contains(&t) && !archived(i),
                        None => !archived(i) || self.filter == StatusFilter::Read,
                    }
            })
            .filter(|&i| {
                self.books
                    || match &self.author_filter {
                        Some(a) => self.vault[i].byline.as_deref() == Some(a.as_str()),
                        None => true,
                    }
            })
            .filter(|&i| q.is_empty() || title_or_byline_matches(&self.vault[i], &q))
            .collect();
        let title = |i: usize| &self.vault[i].title_lower;
        match self.sort {
            SortKey::Title => ix.sort_by(|&a, &b| title(a).cmp(&title(b))),
            SortKey::Recent => ix.sort_by(|&a, &b| {
                let key = |i: usize| self.vault[i].published.as_deref().unwrap_or("");
                key(b).cmp(key(a)).then_with(|| title(a).cmp(&title(b)))
            }),
            SortKey::Added => ix.sort_by(|&a, &b| {
                // Capture date, falling back to published for pre-template clippings — so the old
                // library keeps a sensible order until the clipper backfills `created:`.
                let key = |i: usize| {
                    let f = &self.vault[i];
                    f.created.as_deref().or(f.published.as_deref()).unwrap_or("")
                };
                key(b).cmp(key(a)).then_with(|| title(a).cmp(&title(b)))
            }),
        }
        ix
    }

    /// Tally clippings per shelf filter. All/Unread/Reading + content tags are over the
    /// *non-archived* set (what those views show); `read` and `favourites` count across the whole
    /// vault and `archived` is the hidden set — matching [`visible`](Self::visible), where the Read
    /// and Favourites views keep archived clippings. Cheap — only computed when the shelf is open.
    pub fn library_counts(&self) -> LibraryCounts {
        // Every status/tag tally is over the ARTICLES; books live only in their own count.
        let articles: Vec<&VaultFile> = self.vault.iter().filter(|f| f.kind == DocKind::Article).collect();
        let live: Vec<&&VaultFile> = articles.iter().filter(|f| !f.tags.contains(&Tag::Archive)).collect();
        LibraryCounts {
            all: live.len(),
            unread: live.iter().filter(|f| f.status == ReadStatus::NotRead).count(),
            reading: live.iter().filter(|f| f.status == ReadStatus::Reading).count(),
            read: articles.iter().filter(|f| f.status == ReadStatus::Read).count(),
            favourites: articles.iter().filter(|f| f.tags.contains(&Tag::Favourite)).count(),
            archived: articles.iter().filter(|f| f.tags.contains(&Tag::Archive)).count(),
            books: self.vault.iter().filter(|f| f.kind == DocKind::Book).count(),
            tags: Tag::CONTENT.iter().map(|&t| (t, live.iter().filter(|f| f.tags.contains(&t)).count())).collect(),
        }
    }

    /// Every distinct article byline with its non-archived clipping count, most-clipped first
    /// (ties A–Z, case-insensitive). Feeds the shelf's AUTHORS section (top few) and the
    /// full-screen author picker (all of them). Books are excluded — the author filter is an
    /// article-view narrowing, like the topic tags.
    pub fn authors(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for f in &self.vault {
            if f.kind != DocKind::Article || f.tags.contains(&Tag::Archive) {
                continue;
            }
            let Some(b) = f.byline.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            match counts.iter_mut().find(|(name, _)| name == b) {
                Some((_, n)) => *n += 1,
                None => counts.push((b.to_string(), 1)),
            }
        }
        counts.sort_by(|(an, ac), (bn, bc)| bc.cmp(ac).then_with(|| an.to_lowercase().cmp(&bn.to_lowercase())));
        counts
    }

    /// The vault indices on the page currently drawn — [`visible`](Self::visible) sliced to the rows
    /// between `scroll` and `scroll + per`. What the select bar's "All" ticks: exactly the rows you
    /// can see, so ticking never reaches past the fold.
    pub fn page_indices(&self, scroll: usize, per: usize) -> Vec<usize> {
        self.visible().into_iter().skip(scroll).take(per).collect()
    }

    /// Every ticked clipping is already Read (false when nothing is ticked) — so the select bar's
    /// Read chip can flip to "Unread" and the update step can act on the same fact.
    pub fn selection_all_read(&self) -> bool {
        self.selection_all(|f| f.status == ReadStatus::Read)
    }

    /// Every ticked clipping is already a favourite (false when nothing is ticked) — the Fav chip's
    /// "Unfav" flip, mirroring [`selection_all_read`](Self::selection_all_read).
    pub fn selection_all_fav(&self) -> bool {
        self.selection_all(|f| f.tags.contains(&Tag::Favourite))
    }

    fn selection_all(&self, pred: impl Fn(&VaultFile) -> bool) -> bool {
        match self.selected.as_deref() {
            Some(sel) if !sel.is_empty() => sel.iter().all(|&i| self.vault.get(i).is_some_and(&pred)),
            _ => false,
        }
    }

    /// Whether any NON-ARCHIVED article carries this byline — i.e. whether filtering to that author
    /// would show anything. [`authors`](Self::authors) is built from the same set, so this is also
    /// the test for "the shelf would offer a row for this author".
    pub fn author_has_live_clippings(&self, author: &str) -> bool {
        self.vault
            .iter()
            .any(|f| f.kind == DocKind::Article && !f.tags.contains(&Tag::Archive) && f.byline.as_deref() == Some(author))
    }

    /// Whether anything is narrowing the library right now — the test for offering a way out of an
    /// empty list, and for whether [`clear_filters_until_visible`](Self::clear_filters_until_visible)
    /// has anything to do.
    pub fn any_filter_active(&self) -> bool {
        self.filter != StatusFilter::All
            || self.tag_filter.is_some()
            || self.author_filter.is_some()
            || !self.query.trim().is_empty()
            || self.books
    }

    /// Reset every list filter. The "Clear filters" escape hatch on an empty list.
    pub fn clear_filters(&mut self) {
        self.filter = StatusFilter::All;
        self.tag_filter = None;
        self.author_filter = None;
        self.query.clear();
        self.books = false;
    }

    /// Drop filters, narrowest first, until the library shows something again — returning a label
    /// for what went, or `None` if nothing needed to (or nothing helps: an empty vault).
    ///
    /// A filter can outlive the clippings that justified it: archive every article by one author and
    /// that author leaves [`authors`](Self::authors), so the shelf stops drawing the row that would
    /// toggle the filter off. Rather than strand the view, a bulk write walks the filters off in
    /// narrowing order — author, then tag/collection, then status + search — so archiving one
    /// author's work inside the Unread view doesn't also throw Unread away.
    pub fn clear_filters_until_visible(&mut self) -> Option<&'static str> {
        if !self.visible().is_empty() || !self.any_filter_active() {
            return None;
        }
        let steps: [(fn(&mut Self) -> bool, &'static str); 3] = [
            (|m| m.author_filter.take().is_some(), "Author filter cleared"),
            (|m| m.tag_filter.take().is_some() || std::mem::take(&mut m.books), "Collection cleared"),
            (
                |m| {
                    let had = m.filter != StatusFilter::All || !m.query.trim().is_empty();
                    m.filter = StatusFilter::All;
                    m.query.clear();
                    had
                },
                "Filters cleared",
            ),
        ];
        let mut label = None;
        for (step, msg) in steps {
            if step(self) {
                label = Some(msg);
                if !self.visible().is_empty() {
                    break;
                }
            }
        }
        label
    }

    /// Whether the open document is a read-only Book (no pen highlights, no status/tag writes).
    pub fn reading_is_book(&self) -> bool {
        match &self.screen {
            Screen::Reading(r) => self.vault.get(r.file_ix).map(|f| f.kind == DocKind::Book).unwrap_or(false),
            _ => false,
        }
    }

    /// The open document's reading status (NotRead when not reading anything).
    pub fn reading_status(&self) -> ReadStatus {
        match &self.screen {
            Screen::Reading(r) => self.vault.get(r.file_ix).map(|f| f.status).unwrap_or_default(),
            _ => ReadStatus::NotRead,
        }
    }
}

/// Case-insensitive substring match (`q` already lowercased) over a file's title OR byline — the
/// predicate the library search filters `visible()` on.
fn title_or_byline_matches(f: &VaultFile, q: &str) -> bool {
    f.title_lower.contains(q) || f.byline_lower.as_deref().is_some_and(|b| b.contains(q))
}

/// How many `Tick`s a flash survives (~poll cadence × this ≈ a couple of seconds).
pub const FLASH_TICKS: u8 = 12;

/// Everything the outside world can tell the model. Pen events drive selection; taps drive nav.
pub enum ReaderEvent {
    Tap { x: i32, y: i32 },
    /// A press-and-hold on the page — opens the highlight menu for a highlight at/near `(x, y)`. A
    /// quick tap navigates instead, so the two gestures never fight over the thin highlight target.
    LongPress { x: i32, y: i32 },
    PenDown { x: i32, y: i32 },
    PenMove { x: i32, y: i32 },
    PenUp { x: i32, y: i32 },
    /// A horizontal edge swipe in the reading view; `dir` is the page delta (+1 next / −1 prev),
    /// `(x, y)` the swipe start (for edge-gating). Turns the page like a Prev/Next tap.
    Swipe { x: i32, y: i32, dir: i32 },
    /// A file finished loading (runtime read + parsed + laid it out).
    Opened { file_ix: usize, source: String, doc: Doc, pages: Vec<Page> },
    /// A highlight write + re-layout finished; carries the refreshed source/layout. `added` is whether
    /// this was an add (vs. a delete) — only used to pick the flash message.
    Rehighlighted { source: String, doc: Doc, pages: Vec<Page>, added: bool },
    /// A note write (`==…==[^n]` + `[^n]: …`) + re-layout finished; carries the refreshed source/layout.
    NoteAdded { source: String, doc: Doc, pages: Vec<Page> },
    /// A text-size re-layout finished: swap in the new pages + the remapped page index.
    Relaid { pages: Vec<Page>, page_ix: usize },
    /// A status write + re-layout finished for the OPEN file; carries refreshed source/layout + status.
    /// `quiet` (auto-status from a page turn, deferred until after the present) installs the state with
    /// no flash and no redraw — the re-laid pages are visually identical, only frontmatter bytes moved.
    StatusSet { source: String, doc: Doc, pages: Vec<Page>, status: ReadStatus, quiet: bool },
    /// A list-initiated status write finished (the file wasn't open): just update its list marker.
    /// `quiet` as in [`ReaderEvent::StatusSet`].
    StatusMarked { file_ix: usize, status: ReadStatus, quiet: bool },
    /// A tag write + re-layout finished for the OPEN file; carries refreshed source/layout + tags.
    TagsSet { source: String, doc: Doc, pages: Vec<Page>, tags: Vec<Tag> },
    /// A tag write finished for a file that wasn't open: just update its list tags. (Mirrors
    /// [`ReaderEvent::StatusMarked`]; tags are assigned from the reading view, so this is rare.)
    TagsMarked { file_ix: usize, tags: Vec<Tag> },
    /// Background vault sync finished.
    Synced { ok: bool },
    /// A status line from the background self-update check (fed by the runtime; shown as a flash):
    /// "Checking for updates…" / "Updating…" / "Up to date (vX)" / "Updated to vX".
    UpdateChecked { msg: String },
    /// The runtime rescanned the vault after a sync — swap in the fresh file list (remapping the
    /// open file's index by path, since new/removed articles shift the sorted order).
    VaultRescanned { files: Vec<VaultFile> },
    Tick,
}

/// What the update step asks the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ReaderEffect {
    Redraw(Dirty),
    /// Read + parse + lay out `vault[ix]`, then feed back [`ReaderEvent::Opened`].
    Open(usize),
    /// Apply (`add`) or remove a highlight over `range` in `path`, write it, re-lay-out, then feed
    /// back [`ReaderEvent::Rehighlighted`]; also nudge a vault sync.
    Highlight { path: PathBuf, range: Range<usize>, add: bool },
    /// Attach or update a note on the highlight enclosing `anchor` in `path` (splice/replace `[^n]` +
    /// definition; empty `text` clears it), write it, re-lay-out, then feed back
    /// [`ReaderEvent::NoteAdded`]; also nudge a vault sync.
    AddNote { path: PathBuf, anchor: usize, text: String },
    /// Step the persisted text size by `delta` (−1 / +1), re-lay-out the open doc keeping the reading
    /// position, then feed back [`ReaderEvent::Relaid`].
    Rescale { delta: i32 },
    /// Write `status` into `vault[file_ix]`'s front matter (from the open source if it's the current
    /// file, else from disk); nudge sync. Feeds back [`ReaderEvent::StatusSet`] when the file is open
    /// (re-laid-out) or [`ReaderEvent::StatusMarked`] when it was changed from the list. `quiet` marks
    /// a page-turn auto-advance: the runtime defers the write until after the turn has painted (so a
    /// tap never waits on file I/O + relayout) and the feedback installs silently.
    SetStatus { file_ix: usize, status: ReadStatus, quiet: bool },
    /// Write `tags` (the managed set) into `vault[file_ix]`'s front matter, merging over any
    /// non-managed Obsidian tags already present; nudge sync. Feeds back [`ReaderEvent::TagsSet`]
    /// when the file is open (re-laid-out) or [`ReaderEvent::TagsMarked`] otherwise.
    SetTags { file_ix: usize, tags: Vec<Tag> },
    /// First page turn inside focus: re-paginate to the near-full-screen focus grid with a forced
    /// page break at `anchor` (the first byte NOT visible on the entry screen when turning forward,
    /// or the first visible byte when turning back), then feed back [`ReaderEvent::Relaid`] on the
    /// page starting at (forward) or ending at (backward) the anchor — continuous, no repeats.
    FocusPaginate { anchor: usize, forward: bool },
    /// Leaving focus after in-focus turns: restore the normal content area, pinned so the current
    /// top line (`anchor`) stays the page top; feed back [`ReaderEvent::Relaid`].
    RestoreLayout { anchor: usize },
    /// Library ↺: run a vault sync (git pull for new clippings + push local edits), then rescan.
    /// The runtime spawns `vaultsync`, polls it without blocking, and feeds back
    /// [`ReaderEvent::VaultRescanned`] (on success) + [`ReaderEvent::Synced`].
    SyncVault,
    Exit,
}

/// Redraw hint for the e-ink refresh strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Full,
    /// x, y, w, h in framebuffer pixels.
    Region(i32, i32, i32, i32),
}
