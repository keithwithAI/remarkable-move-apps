//! `qtfbspike` — the D4 hardware gate (throwaway). Proves a native Rust qtfb external app
//! works on the Paper Pro Move before the real planner is deployed:
//!
//! 1. connects to qtfb (key from `QTFB_KEY` when launched by AppLoad, else the test scene),
//! 2. draws a labelled grid so scaling/orientation problems are visible at a glance,
//! 3. echoes every input event to the log AND paints a dot where each touch lands —
//!    if dots appear under your finger, coordinates need no extra mapping.
//!
//! Record the results in PROGRESS.md (works at all? dot under finger? partial-update latency?
//! killed on sleep?). Exits on SIGTERM (AppLoad closing the app) or socket close.

#[cfg(target_os = "linux")]
fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(e) = spike::run() {
        eprintln!("qtfbspike failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    if std::env::args().any(|a| a == "-V" || a == "--version") {
        println!(env!("CARGO_PKG_VERSION"));
        return;
    }
    eprintln!("qtfbspike is device-only (qtfb needs Linux); cross-compile for aarch64 and run via AppLoad.");
}

#[cfg(target_os = "linux")]
mod spike {
    use image::{Rgb, RgbImage};
    use qtfb::{Connection, InputKind, ServerMsg};
    use render::draw::{draw_text, fill_rect, hline, line, rect_outline, vline};
    use render::Fonts;
    use std::time::Instant;

