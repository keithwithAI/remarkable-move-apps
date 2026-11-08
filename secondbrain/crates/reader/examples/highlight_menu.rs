//! Render the tap-a-highlight menu, note editor, and note indicator headlessly (v0.10.1):
//! `cargo run -p reader --example highlight_menu -- [out_dir]`.
//!
//! Drives the real view → update → view loop (hit-test the highlighted span, tap it, then tap
//! Note/Delete/Edit) so every frame is exactly what the device paints — the floating pill, the note
//! editor (empty + prefilled), and the small "this highlight has a note" indicator glyph.

use reader::model::{ReadStatus, Reading, ReaderModel, Screen, VaultFile};
use reader::{layout, page_minutes, parse, update, view, LayoutOpts, ReaderEvent, ReaderTarget};
use render::{Canvas, Fonts};
use std::path::PathBuf;

fn vf() -> VaultFile {
    VaultFile {
        kind: reader::model::DocKind::Article,
        path: PathBuf::from("/v/a.md"),
        title_lower: "a clean, well-lighted place".into(),
        title: "A Clean, Well-Lighted Place".into(),
        has_highlights: true,
        published: Some("2026-07-01".into()),
        created: None,
        byline_lower: Some("scribner".into()),
        byline: Some("Scribner".into()),
        status: ReadStatus::Reading,
        tags: vec![],
        minutes: 6,
    }
}

fn open(src: &str) -> ReaderModel {
    let mut model = ReaderModel::new(vec![vf()]);
    let doc = parse(src);
    let pages = layout(&doc, &Fonts::embedded(), LayoutOpts::default());
    model.screen = Screen::Reading(Reading {
        page_minutes: page_minutes(src, &pages),
        file_ix: 0,
        source: src.into(),
        doc,
        pages,
        page_ix: 0,
        selection: None,
        pen_start: None,
        pen_dwelled: false,
        title: "A Clean, Well-Lighted Place".into(),
        more_open: false,
        contents_open: false,
        focus: false,
        focus_paginated: false,
        note: None,
        highlight_menu: None,
    });
    model
}

fn tap(model: &mut ReaderModel, fonts: &Fonts, canvas: Canvas, pred: impl Fn(&ReaderTarget) -> bool) {
    let out = view(model, fonts, canvas.width, canvas.height);
    let (rect, _) = out.hits.iter().find(|(_, t)| pred(t)).expect("target present in the hit map");
    let (x, y) = rect.center();
    update(model, ReaderEvent::Tap { x, y }, &out.hits, fonts, canvas);
}

/// Like [`tap`] but a press-and-hold — the gesture that opens the highlight menu (a quick tap navigates).
fn long_press(model: &mut ReaderModel, fonts: &Fonts, canvas: Canvas, pred: impl Fn(&ReaderTarget) -> bool) {
    let out = view(model, fonts, canvas.width, canvas.height);
    let (rect, _) = out.hits.iter().find(|(_, t)| pred(t)).expect("target present in the hit map");
    let (x, y) = rect.center();
    update(model, ReaderEvent::LongPress { x, y }, &out.hits, fonts, canvas);
}

fn save(model: &ReaderModel, fonts: &Fonts, canvas: Canvas, name: &str, out: &str) {
    let path = format!("{out}/{name}");
    view(model, fonts, canvas.width, canvas.height).frame.save(&path).unwrap();
    println!("wrote {path}");
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let fonts = Fonts::embedded();
    let canvas = Canvas::default();

    // A plain highlight (no note yet).
    let plain = "It was late and everyone had left the café except an old man who sat in the shadow the \
        leaves of the tree made against the electric light. ==In the day time the street was dusty, but \
        at night the dew settled the dust== and the old man liked to sit late because he was deaf and \
        now at night it was quiet and he felt the difference between the day and the night.\n\nThe two \
        waiters inside the café knew that the old man was a little drunk, and while he was a good client \
        they knew that if he became too drunk he would leave without paying, so they kept watch on him.";

    // 1. Long-press the highlight → the floating Note / Delete pill (a quick tap would navigate).
    let mut model = open(plain);
    long_press(&mut model, &fonts, canvas, |t| matches!(t, ReaderTarget::TapHighlight { .. }));
    save(&model, &fonts, canvas, "hl-1-menu.png", &out);

    // 2. Tap Note → the (empty) note editor.
    tap(&mut model, &fonts, canvas, |t| *t == ReaderTarget::HighlightAddNote);
    save(&model, &fonts, canvas, "hl-2-note-editor.png", &out);

    // A highlight that already carries a note: `==…==[^1]` + a `[^1]: …` definition.
    let noted = "It was late and everyone had left the café except an old man who sat in the shadow. \
        ==In the day time the street was dusty, but at night the dew settled the dust==[^1] and the old \
        man liked to sit late because he was deaf and felt the difference between day and night.\n\nThe \
        two waiters inside the café knew that the old man was a little drunk.\n\n[^1]: The famous \"nada\" \
        passage — a meditation on nothingness.";

    // 3. The noted highlight renders with a small indicator glyph after it (no menu open).
    let mut m2 = open(noted);
    save(&m2, &fonts, canvas, "hl-3-indicator.png", &out);

    // 4. Long-press it → the pill now reads "Edit note" (has_note = true).
    long_press(&mut m2, &fonts, canvas, |t| matches!(t, ReaderTarget::TapHighlight { .. }));
    save(&m2, &fonts, canvas, "hl-4-edit-menu.png", &out);

    // 5. Tap Edit note → the editor, prefilled with the existing note text.
    tap(&mut m2, &fonts, canvas, |t| *t == ReaderTarget::HighlightAddNote);
    save(&m2, &fonts, canvas, "hl-5-edit-editor-prefilled.png", &out);

    println!("done — 5 frames in {out}");
}
