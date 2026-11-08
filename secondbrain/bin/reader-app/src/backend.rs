//! Platform backends for the reader: qtfb (device), minifb (macOS sim), PNG (headless).
//! Unlike the planner, the reader needs raw pen strokes (not just taps), so events are queued —
//! one pen drag produces a `PenDown` + many `PenMove`s + a `PenUp` from a single poll.

use anyhow::Result;
use image::RgbImage;
use reader::Dirty;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub enum BackendEvent {
    None,
    Tap { x: i32, y: i32 },
    /// A deliberate press-and-hold at `(x, y)` (finger dwell past [`LONG_PRESS`]) — opens the highlight
    /// menu. Fires while the finger is still down, so the release doesn't also turn the page.
    LongPress { x: i32, y: i32 },
    /// A horizontal page swipe from `(x, y)`; `dir` is the page delta (+1 next / −1 prev).
    Swipe { x: i32, y: i32, dir: i32 },
    PenDown { x: i32, y: i32 },
    PenMove { x: i32, y: i32 },
    PenUp { x: i32, y: i32 },
    Quit,
}

pub trait Backend {
    fn present(&mut self, frame: &RgbImage, dirty: Dirty) -> Result<()>;
    fn poll(&mut self, timeout: Duration) -> Result<BackendEvent>;
    /// Whether input events are already queued (a burst backed up behind a slow render). The event
    /// loop drains them into one iteration — one render + one e-ink refresh for the whole burst.
    /// Default false: the sim and headless backends keep strict frame-per-event semantics (a PNG
    /// script must not be swallowed in one gulp).
    fn has_pending(&self) -> bool {
        false
    }
    fn describe(&self) -> String;
}

/// Distinguishes a tap (release near its press) from a drag. Device-only (the Mac sim derives taps
/// from mouse state directly; headless replays taps from a script).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct TapTracker {
    start: Option<(i32, i32)>,
    last: (i32, i32),
    moved_far: bool,
    /// Long-press eligibility, cancelled past the larger [`LP_SLOP_PX`] (not [`SLOP_PX`]): finger
    /// contact-patch drift and stylus tremor routinely exceed the tap slop within the first ~100ms,
    /// which used to make a deliberately-still hold impossible to land.
    lp_cancelled: bool,
    press_time: Option<Instant>,
    long_fired: bool,
}

const SLOP_PX: i32 = 24;
/// Movement past this (from the press origin) cancels a long-press. Larger than the tap slop
/// [`SLOP_PX`] — a hold only has to be roughly still, but a real drag/swipe still cancels for good.
const LP_SLOP_PX: i32 = 48;
/// Horizontal travel (px) for a touch drag to count as a page-turn swipe rather than a discarded
/// drag. Well above `SLOP_PX` so a sloppy tap never turns the page.
const SWIPE_MIN_PX: i32 = 100;
/// A press held this long without moving far is a long-press (opens the highlight menu) rather than a
/// tap. Fires while the finger is still down, so a deliberate long-press never also turns the page.
const LONG_PRESS: Duration = Duration::from_millis(450);
/// After the pen lifts, the touch panel stays ignored this long — the palm that rode along with the
/// pen doesn't get to tap/swipe/long-press as it peels off the glass.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PEN_TOUCH_COOLDOWN: Duration = Duration::from_millis(400);

