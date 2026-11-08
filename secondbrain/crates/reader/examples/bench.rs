//! Timing harness for the reader's hot paths — run host-side to measure perf work:
//! `cargo run --release -p reader --example bench -- [file.md] [iters]`.
//!
//! Times four paths that map to felt latency on the device:
//!   layout     — full-document `layout()` pass (open / rescale / every highlight write)
//!   warm frame — `view()` of the same reading page repeatedly (menu open/close, flash, toggles)
//!   page turn  — `view()` cycling `page_ix` each iteration (the cold-glyph path)
//!   library    — `view()` of a 340-file list + `visible()` with a query (search keystroke path)

use reader::model::{ReadStatus, Reading, ReaderModel, Screen, VaultFile};
use reader::{layout, page_minutes, parse, view, LayoutOpts};
use render::Fonts;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (src, iters) = match args.first() {
        Some(a) if a.ends_with(".md") => (std::fs::read_to_string(a).expect("read doc"), args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30)),
        Some(a) => (synth_doc(), a.parse().unwrap_or(30)),
        None => (synth_doc(), 30),
    };
    let fonts = Fonts::embedded();
    let doc = parse(&src);

    // layout: full-document pass.
    let t = Instant::now();
    for _ in 0..iters {
        black_box(layout(black_box(&doc), &fonts, LayoutOpts::default()));
    }
    let layout_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    let pages = layout(&doc, &fonts, LayoutOpts::default());
    let npages = pages.len();
    let mut model = ReaderModel::new(vault(1));
    model.screen = Screen::Reading(Reading {
        page_minutes: page_minutes(&src, &pages),
        file_ix: 0,
        source: src.clone(),
        title: "Bench Doc".into(),
        doc,
        pages,
        page_ix: npages / 2,
        selection: None,
        pen_start: None,
        pen_dwelled: false,
        more_open: false,
        contents_open: false,
        focus: false,
        focus_paginated: false,
        note: None,
        highlight_menu: None,
    });

    // warm frame: identical page re-rendered (selection ops, flash ticks, menu toggles).
    let t = Instant::now();
    for _ in 0..iters {
        black_box(view(black_box(&model), &fonts, 954, 1696));
    }
    let warm_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // page turn: a different page every iteration (cold glyph positions).
    let t = Instant::now();
    for i in 0..iters {
        if let Screen::Reading(r) = &mut model.screen {
            r.page_ix = i % npages.max(1);
        }
        black_box(view(black_box(&model), &fonts, 954, 1696));
    }
    let turn_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // library: list render + query filter over a 340-file vault (search keystroke path).
    let mut lib = ReaderModel::new(vault(340));
    let t = Instant::now();
    for _ in 0..iters {
        black_box(view(black_box(&lib), &fonts, 954, 1696));
    }
    let list_ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    lib.query = "the".into();
    let t = Instant::now();
    for _ in 0..(iters * 20) {
        black_box(lib.visible());
    }
    let visible_us = t.elapsed().as_secs_f64() * 1_000_000.0 / (iters * 20) as f64;

    println!("doc: {} bytes, {} pages, iters {iters}", src.len(), npages);
    println!("layout      {layout_ms:8.2} ms/pass");
    println!("warm frame  {warm_ms:8.2} ms/frame");
    println!("page turn   {turn_ms:8.2} ms/frame");
    println!("library     {list_ms:8.2} ms/frame");
    println!("visible()   {visible_us:8.1} µs/call (query \"the\", 340 files)");
}

/// Deterministic ~40-paragraph document with varied words, a heading and a highlight per section.
fn synth_doc() -> String {
    let mut s = String::new();
    for p in 0..40 {
        s.push_str(&format!("## Section {p}\n\n"));
        for w in 0..120 {
            let word = ["reading", "the", "quiet", "afternoon", "paper", "margin", "sentence", "device", "returns", "slowly"][(p * 7 + w * 3) % 10];
            if w == 40 {
                s.push_str("==highlighted phrase== ");
            }
            s.push_str(word);
            s.push(' ');
        }
        s.push_str("\n\n");
    }
    s
}

fn vault(n: usize) -> Vec<VaultFile> {
    (0..n)
        .map(|i| VaultFile {
            kind: reader::model::DocKind::Article,
            path: PathBuf::from(format!("/v/{i}.md")),
            title_lower: format!("the clipping number {i} of considerable length"),
            title: format!("The Clipping Number {i} Of Considerable Length"),
            has_highlights: i % 3 == 0,
            published: Some(format!("2025-01-{:02}", (i % 28) + 1)),
            created: None,
            byline_lower: Some(format!("the publisher {i}")),
            byline: Some(format!("The Publisher {i}")),
            status: match i % 3 {
                0 => ReadStatus::Read,
                1 => ReadStatus::Reading,
                _ => ReadStatus::NotRead,
            },
            tags: Vec::new(),
            minutes: 3 + (i as u32 % 9),
        })
        .collect()
}
