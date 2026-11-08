//! Render a clipping to PNG(s): `cargo run -p reader --example render -- <file.md> [out_dir] [pages]`
//! Verifies the full parse → layout → paint pipeline against real content, headless.

use reader::{draw_page, layout, parse, LayoutOpts};

fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: render <file.md> [out_dir] [max_pages]");
    let out_dir = args.next().unwrap_or_else(|| ".".to_string());
    let max_pages: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(2);

    let src = std::fs::read_to_string(&file).expect("read file");
    let doc = parse(&src);
    let fonts = render::Fonts::embedded();
    let opts = LayoutOpts::default();
    let pages = layout(&doc, &fonts, opts);

    println!("title: {:?}", doc.title);
    println!("pages: {}", pages.len());
    for (i, page) in pages.iter().enumerate().take(max_pages) {
        let img = draw_page(page, &fonts, opts.page_w, opts.page_h);
        let path = format!("{out_dir}/reader-page-{}.png", i + 1);
        img.save(&path).expect("save png");
        println!("wrote {path}  ({} items)", page.items.len());
    }
}