/// What a completed touch resolved to. (Named `Gesture`, not `Touch`, to avoid clashing with the
/// evdev `Touch` input device on the linux build.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub enum Gesture {
    Tap(i32, i32),
    /// A horizontal swipe from `(x, y)`; `dir` is the page delta (+1 next / −1 prev).
    Swipe { x: i32, y: i32, dir: i32 },
    None,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl TapTracker {
    pub fn new() -> Self {
        TapTracker { start: None, last: (0, 0), moved_far: false, lp_cancelled: false, press_time: None, long_fired: false }
    }
    pub fn press(&mut self, x: i32, y: i32) {
        self.start = Some((x, y));
        self.last = (x, y);
        self.moved_far = false;
        self.lp_cancelled = false;
        self.press_time = Some(Instant::now());
        self.long_fired = false;
    }
    pub fn movement(&mut self, x: i32, y: i32) {
        if let Some((sx, sy)) = self.start {
            self.last = (x, y);
            if (x - sx).abs() > SLOP_PX || (y - sy).abs() > SLOP_PX {
                self.moved_far = true;
            }
            if (x - sx).abs() > LP_SLOP_PX || (y - sy).abs() > LP_SLOP_PX {
                self.lp_cancelled = true;
            }
        }
    }
    /// Once a roughly-still press passes [`LONG_PRESS`], fire a long-press exactly once (latched).
    /// Cancelled if the touch travelled past [`LP_SLOP_PX`] (that's a drag/swipe). Returns the *press
    /// origin* — where the user aimed — not wherever the contact drifted to. Call each poll while a
    /// touch may be held down.
    pub fn long_press_due(&mut self) -> Option<(i32, i32)> {
        if let Some(anchor) = self.start {
            if !self.lp_cancelled && !self.long_fired && matches!(self.press_time, Some(t) if t.elapsed() >= LONG_PRESS) {
                self.long_fired = true;
                return Some(anchor);
            }
        }
        None
    }
    /// Abandon the in-flight press entirely (palm rejection: the pen took over mid-touch). The
    /// eventual Up resolves to nothing.
    pub fn reset(&mut self) {
        self.start = None;
        self.press_time = None;
        self.moved_far = false;
        self.lp_cancelled = false;
        self.long_fired = false;
    }
    /// A release within `SLOP_PX` is a tap; a longer horizontal-dominant drag is a page swipe;
    /// anything else (a vertical or short drag) is discarded. A press that already fired a long-press
    /// resolves to nothing — the menu opened while the finger was down.
    pub fn release(&mut self) -> Gesture {
        let Some((sx, sy)) = self.start.take() else {
            return Gesture::None;
        };
        if self.long_fired {
            return Gesture::None;
        }
        if !self.moved_far {
            return Gesture::Tap(self.last.0, self.last.1);
        }
        let (dx, dy) = (self.last.0 - sx, self.last.1 - sy);
        if dx.abs() >= SWIPE_MIN_PX && dx.abs() > dy.abs() {
            // Dragging content leftward (dx<0) advances to the next page; rightward → previous.
            Gesture::Swipe { x: sx, y: sy, dir: if dx < 0 { 1 } else { -1 } }
        } else {
            Gesture::None
        }
    }
}

/// Copy the rect `x0..x1 × y0..y1` of `frame` into `dst` — a tightly packed RGB8 buffer of
/// `dst_w × dst_h` — one `copy_from_slice` per row. A full-frame present copies ~1.6M pixels, so the
/// tap-to-refresh latency rides on this being a memcpy, not a per-pixel bounds-checked loop.
/// Coordinates are clamped to both buffers.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn copy_rows(dst: &mut [u8], dst_w: usize, dst_h: usize, frame: &RgbImage, x0: i32, y0: i32, x1: i32, y1: i32) {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1.max(0) as usize).min(dst_w).min(fw);
    let y1 = (y1.max(0) as usize).min(dst_h).min(fh);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let src = frame.as_raw();
    let row = (x1 - x0) * 3;
    for y in y0..y1 {
        let s = (y * fw + x0) * 3;
        let d = (y * dst_w + x0) * 3;
        dst[d..d + row].copy_from_slice(&src[s..s + row]);
    }
}

/* ─────────────────────────── device (qtfb) ─────────────────────────── */

#[cfg(target_os = "linux")]
pub use device::QtfbBackend;

#[cfg(target_os = "linux")]
mod device {
    use super::*;
    use crate::evdev::{Pen, Phase, Touch};
    use qtfb::{Connection, RefreshMode, ServerMsg};

    const FULLS_BETWEEN_DEEP: u32 = 8;

