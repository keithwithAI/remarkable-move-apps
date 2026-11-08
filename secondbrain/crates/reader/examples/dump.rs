//! Throwaway parser smoke-test: `cargo run -p reader --example dump -- <file-or-dir>`.
//! Parses one file (verbose) or every `.md` in a directory (aggregate + panic/span check).

use std::panic::catch_unwind;
use std::path::Path;
use std::process::ExitCode;

use reader::{parse, Block, Doc};

fn main() -> ExitCode {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg.is_empty() {
        eprintln!("usage: dump <file.md | dir>");
        return ExitCode::FAILURE;
    }
    let path = Path::new(&arg);
    if path.is_dir() {
        scan_dir(path)
    } else {
        dump_one(path);
        ExitCode::SUCCESS
    }
}

fn stats(doc: &Doc) -> (usize, usize, usize, usize) {
    let mut hl_runs = 0;
    let mut imgs = 0;
    let mut runs = 0;
    for b in &doc.blocks {
        match b {
            Block::Paragraph { inlines } | Block::Heading { inlines, .. } | Block::ListItem { inlines, .. } => {
                runs += inlines.len();
                hl_runs += inlines.iter().filter(|r| r.highlight).count();
            }
            Block::Image { .. } => imgs += 1,
            _ => {}
        }
    }
    (doc.blocks.len(), runs, hl_runs, imgs)
}

/// Confirm every inline span still indexes its exact source bytes (the highlight-write contract).
fn spans_ok(source: &str, doc: &Doc) -> bool {
    first_span_mismatch(source, doc).is_none()
}

/// Return the first `(run.text, source[run.src])` that disagree — for diagnosing failures.
fn first_span_mismatch(source: &str, doc: &Doc) -> Option<(String, String)> {
    for b in &doc.blocks {
        let inlines = match b {
            Block::Paragraph { inlines } | Block::Heading { inlines, .. } | Block::ListItem { inlines, .. } => inlines,
            _ => continue,
        };
        for r in inlines {
            let slice = source.get(r.src.clone()).unwrap_or("<OOB>");
            if slice == r.text {
                continue;
            }
            // A synthesized break separator (a space rendered for a source newline) legitimately
            // maps whitespace→whitespace; only a *visible* text run must round-trip exactly.
            if r.text.trim().is_empty() && !slice.is_empty() && slice.trim().is_empty() {
                continue;
            }
            return Some((r.text.clone(), slice.to_string()));
        }
    }
    None
}

fn dump_one(path: &Path) {
    let src = std::fs::read_to_string(path).expect("read file");
    let doc = parse(&src);
    let (blocks, runs, hl, imgs) = stats(&doc);
    println!("title: {:?}", doc.title);
    println!("meta:  {:?}", doc.meta);
    println!("blocks={blocks} runs={runs} highlighted_runs={hl} images={imgs} spans_ok={}", spans_ok(&src, &doc));
    println!("--- first 12 blocks ---");
    for b in doc.blocks.iter().take(12) {
        match b {
            Block::Heading { level, inlines } => println!("H{level}: {}", join(inlines)),
            Block::Paragraph { inlines } => println!("P: {}", truncate(&join(inlines), 90)),
            Block::ListItem { depth, inlines, .. } => println!("{}• {}", "  ".repeat(*depth as usize), truncate(&join(inlines), 80)),
            Block::Image { url, alt, .. } => println!("IMG[{alt}] {url}"),
            Block::CodeBlock { text, .. } => println!("CODE: {}", truncate(text, 60)),
            Block::Rule => println!("---"),
        }
    }
}

fn scan_dir(dir: &Path) -> ExitCode {
    let mut files = 0;
    let mut total_hl = 0;
    let mut span_failures = Vec::new();
    let mut panics = Vec::new();
    let mut with_hl = 0;
    let mut total_imgs = 0;

    let mut entries: Vec<_> = std::fs::read_dir(dir).expect("read dir").filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue, // dataless/unreadable iCloud file
        };
        files += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        match catch_unwind(|| parse(&src)) {
            Ok(doc) => {
                let (_, _, hl, imgs) = stats(&doc);
                total_hl += hl;
                total_imgs += imgs;
                if hl > 0 {
                    with_hl += 1;
                }
                if let Some((text, slice)) = first_span_mismatch(&src, &doc) {
                    if span_failures.len() < 8 {
                        println!("  MISMATCH in {name}:\n    run.text = {text:?}\n    source   = {slice:?}");
                    }
                    span_failures.push(name);
                }
            }
            Err(_) => panics.push(name),
        }
    }

    println!("scanned {files} .md files");
    println!("  files with highlights: {with_hl}");
    println!("  total highlighted runs: {total_hl}");
    println!("  total images: {total_imgs}");
    println!("  PANICS: {} {:?}", panics.len(), &panics[..panics.len().min(10)]);
    println!("  SPAN FAILURES: {} {:?}", span_failures.len(), &span_failures[..span_failures.len().min(10)]);
    if panics.is_empty() && span_failures.is_empty() {
        println!("  ✓ all files parsed cleanly with valid spans");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn join(inlines: &[reader::Inline]) -> String {
    inlines.iter().map(|i| i.text.as_str()).collect()
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}
