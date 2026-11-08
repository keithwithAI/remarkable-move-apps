//! The recipe browser's Elm-style state. All I/O (scan, doc loading + layout, rating writes,
//! sync, the framebuffer) lives in the binary — `LoadDoc` round-trips through it because layout
//! needs fonts and the file read.

use crate::file::RecipeFile;
use reader::{Doc, Page};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RecipeModel {
    pub recipes: Vec<RecipeFile>,
    pub screen: Screen,
    pub filter: Filter,
    pub sort: Sort,
    /// `(text, ticks_left)` — decremented by Tick.
    pub flash: Option<(String, u8)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub cuisine: Option<String>,
    pub tag: Option<String>,
    /// "quick" = `time_min` present and ≤ 30.
    pub quick: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Title,
    /// Ascending; recipes without a time last.
    Time,
    /// Descending; unrated last.
    Rating,
}

#[derive(Debug, Clone)]
pub enum Screen {
    Library { scroll: usize, shelf_open: bool },
    Reading { ix: usize, doc: Doc, pages: Vec<Page>, page_ix: usize },
}

#[derive(Debug, Clone)]
pub enum RecipeEvent {
    Tap { x: i32, y: i32 },
    Tick,
    Rescanned(Vec<RecipeFile>),
    /// The binary read + parsed + laid out `recipes[ix]`'s note.
    DocLoaded { ix: usize, doc: Doc, pages: Vec<Page> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Open(usize),
    PrevPage,
    NextPage,
    Back,
    /// Open the filter/sort shelf.
    Shelf,
    CloseShelf,
    Cuisine(String),
    TagChip(String),
    Quick,
    SortTitle,
    SortTime,
    SortRating,
    ClearFilters,
    LibPrev,
    LibNext,
    /// Set the open recipe's rating (1..=5).
    Rate(u8),
    Refresh,
    Exit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecipeEffect {
    /// Read + parse + lay out `recipes[ix]`, then feed back [`RecipeEvent::DocLoaded`].
    LoadDoc { ix: usize },
    /// Write `rating:` into the note's frontmatter (read fresh, `fm::set_scalar`, atomic write).
    WriteRating { path: PathBuf, rating: u8 },
    /// Spawn a detached vaultsync.
    Sync,
    /// Rescan the dir and feed back [`RecipeEvent::Rescanned`].
    Rescan,
    Redraw(Dirty),
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dirty {
    Full,
    Region(i32, i32, i32, i32),
}

impl RecipeModel {
    pub fn new(recipes: Vec<RecipeFile>) -> Self {
        RecipeModel {
            recipes,
            screen: Screen::Library { scroll: 0, shelf_open: false },
            filter: Filter::default(),
            sort: Sort::Title,
            flash: None,
        }
    }

    /// Indices matching the active filter, in the active sort order.
    pub fn visible(&self) -> Vec<usize> {
        let mut ix: Vec<usize> = (0..self.recipes.len())
            .filter(|&i| {
                let r = &self.recipes[i];
                self.filter.cuisine.as_deref().is_none_or(|c| r.cuisine.as_deref() == Some(c))
                    && self.filter.tag.as_deref().is_none_or(|t| r.tags.iter().any(|x| x == t))
                    && (!self.filter.quick || r.time_min.is_some_and(|m| m <= 30))
            })
            .collect();
        match self.sort {
            Sort::Title => {} // scan order is already title order
            Sort::Time => ix.sort_by_key(|&i| self.recipes[i].time_min.unwrap_or(u32::MAX)),
            Sort::Rating => ix.sort_by_key(|&i| std::cmp::Reverse(self.recipes[i].rating.unwrap_or(0))),
        }
        ix
    }

    /// Distinct cuisines / tags across the scanned set (the shelf's chip options), sorted.
    pub fn cuisines(&self) -> Vec<String> {
        let mut v: Vec<String> = self.recipes.iter().filter_map(|r| r.cuisine.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
    pub fn all_tags(&self) -> Vec<String> {
        let mut v: Vec<String> = self.recipes.iter().flat_map(|r| r.tags.iter().cloned()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub(crate) fn flash(&mut self, text: &str) {
        self.flash = Some((text.to_string(), 12));
    }
}