    pub struct QtfbBackend {
        conn: Connection,
        touch: Option<Touch>,
        pen: Option<Pen>,
        tap: TapTracker,
        /// Dwell tracker for the stylus, so holding the nib still on a highlight long-presses just like
        /// a finger. Separate from `tap` (finger) — only one is ever held at a time.
        pen_tap: TapTracker,
        /// Palm rejection state: while the nib is on the glass (`pen_down`), hovering within EMR range,
        /// or within [`PEN_TOUCH_COOLDOWN`] of lifting (`pen_up_at`), the touch panel is ignored.
        pen_down: bool,
        pen_up_at: Option<Instant>,
        pending: VecDeque<BackendEvent>,
        partials_since_full: u32,
        fulls_since_deep: u32,
    }

    impl QtfbBackend {
        pub fn connect() -> Result<QtfbBackend> {
            let key = qtfb::key_from_env().ok_or_else(|| anyhow::anyhow!("QTFB_KEY not set — launch via the AppLoad manifest"))?;
            let conn = Connection::connect(key, qtfb::FBFMT_RMPPM_RGB888, None)?;
            let _ = conn.set_refresh_mode(RefreshMode::Ui);
            let touch = Touch::open().map_err(|e| log::warn!("evdev touch open failed ({e}); taps disabled")).ok();
            let pen = Pen::open().map_err(|e| log::warn!("evdev pen open failed ({e}); highlighting disabled")).ok();
            Ok(QtfbBackend { conn, touch, pen, tap: TapTracker::new(), pen_tap: TapTracker::new(), pen_down: false, pen_up_at: None, pending: VecDeque::new(), partials_since_full: 0, fulls_since_deep: 0 })
        }

        /// Palm rejection: the touch panel is ignored while the pen is in play — nib on the glass,
        /// hovering within EMR range (a palm usually lands *before* the nib), or within the cooldown
        /// of the nib lifting (the palm peels off *after*).
        fn pen_blocks_touch(&self) -> bool {
            self.pen_down
                || self.pen.as_ref().is_some_and(|p| p.in_range())
                || self.pen_up_at.is_some_and(|t| t.elapsed() < PEN_TOUCH_COOLDOWN)
        }

        fn blit(&mut self, frame: &RgbImage, x0: i32, y0: i32, x1: i32, y1: i32) {
            let (w, h) = (self.conn.width as usize, self.conn.height as usize);
            super::copy_rows(self.conn.frame_mut(), w, h, frame, x0, y0, x1, y1);
        }
    }

    impl Backend for QtfbBackend {
        fn present(&mut self, frame: &RgbImage, dirty: Dirty) -> Result<()> {
            match dirty {
                Dirty::Region(x, y, w, h) if self.partials_since_full < FULLS_BETWEEN_DEEP => {
                    self.blit(frame, x, y, x + w, y + h);
                    self.conn.update_partial(x, y, w, h)?;
                    self.partials_since_full += 1;
                }
                _ => {
                    let (w, h) = (self.conn.width as i32, self.conn.height as i32);
                    self.blit(frame, 0, 0, w, h);
                    self.conn.update_all()?;
                    self.partials_since_full = 0;
                    self.fulls_since_deep += 1;
                    if self.fulls_since_deep >= FULLS_BETWEEN_DEEP {
                        let _ = self.conn.request_full_refresh();
                        self.fulls_since_deep = 0;
                    }
                }
            }
            Ok(())
        }

