//! Reader core for the reMarkable Markdown reader/highlighter.
//!
//! Splits into two pure, host-testable layers:
//!   - [`parse`] — Obsidian-clipped Markdown → [`Doc`] of styled [`Block`]s, every run carrying
//!     its **source byte span** and an Obsidian `==highlight==` flag.
//!   - `layout` (next) — wrap + paginate those blocks into positioned glyph runs, keeping the
//!     glyph→source-byte map that powers pen selection.
//!
//! Nothing here touches the framebuffer or the device; the app crate wires these to a backend.

pub mod book;
pub mod byline;
pub mod imaging;
pub mod layout;
pub mod model;
pub mod paint;
pub mod parse;
pub mod update;
pub mod vault;
pub mod view;

pub use byline::derive_byline;
pub use imaging::prepare;
pub use layout::{
    layout, layout_with_images, page_hit, page_of_byte, range_rects, GlyphRun, LayoutOpts, Page,
    PageItem,
};
pub use book::clean_book_source;
pub use model::{
    Dirty, DocKind, ReadStatus, Reading, ReaderEffect, ReaderEvent, ReaderModel, Screen, Selection,
    SortKey, StatusFilter, Tag, VaultFile,
};
pub use paint::{draw_page, draw_page_with_images, HIGHLIGHT};
pub use parse::{frontmatter, parse, set_status, set_tags, tidy_frontmatter, Block, Doc, Emphasis, Inline, Meta};
pub use update::update;
pub use view::{
    focus_entry_window, hit_test, list_rows_per_page, page_minutes, reading_masthead_height, view, view_with_images, HitMap, ReaderTarget,
    Rect, ViewOutput,
};
