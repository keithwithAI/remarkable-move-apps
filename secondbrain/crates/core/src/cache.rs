//! Offline caches. When a fetch fails (device on a flaky hotspot), binaries fall back to the
//! last good state, flagged `stale` so the renderer can mark it.

use crate::{AgendaState, PlannerState};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

/// Persist any state as JSON (atomic: write to a temp file then rename).
///
/// The temp name carries the writer's pid: the `agenda` timer and the planner app can save the
/// same cache concurrently (the timer is NOT an AppLoad app, so "one app at a time" doesn't
/// cover it), and a SHARED temp name would interleave two writers' bytes and publish a corrupt
/// file. The data is fsync'd before the rename — rename alone is only atomic for metadata, and
/// on a device that can lose power any time an unflushed temp can publish as a truncated file.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Load cached JSON. Returns `None` if absent/corrupt.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Persist the latest agenda (suspend-screen path).
pub fn save(path: &Path, state: &AgendaState) -> std::io::Result<()> {
    save_json(path, state)
}

/// Load the last cached agenda, marking it `stale`. Returns `None` if absent/corrupt.
pub fn load(path: &Path) -> Option<AgendaState> {
    let mut state: AgendaState = load_json(path)?;
    state.stale = true;
    Some(state)
}

/// Persist the latest planner window (interactive app path).
pub fn save_planner(path: &Path, state: &PlannerState) -> std::io::Result<()> {
    save_json(path, state)
}

/// Load the last cached planner window, marking it `stale`. Returns `None` if absent/corrupt.
pub fn load_planner(path: &Path) -> Option<PlannerState> {
    let mut state: PlannerState = load_json(path)?;
    state.stale = true;
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgendaState;
    use chrono::{Local, TimeZone};

    #[test]
    fn round_trips_and_marks_stale() {
        let now = Local.with_ymd_and_hms(2026, 6, 29, 9, 0, 0).unwrap();
        let state = AgendaState::build(vec![], vec![], now);
        let dir = std::env::temp_dir().join(format!("sbcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agenda.json");
        save(&p, &state).unwrap();
        let loaded = load(&p).unwrap();
        assert!(loaded.stale);
        assert_eq!(loaded.date, state.date);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_none() {
        assert!(load(Path::new("/nonexistent/agenda.json")).is_none());
    }

    #[test]
    fn planner_round_trips_and_marks_stale() {
        let now = Local.with_ymd_and_hms(2026, 6, 29, 9, 0, 0).unwrap();
        let today = now.date_naive();
        let state = crate::PlannerState::build(vec![], vec![], (today, today.succ_opt().unwrap()), today, now);
        let dir = std::env::temp_dir().join(format!("sbplanner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("planner.json");
        save_planner(&p, &state).unwrap();
        let loaded = load_planner(&p).unwrap();
        assert!(loaded.stale);
        assert_eq!(loaded.window, state.window);
        assert_eq!(loaded.days.len(), 2, "BTreeMap<NaiveDate, _> must survive JSON keys");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