        fn poll(&mut self, timeout: Duration) -> Result<BackendEvent> {
            // A due long-press outranks the queue: deliver it even mid pen-jitter stream, so a still
            // hold can't be starved by its own move events backing up in `pending`. (Finger or stylus;
            // only one is ever held at once.) Ordering is safe: delivered ahead of queued PenMoves, the
            // reader either opens the menu and clears the stroke (stale moves then no-op) or leaves the
            // stroke alive (moves extend it as intended).
            if let Some((x, y)) = self.tap.long_press_due().or_else(|| self.pen_tap.long_press_due()) {
                return Ok(BackendEvent::LongPress { x, y });
            }
            if let Some(ev) = self.pending.pop_front() {
                return Ok(ev);
            }
            let mut fds = [
                libc::pollfd { fd: self.conn.raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.touch.as_ref().map(|t| t.raw_fd()).unwrap_or(-1), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.pen.as_ref().map(|p| p.raw_fd()).unwrap_or(-1), events: libc::POLLIN, revents: 0 },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 3, timeout.as_millis() as libc::c_int) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(BackendEvent::None);
                }
                return Err(e.into());
            }
            if rc == 0 {
                return Ok(BackendEvent::None); // dwell already checked at the top of this call
            }
            // qtfb socket: only watch for the compositor closing us (EOF); ignore forwarded input
            // (a single drain per poll — input comes from evdev directly).
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match self.conn.poll_msg(0) {
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(BackendEvent::Quit),
                    Ok(Some(ServerMsg::Input(_))) | Ok(_) | Err(_) => {}
                }
            }
            if fds[1].revents & libc::POLLIN != 0 {
                if let Some(t) = self.touch.as_mut() {
                    // Palm rejection: always drain the fd (it must not back up), but while the pen is
                    // in play the events are discarded and any in-flight finger press abandoned — a
                    // palm resting beside the nib must never tap, swipe, or long-press.
                    let evs = t.poll_events()?;
                    if self.pen_blocks_touch() {
                        self.tap.reset();
                    } else {
                        for ev in evs {
                            match ev.phase {
                                Phase::Down => self.tap.press(ev.x, ev.y),
                                Phase::Move => self.tap.movement(ev.x, ev.y),
                                Phase::Up => match self.tap.release() {
                                    Gesture::Tap(x, y) => self.pending.push_back(BackendEvent::Tap { x, y }),
                                    Gesture::Swipe { x, y, dir } => self.pending.push_back(BackendEvent::Swipe { x, y, dir }),
                                    Gesture::None => {}
                                },
                            }
                        }
                    }
                }
            }
            if fds[2].revents & libc::POLLIN != 0 {
                if let Some(p) = self.pen.as_mut() {
                    for ev in p.poll_events()? {
                        // Feed the pen dwell tracker in lockstep with the forwarded events, so a still
                        // hold long-presses (a moved-far stroke cancels it → it stays a drag/highlight).
                        match ev.phase {
                            Phase::Down => {
                                self.pen_tap.press(ev.x, ev.y);
                                self.pen_down = true;
                                // The palm usually lands before the nib: abandon its press and drop any
                                // gesture it already queued this drain (keep only pen events).
                                self.tap.reset();
                                self.pending.retain(|e| matches!(e, BackendEvent::PenDown { .. } | BackendEvent::PenMove { .. } | BackendEvent::PenUp { .. }));
                            }
                            Phase::Move => self.pen_tap.movement(ev.x, ev.y),
                            Phase::Up => {
                                self.pen_tap.release();
                                self.pen_down = false;
                                self.pen_up_at = Some(Instant::now());
                            }
                        }
                        self.pending.push_back(match ev.phase {
                            Phase::Down => BackendEvent::PenDown { x: ev.x, y: ev.y },
                            Phase::Move => BackendEvent::PenMove { x: ev.x, y: ev.y },
                            Phase::Up => BackendEvent::PenUp { x: ev.x, y: ev.y },
                        });
                    }
                }
            }
            Ok(self.pending.pop_front().unwrap_or(BackendEvent::None))
        }

        fn has_pending(&self) -> bool {
            !self.pending.is_empty()
        }

        fn describe(&self) -> String {
            format!("qtfb {}x{} (touch={}, pen={})", self.conn.width, self.conn.height, self.touch.is_some(), self.pen.is_some())
        }
    }
}

/* ─────────────────────────── simulator (macOS) ─────────────────────────── */

#[cfg(target_os = "macos")]
pub use sim::SimBackend;

#[cfg(target_os = "macos")]
mod sim {
    use super::*;
    use minifb::{Key, MouseButton, MouseMode, Window, WindowOptions};

    const SCALE: usize = 2;

    /// Mouse maps to both input kinds: a click (little movement) is a touch tap; a drag is a pen
    /// stroke (down → moves → up). Mirrors finger-nav + pen-select on the device with one mouse.
    pub struct SimBackend {
        window: Window,
        buf: Vec<u32>,
        w: usize,
        h: usize,
        was_down: bool,
        press_at: Option<(i32, i32)>,
        pen_active: bool,
        press_time: Option<Instant>,
        long_fired: bool,
        pending: VecDeque<BackendEvent>,
    }

