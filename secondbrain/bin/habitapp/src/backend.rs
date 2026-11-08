//! Platform backends for the planner: one trait, three implementations.
//!
//! - [`QtfbBackend`] (Linux/device): AppLoad external app — draws into the qtfb shared
//!   framebuffer and receives touch/pen input on the same socket.
//! - `SimBackend` (macOS): minifb window at half scale, mouse taps.
//! - [`PngBackend`] (any host): headless — writes a PNG per frame and replays taps from a
//!   script file, for snapshot tests and visual review.

use anyhow::Result;
use image::RgbImage;
use std::path::PathBuf;
use std::time::Duration;
use habit::Dirty;

pub enum BackendEvent {
    None,
    Tap { x: i32, y: i32 },
    Quit,
}

pub trait Backend {
    fn present(&mut self, frame: &RgbImage, dirty: Dirty) -> Result<()>;
    /// Wait up to `timeout` for one input event.
    fn poll(&mut self, timeout: Duration) -> Result<BackendEvent>;
    fn describe(&self) -> String;
}

/// Turns press/move/release streams into taps: a tap is a release whose pointer never
/// travelled more than `SLOP_PX` from its press. Robust against e-ink latency (no timing).
/// (Only the device backend consumes raw press/release streams; the simulator synthesizes
/// taps straight from mouse clicks.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct TapTracker {
    start: Option<(i32, i32)>,
    last: (i32, i32),
    moved_far: bool,
}

const SLOP_PX: i32 = 24;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl TapTracker {
    pub fn new() -> Self {
        TapTracker { start: None, last: (0, 0), moved_far: false }
    }
    pub fn press(&mut self, x: i32, y: i32) {
        self.start = Some((x, y));
        self.last = (x, y);
        self.moved_far = false;
    }
    pub fn movement(&mut self, x: i32, y: i32) {
        if let Some((sx, sy)) = self.start {
            self.last = (x, y);
            if (x - sx).abs() > SLOP_PX || (y - sy).abs() > SLOP_PX {
                self.moved_far = true;
            }
        }
    }
    /// Returns the tap position if this release completes a tap.
    pub fn release(&mut self) -> Option<(i32, i32)> {
        let was = self.start.take();
        if was.is_some() && !self.moved_far {
            Some(self.last)
        } else {
            None
        }
    }
}

/* ─────────────────────────── device (qtfb) ─────────────────────────── */

#[cfg(target_os = "linux")]
pub use device::QtfbBackend;

#[cfg(target_os = "linux")]
mod device {
    use super::*;
    use crate::evdev::{Phase, Touch};
    use qtfb::{Connection, InputKind, RefreshMode, ServerMsg};

    /// How many partial updates may pass before the next full update also forces a deep
    /// (deghosting) refresh.
    const FULLS_BETWEEN_DEEP: u32 = 8;

    pub struct QtfbBackend {
        conn: Connection,
        /// Direct touch input (survives suspend). `None` only if the device couldn't be opened, in
        /// which case we fall back to compositor-forwarded input (the suspend-fragile path).
        touch: Option<Touch>,
        tap: TapTracker,
        /// dev_id of the touch point we're tracking on the fallback qtfb-input path (first finger wins).
        tracking_dev: Option<i32>,
        partials_since_full: u32,
        fulls_since_deep: u32,
    }

    impl QtfbBackend {
        pub fn connect() -> Result<QtfbBackend> {
            let key = qtfb::key_from_env().ok_or_else(|| {
                anyhow::anyhow!("QTFB_KEY not set — launch via the AppLoad manifest")
            })?;
            let conn = Connection::connect(key, qtfb::FBFMT_RMPPM_RGB888, None)?;
            // UI waveform: crisp text over speed; taps redraw small regions anyway.
            let _ = conn.set_refresh_mode(RefreshMode::Ui);
            // Read touch straight from the kernel device so input survives suspend — the compositor
            // stops forwarding touch to our socket across a long sleep. Fall back to socket input if
            // the device can't be opened.
            let touch = match Touch::open() {
                Ok(t) => Some(t),
                Err(e) => {
                    log::warn!("evdev touch open failed ({e}); using compositor input (suspend-fragile)");
                    None
                }
            };
            Ok(QtfbBackend {
                conn,
                touch,
                tap: TapTracker::new(),
                tracking_dev: None,
                partials_since_full: 0,
                fulls_since_deep: 0,
            })
        }

