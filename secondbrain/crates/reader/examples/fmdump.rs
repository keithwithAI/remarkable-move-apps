//! Print the parsed frontmatter ([`reader::frontmatter`]) for each file path given — a spot-check that
//! a clipping's YAML reads cleanly (used to verify the tidy-frontmatter migration surfaces metadata).
//!
//!   cargo run -p reader --example fmdump -- <file.md> [<file.md> ...]
use reader::frontmatter;

fn main() {
    for path in std::env::args().skip(1) {
        match std::fs::read_to_string(&path) {
            Ok(src) => {
                let m = frontmatter(&src);
                let name = std::path::Path::new(&path).file_name().unwrap().to_string_lossy();
                println!("== {name}");
                println!("   web_link  : {:?}", m.web_link);
                println!("   author    : {:?}", m.author);
                println!("   published : {:?}", m.published);
                println!("   status    : {:?}", m.status);
                println!("   categories: {:?}", m.categories);
                println!("   tags      : {:?}", m.tags);
            }
            Err(e) => eprintln!("!! {path}: {e}"),
        }
    }
}