    pub fn run() -> std::io::Result<()> {
        if std::env::args().any(|a| a == "-V" || a == "--version") {
            println!(env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        let key = qtfb::key_from_env().unwrap_or_else(|| {
            log::warn!("no QTFB_KEY (not launched by AppLoad?) — using the default test scene");
            qtfb::DEFAULT_SCENE_KEY
        });
        let mut conn = Connection::connect(key, qtfb::FBFMT_RMPPM_RGB888, None)?;
        let (w, h) = (conn.width, conn.height);
        log::info!("connected: {w}x{h}");

        let fonts = Fonts::embedded();
        let mut canvas = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        draw_test_pattern(&mut canvas, &fonts);
        blit_full(&mut conn, &canvas);
        conn.update_all()?;

        let mut taps: u32 = 0;
        loop {
            match conn.poll_msg(-1)? {
                None => continue,
                Some(ServerMsg::Input(ev)) => {
                    log::info!("input {:?} dev={} x={} y={} d={}", ev.kind, ev.dev_id, ev.x, ev.y, ev.d);
                    if matches!(ev.kind, InputKind::TouchPress | InputKind::PenPress | InputKind::TouchUpdate | InputKind::PenUpdate) {
                        let t0 = Instant::now();
                        let r = 8.0;
                        let (x, y) = (ev.x as f32, ev.y as f32);
                        fill_rect(&mut canvas, x - r, y - r, 2.0 * r, 2.0 * r, Rgb([0, 0, 0]));
                        blit_rect(&mut conn, &canvas, ev.x - 8, ev.y - 8, 16, 16);
                        conn.update_partial(ev.x - 8, ev.y - 8, 16, 16)?;
                        taps += 1;
                        log::info!("dot #{taps} painted in {:?}", t0.elapsed());
                    }
                }
                Some(ServerMsg::InitAck { .. }) => log::info!("unexpected extra init ack"),
                Some(ServerMsg::Unknown(t)) => log::info!("server message type {t}"),
            }
        }
    }

    fn draw_test_pattern(img: &mut RgbImage, fonts: &Fonts) {
        let (w, h) = (img.width() as f32, img.height() as f32);
        for gx in (0..img.width()).step_by(100) {
            vline(img, gx as f32, 0.0, h, Rgb([200, 200, 200]), 1);
        }
        for gy in (0..img.height()).step_by(100) {
            hline(img, 0.0, w, gy as f32, Rgb([200, 200, 200]), 1);
        }
        // Corner markers prove orientation; the arrow points at the top-left origin.
        fill_rect(img, 0.0, 0.0, 60.0, 60.0, Rgb([0, 0, 0]));
        rect_outline(img, w - 60.0, 0.0, 60.0, 60.0, Rgb([0, 0, 0]), 6);
        rect_outline(img, 0.0, h - 60.0, 60.0, 60.0, Rgb([0, 0, 0]), 6);
        fill_rect(img, w - 60.0, h - 60.0, 60.0, 60.0, Rgb([120, 120, 120]));
        line(img, 200.0, 200.0, 80.0, 80.0, Rgb([0, 0, 0]), 6);

        draw_text(img, &fonts.sans_semibold, 48.0, 100.0, 320.0, "qtfb spike — second brain", Rgb([0, 0, 0]));
        draw_text(img, &fonts.sans, 34.0, 100.0, 390.0, "grid = 100 px · filled corner = origin (0,0)", Rgb([60, 60, 60]));
        draw_text(img, &fonts.sans, 34.0, 100.0, 450.0, "tap anywhere: a dot must appear under your finger", Rgb([60, 60, 60]));
        draw_text(img, &fonts.sans, 30.0, 100.0, 520.0, "watch the log: journalctl / stderr shows every event", Rgb([90, 90, 90]));

        // Color calibration strip: judge how Gallery 3 renders the planner's priority palette
        // (saturated bar + the row-tint wash) and the RGB primaries. Tune render::TINT_WASH
        // from what you see here.
        draw_text(img, &fonts.sans_semibold, 30.0, 100.0, 620.0, "COLOR TEST — planner palette + primaries", Rgb([0, 0, 0]));
        let swatches: [(&str, Rgb<u8>); 4] = [
            ("P1", Rgb([213, 0, 0])),
            ("P2", Rgb([244, 81, 30])),
            ("P3", Rgb([246, 191, 38])),
            ("P4", Rgb([51, 182, 121])),
        ];
        let mut y = 650.0;
        for (label, color) in swatches {
            let tint = Rgb([wash(color[0]), wash(color[1]), wash(color[2])]);
            fill_rect(img, 100.0, y, 180.0, 70.0, color);
            fill_rect(img, 290.0, y, 460.0, 70.0, tint);
            draw_text(img, &fonts.sans_semibold, 30.0, 300.0, y + 46.0, label, Rgb([0, 0, 0]));
            draw_text(img, &fonts.sans, 26.0, 370.0, y + 46.0, "tinted row + black text sample", Rgb([0, 0, 0]));
            y += 86.0;
        }
        for (i, c) in [Rgb([0u8, 0, 0]), Rgb([255, 0, 0]), Rgb([0, 255, 0]), Rgb([0, 0, 255])].iter().enumerate() {
            fill_rect(img, 100.0 + i as f32 * 170.0, y + 10.0, 160.0, 60.0, *c);
        }
    }

    /// Same wash as `render::priority_tint` (kept inline: the spike is throwaway).
    fn wash(channel: u8) -> u8 {
        (255.0 * render::TINT_WASH + channel as f32 * (1.0 - render::TINT_WASH)).round() as u8
    }

    fn blit_full(conn: &mut Connection, gray: &RgbImage) {
        let (w, h) = (conn.width as i32, conn.height as i32);
        blit_rect(conn, gray, 0, 0, w, h);
    }

    /// Copy an RGB region into the RGB888 shared framebuffer.
    fn blit_rect(conn: &mut Connection, gray: &RgbImage, x: i32, y: i32, rw: i32, rh: i32) {
        let (w, h) = (conn.width as i32, conn.height as i32);
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + rw).min(w);
        let y1 = (y + rh).min(h);
        let frame = conn.frame_mut();
        for yy in y0..y1 {
            for xx in x0..x1 {
                let p = gray.get_pixel(xx as u32, yy as u32);
                let at = (yy as usize * w as usize + xx as usize) * 3;
                frame[at] = p[0];
                frame[at + 1] = p[1];
                frame[at + 2] = p[2];
            }
        }
    }
}
