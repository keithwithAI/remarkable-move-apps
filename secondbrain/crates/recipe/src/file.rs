//! Recipe notes: one `.md` per recipe. Frontmatter carries the filterable fields (`cuisine`,
//! `time_min`, `tags`, `rating`); the body is ordinary markdown (ingredients + steps) rendered
//! through the reader crate's pipeline. Title = filename stem.

use sb_core::fm;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeFile {
    pub path: PathBuf,
    pub title: String,
    pub title_lower: String,
    pub cuisine: Option<String>,
    pub time_min: Option<u32>,
    pub tags: Vec<String>,
    /// 1..=5, clamped; anything unparseable reads as unrated.
    pub rating: Option<u8>,
}

/// Scan `dir` (non-recursive) for recipe notes: an 8KB head read per file — plenty for any
/// frontmatter — never the whole body. Missing dir → empty (the view shows the template
/// empty-state). Sorted by title, case-insensitively.
pub fn scan(dir: &Path) -> Vec<RecipeFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
            continue;
        }
        let Some(head) = read_head(&path, 8 * 1024) else {
            continue;
        };
        let map = fm::parse_map(&head);
        let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").trim().to_string();
        out.push(RecipeFile {
            title_lower: title.to_lowercase(),
            title,
            cuisine: fm::get(&map, "cuisine").and_then(|v| v.as_str()).map(String::from).filter(|s| !s.is_empty()),
            time_min: fm::get(&map, "time_min").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()),
            tags: fm::get(&map, "tags").map(|v| v.items().iter().map(|s| s.to_string()).collect()).unwrap_or_default(),
            rating: fm::get(&map, "rating")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u8>().ok())
                .map(|r| r.clamp(1, 5)),
            path,
        });
    }
    out.sort_by(|a, b| a.title_lower.cmp(&b.title_lower));
    out
}

fn read_head(path: &Path, cap: usize) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(cap as u64).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_reads_filter_fields_with_tolerant_defaults() {
        let dir = std::env::temp_dir().join(format!("sb-recipe-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Shakshuka.md"),
            "---\ntype: recipe\ncuisine: Middle Eastern\ntime_min: 30\ntags: [breakfast, eggs]\nrating: 4\n---\n## Ingredients\n",
        )
        .unwrap();
        std::fs::write(dir.join("Toast.md"), "just a body, no frontmatter\n").unwrap();
        std::fs::write(dir.join("Weird.md"), "---\nrating: eleven\ntime_min: fast\n---\n").unwrap();
        std::fs::write(dir.join("Clamped.md"), "---\nrating: 9\n---\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let recipes = scan(&dir);
        assert_eq!(recipes.len(), 4);
        assert_eq!(recipes[0].title, "Clamped");
        assert_eq!(recipes[0].rating, Some(5), "out-of-range ratings clamp");
        let shak = recipes.iter().find(|r| r.title == "Shakshuka").unwrap();
        assert_eq!(shak.cuisine.as_deref(), Some("Middle Eastern"));
        assert_eq!(shak.time_min, Some(30));
        assert_eq!(shak.tags, ["breakfast", "eggs"]);
        assert_eq!(shak.rating, Some(4));
        let toast = recipes.iter().find(|r| r.title == "Toast").unwrap();
        assert_eq!((toast.cuisine.as_deref(), toast.time_min, toast.rating), (None, None, None));
        let weird = recipes.iter().find(|r| r.title == "Weird").unwrap();
        assert_eq!((weird.time_min, weird.rating), (None, None), "unparseable numbers read as unset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(scan(Path::new("/definitely/not/here")).is_empty());
    }
}