    impl SimBackend {
        pub fn open(width: u32, height: u32) -> Result<SimBackend> {
            let (w, h) = (width as usize / SCALE, height as usize / SCALE);
            let window = Window::new("secondbrain reader (sim) — click=tap, hold=long-press, drag=pen highlight, Esc=quit", w, h, WindowOptions::default())?;
            Ok(SimBackend { window, buf: vec![0xFFFFFF; w * h], w, h, was_down: false, press_at: None, pen_active: false, press_time: None, long_fired: false, pending: VecDeque::new() })
        }
    }

    impl Backend for SimBackend {
        fn present(&mut self, frame: &RgbImage, _dirty: Dirty) -> Result<()> {
            for oy in 0..self.h {
                for ox in 0..self.w {
                    let (mut r, mut g, mut b): (u32, u32, u32) = (0, 0, 0);
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            let p = frame.get_pixel((ox * SCALE + dx) as u32, (oy * SCALE + dy) as u32);
                            r += p[0] as u32;
                            g += p[1] as u32;
                            b += p[2] as u32;
                        }
                    }
                    let n = (SCALE * SCALE) as u32;
                    self.buf[oy * self.w + ox] = ((r / n) << 16) | ((g / n) << 8) | (b / n);
                }
            }
            self.window.update_with_buffer(&self.buf, self.w, self.h)?;
            Ok(())
        }

        fn poll(&mut self, timeout: Duration) -> Result<BackendEvent> {
            if let Some(ev) = self.pending.pop_front() {
                return Ok(ev);
            }
            std::thread::sleep(timeout.min(Duration::from_millis(30)));
            self.window.update();
            if !self.window.is_open() || self.window.is_key_down(Key::Escape) {
                return Ok(BackendEvent::Quit);
            }
            let down = self.window.get_mouse_down(MouseButton::Left);
            let pos = self.window.get_mouse_pos(MouseMode::Clamp).map(|(x, y)| ((x as usize * SCALE) as i32, (y as usize * SCALE) as i32));
            match (self.was_down, down, pos) {
                (false, true, Some(p)) => {
                    self.press_at = Some(p);
                    self.pen_active = false;
                    self.press_time = Some(Instant::now());
                    self.long_fired = false;
                }
                (true, true, Some(p)) => {
                    if let Some((sx, sy)) = self.press_at {
                        if !self.pen_active && ((p.0 - sx).abs() > SLOP_PX || (p.1 - sy).abs() > SLOP_PX) {
                            self.pen_active = true;
                            self.pending.push_back(BackendEvent::PenDown { x: sx, y: sy });
                            self.pending.push_back(BackendEvent::PenMove { x: p.0, y: p.1 });
                        } else if self.pen_active {
                            self.pending.push_back(BackendEvent::PenMove { x: p.0, y: p.1 });
                        } else if !self.long_fired && matches!(self.press_time, Some(t) if t.elapsed() >= LONG_PRESS) {
                            // Held still past the threshold — a long-press (opens the highlight menu).
                            self.long_fired = true;
                            self.pending.push_back(BackendEvent::LongPress { x: sx, y: sy });
                        }
                    }
                }
                (true, false, _) => {
                    let end = pos.or(self.press_at);
                    if self.pen_active {
                        if let Some((x, y)) = end {
                            self.pending.push_back(BackendEvent::PenUp { x, y });
                        }
                    } else if !self.long_fired {
                        if let Some((x, y)) = self.press_at {
                            self.pending.push_back(BackendEvent::Tap { x, y });
                        }
                    }
                    self.press_at = None;
                    self.pen_active = false;
                    self.long_fired = false;
                }
                _ => {}
            }
            self.was_down = down;
            Ok(self.pending.pop_front().unwrap_or(BackendEvent::None))
        }

        fn describe(&self) -> String {
            format!("minifb sim {}x{} (half scale)", self.w * SCALE, self.h * SCALE)
        }
    }
}

