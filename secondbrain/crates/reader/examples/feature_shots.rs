//! Render the reader's new UI states headlessly for visual review:
//! `cargo run -p reader --example feature_shots -- <clippings-dir> [out_dir]`
//! Shots: list sorted by recency, list filtered to Reading, and the reading view at 1.0× vs 1.5×
//! (bigger text + spacing) with the status chip + A-/A+ controls.

use reader::model::{Reading, ReaderModel, Screen};
use reader::{layout, page_minutes, parse, vault, view, LayoutOpts, SortKey, StatusFilter};
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: feature_shots <clippings-dir> [out]");
    let out = std::env::args().nth(2).unwrap_or_else(|| ".".to_string());
    let fonts = render::Fonts::embedded();

    let files = vault::scan(Path::new(&dir), true);
    println!("scanned {} files", files.len());
    for f in &files {
        println!("  {:?}  published={:?}  status={:?}", f.title, f.published, f.status);
    }
    let mut model = ReaderModel::new(files);

    // 1. List, sorted by recency (newest published first).
    model.sort = SortKey::Recent;
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-list-recent.png")).unwrap();

    // 2. List, filtered to Reading only.
    model.filter = StatusFilter::Reading;
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-list-reading.png")).unwrap();
    model.filter = StatusFilter::All;
    model.sort = SortKey::Title;

    // 2b. Multi-select: the bulk-action bar replaces the nav icons, with nothing ticked (the three
    // actions inert) and then a whole page ticked by "All" (the chip now reading "None").
    model.selected = Some(Vec::new());
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-select-empty.png")).unwrap();
    model.selected = Some(model.visible().into_iter().take(reader::list_rows_per_page(1696)).collect());
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-select-all.png")).unwrap();

    // 2c. The same bar on the committed search results — no pager (the header carries it).
    model.query = "the".into();
    model.screen = Screen::Search { scroll: 0, kb: false };
    model.selected = Some(model.visible().into_iter().take(3).collect());
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-select-search.png")).unwrap();
    model.selected = None;
    model.query = String::new();
    model.screen = Screen::List { scroll: 0 };
    model.sort = SortKey::Recent;

    // Open a clipping (status: reading in its front matter) for the reading shots.
    let ix = model.vault.iter().position(|f| f.title.contains("Deep Work")).unwrap_or(0);
    let source = std::fs::read_to_string(&model.vault[ix].path).unwrap();
    let title = model.vault[ix].title.clone();
    let doc = parse(&source);

    // 3. Reading at base size.
    let pages = layout(&doc, &fonts, LayoutOpts::default());
    let n0 = pages.len();
    model.screen = Screen::Reading(Reading {
        page_minutes: page_minutes(&source, &pages),
        file_ix: ix,
        source: source.clone(),
        doc: doc.clone(),
        pages,
        page_ix: 0,
        selection: None,
        pen_start: None,
        pen_dwelled: false,
        title: title.clone(),
        more_open: false,
        contents_open: false,
        focus: false,
        focus_paginated: false,
        note: None,
        highlight_menu: None,
    });
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-reading-1.0.png")).unwrap();

    // 4. Reading at 1.5× — larger text + proportional spacing; same first passage.
    let pages = layout(&doc, &fonts, LayoutOpts { scale: 1.5, ..Default::default() });
    let n1 = pages.len();
    model.screen = Screen::Reading(Reading {
        page_minutes: page_minutes(&source, &pages),
        file_ix: ix,
        source,
        doc,
        pages,
        page_ix: 0,
        selection: None,
        pen_start: None,
        pen_dwelled: false,
        title,
        more_open: false,
        contents_open: false,
        focus: false,
        focus_paginated: false,
        note: None,
        highlight_menu: None,
    });
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-reading-1.5.png")).unwrap();

    println!("pages: {n0} at 1.0× → {n1} at 1.5×; wrote 4 shots to {out}");
}
