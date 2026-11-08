//! Render the shelf (AUTHORS + sort chips) and the author picker headlessly:
//! `cargo run -p reader --example shelf_shots -- <clippings-dir> [out_dir]`.

use reader::model::{ReaderModel, Screen, SortKey};
use reader::{vault, view};
use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: shelf_shots <clippings-dir> [out]");
    let out = std::env::args().nth(2).unwrap_or_else(|| ".".to_string());
    let fonts = render::Fonts::embedded();

    let files = vault::scan(Path::new(&dir), true);
    println!("scanned {} files", files.len());
    let mut model = ReaderModel::new(files);

    // 1. The shelf with the AUTHORS section + sort chips ("Added" active).
    model.sort = SortKey::Added;
    model.shelf_open = true;
    let shelf = view(&model, &fonts, 954, 1696);
    shelf.frame.save(format!("{out}/shot-shelf.png")).unwrap();
    println!("wrote {out}/shot-shelf.png ({} targets)", shelf.hits.len());
    model.shelf_open = false;

    // 2. The full-screen author picker, one author active.
    model.author_filter = model.authors().first().map(|(n, _)| n.clone());
    model.screen = Screen::Authors { scroll: 0 };
    let picker = view(&model, &fonts, 954, 1696);
    picker.frame.save(format!("{out}/shot-authors.png")).unwrap();
    println!("wrote {out}/shot-authors.png");

    // 3. The list under that author filter (header shows the byline), sorted by Added.
    model.screen = Screen::List { scroll: 0 };
    let list = view(&model, &fonts, 954, 1696);
    list.frame.save(format!("{out}/shot-list-author.png")).unwrap();
    println!("wrote {out}/shot-list-author.png");
}