        fn blit(&mut self, frame: &RgbImage, x0: i32, y0: i32, x1: i32, y1: i32) {
            let w = self.conn.width as i32;
            let h = self.conn.height as i32;
            let (x0, y0) = (x0.max(0), y0.max(0));
            let (x1, y1) = (x1.min(w), y1.min(h));
            let buf = self.conn.frame_mut();
            for yy in y0..y1 {
                for xx in x0..x1 {
                    let p = frame.get_pixel(xx as u32, yy as u32);
                    let at = (yy as usize * w as usize + xx as usize) * 3;
                    buf[at] = p[0];
                    buf[at + 1] = p[1];
                    buf[at + 2] = p[2];
                }
            }
        }

        /// Legacy input path: one touch/pen event from the qtfb socket. Only used when the evdev
        /// device could not be opened (`self.touch == None`) — this is the suspend-fragile path.
        fn poll_qtfb_input(&mut self) -> Result<BackendEvent> {
            let msg = match self.conn.poll_msg(0) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(BackendEvent::Quit)
                }
                Err(e) => return Err(e.into()),
            };
            let Some(ServerMsg::Input(ev)) = msg else {
                return Ok(BackendEvent::None);
            };
            // Track the first contact only; extra fingers (or the 5-finger AppLoad
            // force-refresh gesture) are ignored.
            match ev.kind {
                InputKind::TouchPress | InputKind::PenPress => {
                    if self.tracking_dev.is_none() {
                        self.tracking_dev = Some(ev.dev_id);
                        self.tap.press(ev.x, ev.y);
                    }
                }
                InputKind::TouchUpdate | InputKind::PenUpdate => {
                    if self.tracking_dev == Some(ev.dev_id) {
                        self.tap.movement(ev.x, ev.y);
                    }
                }
                InputKind::TouchRelease | InputKind::PenRelease => {
                    if self.tracking_dev == Some(ev.dev_id) {
                        self.tracking_dev = None;
                        if let Some((x, y)) = self.tap.release() {
                            return Ok(BackendEvent::Tap { x, y });
                        }
                    }
                }
                _ => {}
            }
            Ok(BackendEvent::None)
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
                        // Periodic deep refresh purges accumulated ghosting.
                        let _ = self.conn.request_full_refresh();
                        self.fulls_since_deep = 0;
                    }
                }
            }
            Ok(())
        }

        fn poll(&mut self, timeout: Duration) -> Result<BackendEvent> {
            // Poll the qtfb socket (for compositor close) and, when available, the touch device.
            // Input comes from evdev directly so it survives suspend; the compositor's own input
            // forwarding dies across a long sleep, which used to trap the app with dead taps.
            let mut fds = [
                libc::pollfd { fd: self.conn.raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd {
                    fd: self.touch.as_ref().map(|t| t.raw_fd()).unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let nfds = if self.touch.is_some() { 2 } else { 1 };
            let rc = unsafe {
                libc::poll(fds.as_mut_ptr(), nfds as libc::nfds_t, timeout.as_millis() as libc::c_int)
            };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    return Ok(BackendEvent::None);
                }
                return Err(e.into());
            }
            if rc == 0 {
                return Ok(BackendEvent::None);
            }

            if self.touch.is_some() {
                // evdev input path. Watch the qtfb socket only for EOF (compositor closed us);
                // ignore any input messages it still forwards so taps aren't double-counted.
                if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    if let Err(e) = self.conn.poll_msg(0) {
                        if e.kind() == std::io::ErrorKind::UnexpectedEof {
                            return Ok(BackendEvent::Quit);
                        }
                    }
                }
                if fds[1].revents & libc::POLLIN != 0 {
                    // Drain the WHOLE batch before returning: the kernel can group a tap's
                    // release and the next tap's press into one SYN batch (fast double-tap) —
                    // returning at the first completed tap would drop the trailing Down and
                    // silently lose the second tap. (The reader-app backend queues; here a
                    // single buffered tap suffices — the tracker holds one contact anyway.)
                    let mut tap: Option<(i32, i32)> = None;
                    for ev in self.touch.as_mut().unwrap().poll_events()? {
                        match ev.phase {
                            Phase::Down => self.tap.press(ev.x, ev.y),
                            Phase::Move => self.tap.movement(ev.x, ev.y),
                            Phase::Up => {
                                if let Some((x, y)) = self.tap.release() {
                                    tap.get_or_insert((x, y));
                                }
                            }
                        }
                    }
                    if let Some((x, y)) = tap {
                        return Ok(BackendEvent::Tap { x, y });
                    }
                }
                Ok(BackendEvent::None)
            } else {
                // Fallback: no evdev device — read compositor-forwarded input (suspend-fragile).
                if fds[0].revents & libc::POLLIN != 0 {
                    self.poll_qtfb_input()
                } else {
                    Ok(BackendEvent::None)
                }
            }
        }

        fn describe(&self) -> String {
            format!("qtfb {}x{}", self.conn.width, self.conn.height)
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

    /// The panel is 1696 px tall — show it at half scale so it fits a laptop screen.
    const SCALE: usize = 2;

    pub struct SimBackend {
        window: Window,
        buf: Vec<u32>,
        w: usize,
        h: usize,
        mouse_was_down: bool,
        press_at: Option<(i32, i32)>,
    }

    impl SimBackend {
        pub fn open(width: u32, height: u32) -> Result<SimBackend> {
            let (w, h) = (width as usize / SCALE, height as usize / SCALE);
            let window = Window::new(
                "secondbrain planner (sim) — click to tap, Esc to quit",
                w,
                h,
                WindowOptions::default(),
            )?;
            Ok(SimBackend { window, buf: vec![0xFFFFFF; w * h], w, h, mouse_was_down: false, press_at: None })
        }
    }

    impl Backend for SimBackend {
        fn present(&mut self, frame: &RgbImage, _dirty: Dirty) -> Result<()> {
            // Box-average SCALE×SCALE blocks down to the window buffer.
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
            std::thread::sleep(timeout.min(Duration::from_millis(50)));
            self.window.update();
            if !self.window.is_open() || self.window.is_key_down(Key::Escape) {
                return Ok(BackendEvent::Quit);
            }
            let down = self.window.get_mouse_down(MouseButton::Left);
            let pos = self.window.get_mouse_pos(MouseMode::Clamp);
            let event = match (self.mouse_was_down, down, pos) {
                (false, true, Some((mx, my))) => {
                    self.press_at = Some(((mx as usize * SCALE) as i32, (my as usize * SCALE) as i32));
                    BackendEvent::None
                }
                (true, false, _) => match self.press_at.take() {
                    Some((x, y)) => BackendEvent::Tap { x, y },
                    None => BackendEvent::None,
                },
                _ => BackendEvent::None,
            };
            self.mouse_was_down = down;
            Ok(event)
        }

        fn describe(&self) -> String {
            format!("minifb sim {}x{} (half scale)", self.w * SCALE, self.h * SCALE)
        }
    }
}

