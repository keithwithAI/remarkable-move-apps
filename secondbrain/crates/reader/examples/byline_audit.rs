//! Audit the bylines a real vault produces, so a precedence regression is one command away.
//!
//!   cargo run -p reader --example byline_audit -- "/path/to/Clippings"
//!
//! Read the PLATFORM AS BYLINE line first. A platform name ("X", "YouTube", "Medium") appearing as
//! a byline means the post is being attributed to its venue rather than to whoever wrote it — which
//! is what happened to all 18 x.com and 23 youtube.com clippings until 2026-08-27. A non-zero count
//! is only correct if those posts genuinely name nobody.
use std::collections::BTreeMap;

const PLATFORMS: [&str; 3] = ["X", "YouTube", "Medium"];

fn main() {
    let dir = std::env::args().nth(1).expect("usage: byline_audit <vault dir>");
    let files = reader::vault::scan(std::path::Path::new(&dir), false);

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut no_byline = 0usize;
    for f in &files {
        match f.byline.as_deref().filter(|b| !b.is_empty()) {
            Some(b) => *counts.entry(b.to_string()).or_default() += 1,
            None => no_byline += 1,
        }
    }

    println!("clippings scanned  : {}", files.len());
    println!("distinct bylines   : {}", counts.len());
    println!("no byline at all   : {no_byline}");

    let platform: usize = PLATFORMS.iter().filter_map(|p| counts.get(*p)).sum();
    println!("PLATFORM AS BYLINE : {platform}");
    for p in PLATFORMS {
        if let Some(n) = counts.get(p) {
            println!("  !! {n} clipping(s) attributed to \"{p}\" rather than an author");
        }
    }

    let mut top: Vec<_> = counts.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    println!("\ntop bylines:");
    for (name, n) in top.iter().take(15) {
        println!("  {n:>3}  {name}");
    }
}
