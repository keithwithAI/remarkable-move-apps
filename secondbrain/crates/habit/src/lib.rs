//! Pure habit-tracker logic (Elm-style): [`HabitModel`] + [`update`] + [`view`].
//!
//! No I/O beyond [`file::scan`] — the same code drives the device (qtfb), the Mac simulator
//! (minifb), and headless snapshot tests. Notes are Obsidian `.md`: frontmatter defines the
//! habit; completions are `- [x] YYYY-MM-DD` lines under `## Log`.

pub mod file;
pub mod model;
pub mod schedule;
pub mod update;
pub mod view;

pub use file::{scan, toggle_log, HabitFile};
pub use model::{Dirty, HabitEffect, HabitEvent, HabitModel, Screen, Target};
pub use schedule::{streak, Schedule};
pub use update::update;
pub use view::{hit_test, view, HitMap, ViewOutput};
