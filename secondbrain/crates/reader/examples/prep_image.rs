//! Dev tool: run a real image file through the reader's [`reader::prepare`] pipeline (decode →
//! contain-fit → grayscale → Floyd–Steinberg dither to 16 grays) and save the result, so the
//! on-e-ink rendering can be eyeballed off-device.
//!
//!   cargo run -p reader --example prep_image -- <in> [out.png] [max_w] [max_h]

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: prep_image <in> [out.png] [max_w] [max_h]");
        std::process::exit(2);
    };
    let out = args.next().unwrap_or_else(|| "prepared.png".to_string());
    let max_w: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(842);
    let max_h: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(1410);

    let bytes = std::fs::read(&input).unwrap_or_else(|e| {
        eprintln!("read {input}: {e}");
        std::process::exit(1);
    });
    match reader::prepare(&bytes, max_w, max_h) {
        Some(img) => {
            let (w, h) = (img.width(), img.height());
            img.save(&out).expect("save prepared png");
            println!("{input}: {} bytes → {w}×{h} (fit {max_w}×{max_h}) → {out}", bytes.len());
        }
        None => {
            eprintln!("{input}: not a decodable image");
            std::process::exit(1);
        }
    }
}
