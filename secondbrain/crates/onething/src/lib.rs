//! Pure "One Thing" logic (Elm-style): [`Model`] + [`update`] + [`view`].
//!
//! One focus session at a time on one deliberately-chosen Todoist task. The timer is wall-clock
//! anchored (`Running.end` − now), so it counts through the device's aggressive suspend; the
//! trade-off — no alarm can fire while suspended, "time's up" appears on wake — is by design.
//! No I/O here: fetch/complete/persist live in `bin/onethingapp`.

pub mod model;
pub mod persist;
pub mod update;
pub mod view;

pub use model::{Dirty, Effect, Event, Model, Picked, Screen, Target, Timer, DEFAULT_MINUTES, PRESETS};
pub use persist::{seed, snapshot, Snapshot};
pub use update::update;
pub use view::{digits_rect, hit_test, rows_per_page, view, HitMap, ViewOutput};
