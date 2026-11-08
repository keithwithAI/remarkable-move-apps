//! Tidy every clipping's YAML frontmatter to the reader's canonical key set (see
//! [`reader::tidy_frontmatter`]): keep only web link/author/published/category/tags/status, drop the
//! rest + empties, canonicalize a URL `source`/`url`/`link` → `web link`, and turn inline `tags: [..]`
//! into a block list. The **body is preserved byte-for-byte**.
//!
//! Dry-run by default (reports what would change); pass `--write` to apply. Files with non-standard or
//! missing frontmatter are left untouched and listed for manual fixing.
//!
//!   cargo run -p reader --example tidy_frontmatter -- --dir <vault> [--write] [--verbose]
use reader::tidy_frontmatter;
use std::path::PathBuf;

fn main() {
    let mut dir: Option<PathBuf> = None;
    let (mut write, mut verbose) = (false, false);
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dir" => dir = it.next().map(PathBuf::from),
            "--write" => write = true,
            "--verbose" => verbose = true,
            other => eprintln!("ignoring unknown arg {other}"),
        }
    }
    let dir = dir.expect("usage: --dir <vault> [--write] [--verbose]");

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read_dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    entries.sort();

    let mut changed = 0usize;
    let mut attention: Vec<String> = Vec::new();
    for path in &entries {
        let Ok(src) = std::fs::read_to_string(path) else { continue };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // Leave — but flag — files whose first line isn't a standard `---` fence (the tool no-ops them).
        if src.lines().next().unwrap_or("") != "---" {
            attention.push(name.clone());
            continue;
        }
        let out = tidy_frontmatter(&src);
        if out == src {
            continue;
        }
        changed += 1;
        let (before, after) = (keys(&src), keys(&out));
        let dropped: Vec<&String> = before.iter().filter(|k| !after.contains(k)).collect();
        let added: Vec<&String> = after.iter().filter(|k| !before.contains(k)).collect();
        println!("~ {name}");
        if !dropped.is_empty() {
            println!("    drop: {}", dropped.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
        }
        if !added.is_empty() {
            println!("    add:  {}", added.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
        }
        if verbose {
            println!("    ----- before -----\n{}    ----- after ------\n{}", indent(&fm_block(&src)), indent(&fm_block(&out)));
        }
        if write {
            std::fs::write(path, &out).expect("write");
        }
    }

    let verb = if write { "rewritten" } else { "would change" };
    println!("\n{changed} of {} .md files {verb}.", entries.len());
    if !attention.is_empty() {
        println!("\n{} file(s) with non-standard / missing frontmatter — fix by hand:", attention.len());
        for a in &attention {
            println!("  ! {a}");
        }
    }
}

/// Top-level keys (lowercased) in a file's leading `---`…`---` block.
fn keys(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = source.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return out;
    }
    for line in lines {
        if line.trim_end() == "---" {
            break;
        }
        if !line.starts_with([' ', '\t']) && !line.trim_start().starts_with("- ") {
            if let Some((k, _)) = line.split_once(':') {
                out.push(k.trim().to_lowercase());
            }
        }
    }
    out
}

/// The raw `---`…`---` block (including fences), for `--verbose` before/after dumps.
fn fm_block(source: &str) -> String {
    let mut buf = String::new();
    let mut lines = source.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return buf;
    }
    buf.push_str("---\n");
    for line in lines {
        buf.push_str(line);
        if line.trim_end() == "---" {
            break;
        }
    }
    buf
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("      {l}\n")).collect()
}