/* ─────────────────────────── headless (PNG + script) ─────────────────────────── */

pub struct PngBackend {
    dir: PathBuf,
    events: VecDeque<BackendEvent>,
    frame_no: u32,
}

impl PngBackend {
    /// Script lines: `x,y` or `tap x,y` → a tap; `long x,y` → a long-press (opens the highlight menu);
    /// `pen x1,y1 x2,y2 [x3,y3 …]` → a pen drag (down, moves, up); `swipe x0,y0 x1,y1` → a horizontal
    /// page swipe. Blank lines and `#` comments ignored.
    pub fn new(dir: PathBuf, script: Option<&str>) -> Result<PngBackend> {
        std::fs::create_dir_all(&dir)?;
        let mut events = VecDeque::new();
        if let Some(s) = script {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("pen ") {
                    let pts: Vec<(i32, i32)> = rest.split_whitespace().filter_map(parse_xy).collect();
                    for (i, &(x, y)) in pts.iter().enumerate() {
                        events.push_back(if i == 0 { BackendEvent::PenDown { x, y } } else { BackendEvent::PenMove { x, y } });
                    }
                    if let Some(&(x, y)) = pts.last() {
                        events.push_back(BackendEvent::PenUp { x, y });
                    }
                } else if let Some(rest) = line.strip_prefix("swipe ") {
                    // "swipe x0,y0 x1,y1" → a horizontal page swipe from (x0,y0); dir from x-travel
                    // (right→left = next). For headless page-turn tests.
                    let pts: Vec<(i32, i32)> = rest.split_whitespace().filter_map(parse_xy).collect();
                    if let (Some(&(x0, y0)), Some(&(x1, _))) = (pts.first(), pts.last()) {
                        let dir = if x1 - x0 < 0 { 1 } else { -1 };
                        events.push_back(BackendEvent::Swipe { x: x0, y: y0, dir });
                    }
                } else if let Some(rest) = line.strip_prefix("long ") {
                    if let Some((x, y)) = parse_xy(rest) {
                        events.push_back(BackendEvent::LongPress { x, y });
                    }
                } else {
                    let coords = line.strip_prefix("tap ").unwrap_or(line);
                    if let Some((x, y)) = parse_xy(coords) {
                        events.push_back(BackendEvent::Tap { x, y });
                    }
                }
            }
        }
        Ok(PngBackend { dir, events, frame_no: 0 })
    }
}