/* ─────────────────────────── headless (PNG + script) ─────────────────────────── */

pub struct PngBackend {
    dir: PathBuf,
    taps: Vec<(i32, i32)>,
    next_tap: usize,
    frame_no: u32,
}

impl PngBackend {
    /// `script`: one `x,y` tap per line (blank lines and `#` comments ignored).
    pub fn new(dir: PathBuf, script: Option<&str>) -> Result<PngBackend> {
        std::fs::create_dir_all(&dir)?;
        let mut taps = Vec::new();
        if let Some(s) = script {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let (x, y) = line
                    .split_once(',')
                    .ok_or_else(|| anyhow::anyhow!("script line not `x,y`: {line}"))?;
                taps.push((x.trim().parse()?, y.trim().parse()?));
            }
        }
        Ok(PngBackend { dir, taps, next_tap: 0, frame_no: 0 })
    }
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
        if self.next_tap < self.taps.len() {
            let (x, y) = self.taps[self.next_tap];
            self.next_tap += 1;
            Ok(BackendEvent::Tap { x, y })
        } else {
            Ok(BackendEvent::Quit) // script exhausted — final frame already written
        }
    }

    fn describe(&self) -> String {
        format!("png frames → {}", self.dir.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_within_slop_reports_release_position() {
        let mut t = TapTracker::new();
        t.press(100, 200);
        t.movement(110, 205);
        assert_eq!(t.release(), Some((110, 205)));
        assert_eq!(t.release(), None, "second release without press is ignored");
    }

    #[test]
    fn drag_beyond_slop_is_not_a_tap() {
        let mut t = TapTracker::new();
        t.press(100, 200);
        t.movement(100, 300); // scrolled/dragged
        t.movement(100, 210); // even if it comes back
        assert_eq!(t.release(), None);
    }

    #[test]
    fn png_backend_replays_script_then_quits() {
        let dir = std::env::temp_dir().join(format!("sb-pngb-{}", std::process::id()));
        let mut b = PngBackend::new(dir.clone(), Some("# demo\n10, 20\n30,40\n")).unwrap();
        assert!(matches!(b.poll(Duration::from_millis(1)).unwrap(), BackendEvent::Tap { x: 10, y: 20 }));
        assert!(matches!(b.poll(Duration::from_millis(1)).unwrap(), BackendEvent::Tap { x: 30, y: 40 }));
        assert!(matches!(b.poll(Duration::from_millis(1)).unwrap(), BackendEvent::Quit));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
