//! Frames for the "a filter outlived its control" fix: the shelf with an active author whose
//! clippings are all archived, and the empty list's Clear filters escape hatch.
//! `cargo run -p reader --example stranded_shots -- <clippings-dir> [out_dir]`

use reader::model::{ReaderModel, Tag};
use reader::{vault, view};
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: stranded_shots <clippings-dir> [out]");
    let out = std::env::args().nth(2).unwrap_or_else(|| ".".to_string());
    let fonts = render::Fonts::embedded();
    let files = vault::scan(Path::new(&dir), true);
    let mut model = ReaderModel::new(files);

    // Pick the most-clipped author and archive every one of their clippings — the Jake scenario.
    let author = model.authors().first().map(|(n, _)| n.clone()).expect("some byline");
    for f in &mut model.vault {
        if f.byline.as_deref() == Some(author.as_str()) {
            f.tags.push(Tag::Archive);
        }
    }
    model.author_filter = Some(author.clone());
    println!("archived every clipping by {author:?}; authors() still lists it: {}", model.authors().iter().any(|(n, _)| *n == author));

    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-stranded-list.png")).unwrap();
    model.shelf_open = true;
    view(&model, &fonts, 954, 1696).frame.save(format!("{out}/shot-stranded-shelf.png")).unwrap();
    println!("wrote shot-stranded-list.png + shot-stranded-shelf.png to {out}");
}
