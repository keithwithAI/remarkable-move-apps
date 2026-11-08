//! Render the reader's actual screens (list + reading chrome) headlessly:
//! `cargo run -p reader --example screens -- <clippings-dir> [out_dir]`.

use reader::model::{Reading, ReaderModel, Screen};
use reader::{layout, page_minutes, parse, vault, view, LayoutOpts};
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: screens <clippings-dir> [out]");
    let out = std::env::args().nth(2).unwrap_or_else(|| ".".to_string());
    let fonts = render::Fonts::embedded();

    let files = vault::scan(Path::new(&dir), true);
    println!("scanned {} files ({} with highlights)", files.len(), files.iter().filter(|f| f.has_highlights).count());

    let mut model = ReaderModel::new(files);

    let list = view(&model, &fonts, 954, 1696);
    list.frame.save(format!("{out}/app-list.png")).unwrap();
    println!("wrote {out}/app-list.png ({} tap targets)", list.hits.len());

    // Open a clipping that already has highlights, to show the reading chrome + wash together.
    let ix = model.vault.iter().position(|f| f.title.contains("Ultra-Processed")).unwrap_or(0);
    let source = std::fs::read_to_string(&model.vault[ix].path).unwrap();
    let doc = parse(&source);
    let pages = layout(&doc, &fonts, LayoutOpts::default());
    let title = model.vault[ix].title.clone();
    println!("opened {title:?} → {} pages", pages.len());
    model.screen = Screen::Reading(Reading { page_minutes: page_minutes(&source, &pages), file_ix: ix, source, doc, pages, page_ix: 3, selection: None, pen_start: None, pen_dwelled: false, title, more_open: false, contents_open: false, focus: false, focus_paginated: false, note: None, highlight_menu: None });
    let reading = view(&model, &fonts, 954, 1696);
    reading.frame.save(format!("{out}/app-reading.png")).unwrap();
    println!("wrote {out}/app-reading.png");

    // Focus ENTRY overlay: pagination untouched — the central text is pixel-identical to the frame
    // above; the freed chrome bars fill with the adjacent pages' preview lines.
    if let Screen::Reading(r) = &mut model.screen {
        r.focus = true;
    }
    let focus = view(&model, &fonts, 954, 1696);
    focus.frame.save(format!("{out}/app-reading-focus.png")).unwrap();
    println!("wrote {out}/app-reading-focus.png");

    // First in-focus page turn: focus's own full-screen pagination, continuing exactly after the
    // last line the entry screen showed (as the runtime's FocusPaginate arm does).
    if let Screen::Reading(r) = &mut model.screen {
        let window = reader::focus_entry_window(&r.pages, r.page_ix, 1696.0);
        let focus_opts = LayoutOpts { content_top: 14.0, content_bottom: 1696.0 - 56.0, force_break_at: Some(window.end), ..LayoutOpts::default() };
        let pages = layout(&r.doc, &fonts, focus_opts);
        r.page_ix = pages.iter().position(|p| p.runs().map(|g| g.src.start).min().unwrap_or(usize::MAX) >= window.end).unwrap_or(0);
        r.page_minutes = page_minutes(&r.source, &pages);
        r.pages = pages;
        r.focus_paginated = true;
    }
    let turned = view(&model, &fonts, 954, 1696);
    turned.frame.save(format!("{out}/app-reading-focus-turned.png")).unwrap();
    println!("wrote {out}/app-reading-focus-turned.png");
}