fn parse_xy(s: &str) -> Option<(i32, i32)> {
    let (x, y) = s.trim().split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

impl Backend for PngBackend {
    fn present(&mut self, frame: &RgbImage, _dirty: Dirty) -> Result<()> {
        let path = self.dir.join(format!("frame-{:03}.png", self.frame_no));
        frame.save(&path)?;
        log::info!("wrote {}", path.display());
        self.frame_no += 1;
        Ok(())
    }

    fn poll(&mut self, _timeout: Duration) -> Result<BackendEvent> {
        Ok(self.events.pop_front().unwrap_or(BackendEvent::Quit))
    }

    fn describe(&self) -> String {
        format!("png frames → {}", self.dir.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_expands_taps_pen_drags_and_swipes() {
        let dir = std::env::temp_dir().join(format!("sb-reader-pngb-{}", std::process::id()));
        let script = "# demo\n10,20\ntap 30,40\nlong 55,66\npen 100,100 200,150 300,150\nswipe 800,850 200,850\n";
        let mut b = PngBackend::new(dir.clone(), Some(script)).unwrap();
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::Tap { x: 10, y: 20 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::Tap { x: 30, y: 40 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::LongPress { x: 55, y: 66 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::PenDown { x: 100, y: 100 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::PenMove { x: 200, y: 150 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::PenMove { x: 300, y: 150 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::PenUp { x: 300, y: 150 }));
        // A right→left swipe is the next page (dir +1); start point is preserved for edge-gating.
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::Swipe { x: 800, y: 850, dir: 1 }));
        assert!(matches!(b.poll(Duration::ZERO).unwrap(), BackendEvent::Quit));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_rows_matches_a_per_pixel_reference_blit_including_clamped_rects() {
        // A 10×8 gradient frame into a 12×9 destination (different stride), for a set of rects
        // including negative / oversized / empty ones.
        let frame = RgbImage::from_fn(10, 8, |x, y| image::Rgb([x as u8, y as u8, (x + y) as u8]));
        for (x0, y0, x1, y1) in [(0, 0, 10, 8), (2, 3, 7, 6), (-4, -2, 20, 30), (5, 5, 5, 8), (9, 7, 10, 8), (3, 2, 2, 5)] {
            let mut fast = vec![7u8; 12 * 9 * 3];
            let mut slow = fast.clone();
            copy_rows(&mut fast, 12, 9, &frame, x0, y0, x1, y1);
            // The reference: the old per-pixel loop (clamped to both buffers).
            for yy in y0.max(0)..y1.min(9).min(8) {
                for xx in x0.max(0)..x1.min(12).min(10) {
                    let p = frame.get_pixel(xx as u32, yy as u32);
                    let at = (yy as usize * 12 + xx as usize) * 3;
                    slow[at] = p[0];
                    slow[at + 1] = p[1];
                    slow[at + 2] = p[2];
                }
            }
            assert_eq!(fast, slow, "rect ({x0},{y0})..({x1},{y1})");
        }
    }

    #[test]
    fn a_still_hold_past_the_threshold_is_one_long_press_then_swallows_the_release() {
        let mut t = TapTracker::new();
        t.press(100, 200);
        assert!(t.long_press_due().is_none(), "not due before the dwell");
        std::thread::sleep(LONG_PRESS + Duration::from_millis(20));
        assert_eq!(t.long_press_due(), Some((100, 200)), "fires once past the threshold");
        assert!(t.long_press_due().is_none(), "latched — fires only once per press");
        assert!(matches!(t.release(), Gesture::None), "release after a long-press is swallowed (no tap)");

        // A press that travelled past the long-press slop never becomes one, even after the dwell.
        let mut t = TapTracker::new();
        t.press(100, 200);
        t.movement(100 + LP_SLOP_PX + 5, 200);
        std::thread::sleep(LONG_PRESS + Duration::from_millis(20));
        assert!(t.long_press_due().is_none(), "a drag/swipe never long-presses");
    }

    #[test]
    fn a_press_that_drifts_past_the_tap_slop_but_holds_still_long_presses_at_the_anchor() {
        // Finger contact-patch drift / stylus tremor routinely exceeds the tap slop — that must not
        // cancel a deliberate hold (it used to, permanently). Fires at the press origin, not the
        // drifted position, so the hit-test runs where the user aimed.
        let mut t = TapTracker::new();
        t.press(100, 200);
        t.movement(100 + SLOP_PX + 6, 200); // past tap slop, under LP_SLOP_PX
        std::thread::sleep(LONG_PRESS + Duration::from_millis(20));
        assert_eq!(t.long_press_due(), Some((100, 200)), "drift under LP_SLOP_PX still long-presses, at the anchor");
        assert!(matches!(t.release(), Gesture::None), "the release is still swallowed");
    }

    #[test]
    fn jitter_under_the_slop_fires_at_the_anchor() {
        let mut t = TapTracker::new();
        t.press(300, 400);
        for d in [3, -5, 8, -2, 6] {
            t.movement(300 + d, 400 - d);
        }
        std::thread::sleep(LONG_PRESS + Duration::from_millis(20));
        assert_eq!(t.long_press_due(), Some((300, 400)));
    }

    #[test]
    fn reset_abandons_a_held_press() {
        // Palm rejection resets the finger tracker when the pen takes over: no long-press ever fires
        // and the eventual release resolves to nothing.
        let mut t = TapTracker::new();
        t.press(100, 200);
        t.reset();
        std::thread::sleep(LONG_PRESS + Duration::from_millis(20));
        assert!(t.long_press_due().is_none(), "an abandoned press never long-presses");
        assert!(matches!(t.release(), Gesture::None), "an abandoned press never taps");
    }
}
