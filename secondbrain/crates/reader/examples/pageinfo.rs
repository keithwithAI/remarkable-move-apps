//! Print each page's source-byte span + run/image counts, to reason about pen hit-testing.
//! `cargo run -p reader --example pageinfo -- <file.md>`
use reader::{layout, parse, LayoutOpts};

fn main() {
    let path = std::env::args().nth(1).expect("usage: pageinfo <file.md>");
    let src = std::fs::read_to_string(&path).unwrap();
    let fonts = render::Fonts::embedded();
    let doc = parse(&src);
    let pages = layout(&doc, &fonts, LayoutOpts::default());
    println!("{} pages, {} source bytes, {} blocks", pages.len(), src.len(), doc.blocks.len());
    for (i, p) in pages.iter().enumerate() {
        let runs: Vec<_> = p.runs().collect();
        let lo = runs.iter().map(|r| r.src.start).min().unwrap_or(0);
        let hi = runs.iter().map(|r| r.src.end).max().unwrap_or(0);
        let total = p.items.len();
        let text = runs.len();
        println!("page {i:2}: src [{lo:5}..{hi:5}]  runs={text:3}  items={total:3}  (non-text={})", total - text);
    }
}
