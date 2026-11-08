//! Pure recipe-browser logic (Elm-style): [`RecipeModel`] + [`update`] + [`view`].
//!
//! Notes are Obsidian `.md` with frontmatter filter fields (`cuisine`, `time_min`, `tags`,
//! `rating` — the rating is editable on-device); bodies render through the reader crate's
//! pure parse/layout/paint pipeline. No I/O beyond [`file::scan`].

pub mod file;
pub mod model;
pub mod update;
pub mod view;

pub use file::{scan, RecipeFile};
pub use model::{Dirty, Filter, RecipeEffect, RecipeEvent, RecipeModel, Screen, Sort, Target};
pub use update::update;
pub use view::{hit_test, rows_per_page, view, HitMap, ViewOutput, READ_BOTTOM, READ_FIRST_EXTRA, READ_TOP};
