//! Habit notes: one `.md` per habit. Frontmatter defines it (name/schedule/tags — the
//! filterable part); completions live in the body as append-only `- [x] YYYY-MM-DD` lines
//! under a `## Log` heading — Obsidian renders them as a native checklist, the file never
//! bloats its YAML, and single-line appends are the friendliest shape for git auto-merge.

use crate::schedule::{parse_schedule, Schedule};
use chrono::NaiveDate;
use sb_core::fm;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct HabitFile {
    pub path: PathBuf,
    /// Frontmatter `name:` else the filename stem.
    pub name: String,
    pub schedule: Schedule,
    pub tags: Vec<String>,
    /// Dates with a `- [x] YYYY-MM-DD` log line.
    pub done: BTreeSet<NaiveDate>,
    /// The full note — habit notes are tiny, and toggles need the bytes.
    pub source: String,
}

/// Scan `dir` (non-recursive) for habit notes. Any `.md` counts (`type: habit` is convention,
/// not a requirement — a note dropped in the folder should never silently vanish). Missing dir →
/// empty (the view shows the template empty-state). Sorted by name, case-insensitively.
pub fn scan(dir: &Path) -> Vec<HabitFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        // Skip the atomic-write temp files (dot-prefixed).
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let map = fm::parse_map(&source);
        let name = fm::get(&map, "name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("Habit").to_string());
        out.push(HabitFile {
            name,
            schedule: parse_schedule(fm::get(&map, "schedule")),
            tags: fm::get(&map, "tags").map(|v| v.items().iter().map(|s| s.to_string()).collect()).unwrap_or_default(),
            done: parse_log(&source),
            source,
            path,
        });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// The done-dates in a note: every `- [x] YYYY-MM-DD` line (unchecked `- [ ]` lines are ignored).
pub fn parse_log(source: &str) -> BTreeSet<NaiveDate> {
    source
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("- [x] "))
        .filter_map(|rest| NaiveDate::parse_from_str(rest.trim(), "%Y-%m-%d").ok())
        .collect()
}

/// Toggle `date`'s log line, preserving every other byte.
///
/// `on`: append `- [x] <date>` at the END of the `## Log` section (after its last log line,
/// before the next `## ` heading or EOF), creating `\n## Log\n` at EOF when the heading is
/// missing; a no-op if the line already exists. Appending at the end (not date-sorted) keeps
/// writes append-only — git's easiest merge. `off`: remove exactly that line.
pub fn toggle_log(source: &str, date: NaiveDate, on: bool) -> String {
    let line = format!("- [x] {}", date.format("%Y-%m-%d"));
    if on {
        if parse_log(source).contains(&date) {
            return source.to_string();
        }
        // End of the `## Log` section: from the heading, past any content, to the next `## ` or EOF.
        if let Some(h) = source.lines().position(|l| l.trim() == "## Log") {
            let mut insert_at = source.len();
            let mut offset = 0usize;
            for (i, l) in source.split_inclusive('\n').enumerate() {
                if i > h && l.starts_with("## ") {
                    insert_at = offset;
                    break;
                }
                offset += l.len();
            }
            // Back up over trailing blank lines so the entry joins the list, not the gap.
            let mut at = insert_at;
            loop {
                if at == 0 {
                    break;
                }
                // The line ending at `at` spans [prev_start, at); step over its own newline first.
                let end = if source[..at].ends_with('\n') { at - 1 } else { at };
                let prev_start = source[..end].rfind('\n').map(|p| p + 1).unwrap_or(0);
                if prev_start < at && source[prev_start..at].trim().is_empty() {
                    at = prev_start;
                } else {
                    break;
                }
            }
            let mut out = String::with_capacity(source.len() + line.len() + 1);
            out.push_str(&source[..at]);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&line);
            out.push('\n');
            out.push_str(&source[at..]);
            return out;
        }
        // No log section yet — create it at EOF.
        let mut out = String::with_capacity(source.len() + line.len() + 12);
        out.push_str(source);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n## Log\n");
        out.push_str(&line);
        out.push('\n');
        out
    } else {
        // Remove exactly the `- [x] <date>` line (tolerating leading indent), once.
        let mut out = String::with_capacity(source.len());
        let mut removed = false;
        for l in source.split_inclusive('\n') {
            if !removed && l.trim_end_matches(['\n', '\r']).trim_start() == line {
                removed = true;
                continue;
            }
            out.push_str(l);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, day).unwrap()
    }

    const NOTE: &str = "---\ntype: habit\nschedule: daily\ntags: [health]\n---\n\nSome intro text.\n\n## Log\n- [x] 2026-07-08\n- [x] 2026-07-09\n\n## Notes\nKeep at it.\n";

    #[test]
    fn parses_only_checked_dated_lines() {
        let done = parse_log("## Log\n- [x] 2026-07-08\n- [ ] 2026-07-09\n- [x] not-a-date\n- [x] 2026-07-10\n");
        assert_eq!(done.into_iter().collect::<Vec<_>>(), vec![d(8), d(10)]);
    }

    #[test]
    fn toggle_on_appends_at_the_end_of_the_log_section() {
        let out = toggle_log(NOTE, d(10), true);
        assert!(out.contains("- [x] 2026-07-09\n- [x] 2026-07-10\n"), "appended after the last entry:\n{out}");
        assert!(out.contains("\n## Notes\nKeep at it.\n"), "later sections untouched");
        assert_eq!(toggle_log(&out, d(10), true), out, "re-toggling on is a no-op");
    }

    #[test]
    fn toggle_round_trip_is_byte_exact() {
        let on = toggle_log(NOTE, d(10), true);
        assert_eq!(toggle_log(&on, d(10), false), NOTE, "off after on restores the original bytes");
        assert_eq!(toggle_log(NOTE, d(10), false), NOTE, "removing an absent date is a no-op");
    }

    #[test]
    fn toggle_creates_the_log_section_when_missing() {
        let out = toggle_log("---\nschedule: daily\n---\nBody.\n", d(10), true);
        assert!(out.ends_with("Body.\n\n## Log\n- [x] 2026-07-10\n"), "{out}");
        // And an empty file works too.
        let out = toggle_log("", d(10), true);
        assert_eq!(out, "\n## Log\n- [x] 2026-07-10\n");
    }

    #[test]
    fn scan_reads_definition_and_log() {
        let dir = std::env::temp_dir().join(format!("sb-habit-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Morning stretch.md"), NOTE).unwrap();
        std::fs::write(dir.join("named.md"), "---\nname: Read 20 pages\nschedule: [mon, wed]\n---\n## Log\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "nope").unwrap();
        std::fs::write(dir.join(".hidden.md.tmp"), "tmp").unwrap();
        let habits = scan(&dir);
        assert_eq!(habits.len(), 2);
        assert_eq!(habits[0].name, "Morning stretch");
        assert_eq!(habits[0].tags, ["health"]);
        assert_eq!(habits[0].done.len(), 2);
        assert_eq!(habits[1].name, "Read 20 pages", "frontmatter name wins over the filename");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_of_a_missing_dir_is_empty() {
        assert!(scan(Path::new("/definitely/not/here")).is_empty());
    }
}
