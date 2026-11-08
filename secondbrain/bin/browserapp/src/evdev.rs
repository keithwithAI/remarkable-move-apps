//! Direct touchscreen input via evdev — bypasses the qtfb compositor's input forwarding.
//!
//! Across a long device suspend the compositor stops routing touch to our socket (the app stays
//! visible but goes deaf to taps — the "stuck in the planner" trap). A repaint on resume revives
//! input after *short* suspends but not after long/deep ones. The kernel input device, however,
//! resumes normally regardless of sleep depth, so we read `/dev/input/event3` ourselves and are
//! independent of the compositor. evdev broadcasts to every reader, so co-reading alongside
//! xochitl is fine.
//!
//! Linux/device only. Parses the Elan multitouch (type-B) protocol, tracks the first finger, and
//! emits press/move/release already mapped to framebuffer pixels (954×1696).

use std::io;
use std::os::unix::io::RawFd;

/// reMarkable Paper Pro Move touch panel (Elan). Confirmed via `/proc/bus/input/devices`.
const TOUCH_DEV: &str = "/dev/input/event3";
/// The pen digitizer ("Elan marker input") — same node the reader app uses.
const PEN_DEV: &str = "/dev/input/event2";

/// Framebuffer geometry the qtfb surface uses; raw digitizer coords are mapped into this.
const FB_W: i32 = 954;
const FB_H: i32 = 1696;

// evdev types/codes (linux/input-event-codes.h).
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_PEN: u16 = 0x140;

/// Minimum screen-px delta before a pen Move is emitted (Down/Up always emit at exact coords). The
/// digitizer reports every raw unit, so a resting nib streams sub-pixel jitter — this dead-band is
/// ≈0.4mm, an order of magnitude below glyph size, so selection precision is unaffected.
const MOVE_DEADBAND_PX: i32 = 2;

/// `struct input_event` on aarch64: 16-byte timeval + u16 type + u16 code + i32 value = 24 bytes.
const EVENT_SIZE: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

pub struct TouchEvent {
    pub phase: Phase,
    pub x: i32,
    pub y: i32,
}

pub struct Touch {
    fd: RawFd,
    // Axis calibration read from the device (EVIOCGABS).
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    // First-finger (slot 0) state machine.
    cur_slot: i32,
    raw_x: i32,
    raw_y: i32,
    down: bool,
    pending_down: bool,
    pending_up: bool,
    // A fresh position has arrived for the in-progress contact — guards against emitting a `Down` at
    // the previous gesture's stale coordinates when the kernel doesn't resend an unchanged position.
    seen_pos: bool,
    moved: bool,
    buf: [u8; EVENT_SIZE * 64],
}

impl Touch {
    /// Open the touch device non-blocking and read its axis ranges. Errors if the node is missing
    /// or unreadable (caller may fall back to compositor input).
    pub fn open() -> io::Result<Touch> {
        let path = std::ffi::CString::new(TOUCH_DEV).expect("static path has no NUL");
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (x_min, x_max) = abs_range(fd, ABS_MT_POSITION_X).unwrap_or((0, FB_W - 1));
        let (y_min, y_max) = abs_range(fd, ABS_MT_POSITION_Y).unwrap_or((0, FB_H - 1));
        log::info!(
            "evdev touch {TOUCH_DEV}: raw x[{x_min}..{x_max}] y[{y_min}..{y_max}] → {FB_W}x{FB_H}"
        );
        Ok(Touch {
            fd,
            x_min,
            x_max,
            y_min,
            y_max,
            cur_slot: 0,
            raw_x: 0,
            raw_y: 0,
            down: false,
            pending_down: false,
            pending_up: false,
            seen_pos: false,
            moved: false,
            buf: [0u8; EVENT_SIZE * 64],
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    fn to_screen(&self) -> (i32, i32) {
        (
            map_axis(self.raw_x, self.x_min, self.x_max, FB_W),
            map_axis(self.raw_y, self.y_min, self.y_max, FB_H),
        )
    }

    /// Drain all currently-available input, returning first-finger phase transitions. Non-blocking:
    /// stops at `EAGAIN`. Call after `poll()` reports the fd readable.
    pub fn poll_events(&mut self) -> io::Result<Vec<TouchEvent>> {
        let mut out = Vec::new();
        loop {
            let n = unsafe {
                libc::read(self.fd, self.buf.as_mut_ptr() as *mut libc::c_void, self.buf.len())
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EAGAIN) => break, // no more data queued
                    Some(libc::EINTR) => continue,
                    _ => return Err(e),
                }
            }
            if n == 0 {
                break;
            }
            let n = n as usize;
            let mut off = 0;
            while off + EVENT_SIZE <= n {
                let rec = &self.buf[off..off + EVENT_SIZE];
                let etype = u16::from_ne_bytes([rec[16], rec[17]]);
                let code = u16::from_ne_bytes([rec[18], rec[19]]);
                let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
                self.feed(etype, code, value, &mut out);
                off += EVENT_SIZE;
            }
        }
        Ok(out)
    }

    /// One decoded evdev record → advance the type-B state machine; emit on `SYN_REPORT`.
    fn feed(&mut self, etype: u16, code: u16, value: i32, out: &mut Vec<TouchEvent>) {
        match (etype, code) {
            (EV_ABS, ABS_MT_SLOT) => self.cur_slot = value,
            // Only track the first finger (slot 0); extra fingers are ignored.
            (EV_ABS, ABS_MT_TRACKING_ID) if self.cur_slot == 0 => {
                if value >= 0 {
                    self.pending_down = true;
                    self.seen_pos = false; // require a fresh position before we trust the coords
                } else {
                    self.pending_up = true;
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) if self.cur_slot == 0 => {
                self.raw_x = value;
                self.seen_pos = true;
                if self.down {
                    self.moved = true;
                }
            }
            (EV_ABS, ABS_MT_POSITION_Y) if self.cur_slot == 0 => {
                self.raw_y = value;
                self.seen_pos = true;
                if self.down {
                    self.moved = true;
                }
            }
            (EV_SYN, SYN_REPORT) => {
                let (x, y) = self.to_screen();
                if self.pending_down && self.seen_pos {
                    // Safety net: if a previous release was somehow missed we'd still be `down` —
                    // close that gesture out first so the consumer never sees two Downs without an Up.
                    if self.down {
                        out.push(TouchEvent { phase: Phase::Up, x, y });
                    }
                    self.down = true;
                    self.moved = false;
                    // Calibration aid: raw→screen mapping per press.
                    log::info!("touch down raw=({},{}) → screen=({},{})", self.raw_x, self.raw_y, x, y);
                    out.push(TouchEvent { phase: Phase::Down, x, y });
                } else if self.pending_up && self.down {
                    self.down = false;
                    out.push(TouchEvent { phase: Phase::Up, x, y });
                } else if self.down && self.moved {
                    self.moved = false;
                    out.push(TouchEvent { phase: Phase::Move, x, y });
                }
                self.pending_down = false;
                self.pending_up = false;
            }
            _ => {}
        }
    }
}

impl Drop for Touch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// The pen digitizer: absolute X/Y + `BTN_TOUCH` (nib contact). The pen is routed as a *precise
/// finger* — same [`TouchEvent`] stream, same surface router — so it drives Kindle's own
/// hold→drag→pick-colour highlight flow with nib precision. Hover (`BTN_TOOL_PEN` without contact)
/// emits nothing but is tracked (`in_range`) so the app can ignore the touch panel while the pen is
/// in play — the palm lands before the nib does. Ported from the reader app's proven implementation.
pub struct Pen {
    fd: RawFd,
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    raw_x: i32,
    raw_y: i32,
    down: bool,
    pending_down: bool,
    pending_up: bool,
    // Whether a real position has arrived on each axis — a Down is deferred until both are true, so
    // a BTN_TOUCH arriving before the first coordinates can't anchor a stroke at (0,0).
    have_x: bool,
    have_y: bool,
    moved: bool,
    in_range: bool,
    /// Screen position of the last emitted event — Moves inside [`MOVE_DEADBAND_PX`] are suppressed.
    last_emit: (i32, i32),
    buf: [u8; EVENT_SIZE * 64],
}

impl Pen {
    pub fn open() -> io::Result<Pen> {
        let path = std::ffi::CString::new(PEN_DEV).expect("static path has no NUL");
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let (x_min, x_max) = abs_range(fd, ABS_X).unwrap_or((0, FB_W - 1));
        let (y_min, y_max) = abs_range(fd, ABS_Y).unwrap_or((0, FB_H - 1));
        log::info!("evdev pen {PEN_DEV}: raw x[{x_min}..{x_max}] y[{y_min}..{y_max}] → {FB_W}x{FB_H}");
        Ok(Pen {
            fd,
            x_min,
            x_max,
            y_min,
            y_max,
            raw_x: 0,
            raw_y: 0,
            down: false,
            pending_down: false,
            pending_up: false,
            have_x: false,
            have_y: false,
            moved: false,
            in_range: false,
            last_emit: (0, 0),
            buf: [0u8; EVENT_SIZE * 64],
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Whether the nib is within EMR hover range of the glass (`BTN_TOOL_PEN` held).
    pub fn in_range(&self) -> bool {
        self.in_range
    }

    fn to_screen(&self) -> (i32, i32) {
        (
            map_axis(self.raw_x, self.x_min, self.x_max, FB_W),
            map_axis(self.raw_y, self.y_min, self.y_max, FB_H),
        )
    }

    /// Drain all currently-available pen input (non-blocking; stops at `EAGAIN`).
    pub fn poll_events(&mut self) -> io::Result<Vec<TouchEvent>> {
        let mut out = Vec::new();
        loop {
            let n = unsafe {
                libc::read(self.fd, self.buf.as_mut_ptr() as *mut libc::c_void, self.buf.len())
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EAGAIN) => break,
                    Some(libc::EINTR) => continue,
                    _ => return Err(e),
                }
            }
            if n == 0 {
                break;
            }
            let n = n as usize;
            let mut off = 0;
            while off + EVENT_SIZE <= n {
                let rec = &self.buf[off..off + EVENT_SIZE];
                let etype = u16::from_ne_bytes([rec[16], rec[17]]);
                let code = u16::from_ne_bytes([rec[18], rec[19]]);
                let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
                self.feed(etype, code, value, &mut out);
                off += EVENT_SIZE;
            }
        }
        Ok(out)
    }

    fn feed(&mut self, etype: u16, code: u16, value: i32, out: &mut Vec<TouchEvent>) {
        match (etype, code) {
            (EV_ABS, ABS_X) => {
                self.raw_x = value;
                self.have_x = true;
                if self.down {
                    self.moved = true;
                }
            }
            (EV_ABS, ABS_Y) => {
                self.raw_y = value;
                self.have_y = true;
                if self.down {
                    self.moved = true;
                }
            }
            (EV_KEY, BTN_TOOL_PEN) => self.in_range = value != 0,
            (EV_KEY, BTN_TOUCH) => {
                if value != 0 {
                    self.pending_down = true;
                } else {
                    self.pending_up = true;
                }
            }
            (EV_SYN, SYN_REPORT) => {
                let (x, y) = self.to_screen();
                if self.pending_down && !self.down {
                    if !(self.have_x && self.have_y) {
                        // Contact before any position: keep the Down pending until the first frame
                        // with real coordinates; a contact that ends while still coordinate-less is
                        // a phantom — drop it entirely.
                        if self.pending_up {
                            self.pending_down = false;
                        }
                        self.pending_up = false;
                        return;
                    }
                    self.pending_down = false;
                    self.down = true;
                    self.moved = false;
                    self.last_emit = (x, y);
                    log::info!("pen down raw=({},{}) → screen=({},{})", self.raw_x, self.raw_y, x, y);
                    out.push(TouchEvent { phase: Phase::Down, x, y });
                } else if self.pending_up && self.down {
                    self.down = false;
                    // Up always emits at the exact final position (bypasses the dead-band) — the
                    // selection endpoint stays precise.
                    out.push(TouchEvent { phase: Phase::Up, x, y });
                } else if self.down && self.moved {
                    self.moved = false;
                    if (x - self.last_emit.0).abs() >= MOVE_DEADBAND_PX
                        || (y - self.last_emit.1).abs() >= MOVE_DEADBAND_PX
                    {
                        self.last_emit = (x, y);
                        out.push(TouchEvent { phase: Phase::Move, x, y });
                    }
                }
                self.pending_down = false;
                self.pending_up = false;
            }
            _ => {}
        }
    }
}

impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Map a raw axis value in `[lo, hi]` onto `[0, size)` framebuffer pixels.
fn map_axis(v: i32, lo: i32, hi: i32, size: i32) -> i32 {
    if hi <= lo {
        return 0;
    }
    let v = v.clamp(lo, hi);
    (((v - lo) as i64 * (size - 1) as i64) / (hi - lo) as i64) as i32
}

/// `EVIOCGABS(code)` — read an absolute axis's min/max. Returns `None` if the ioctl fails.
fn abs_range(fd: RawFd, abs_code: u16) -> Option<(i32, i32)> {
    // struct input_absinfo { s32 value, minimum, maximum, fuzz, flat, resolution } = 24 bytes.
    let mut info = [0i32; 6];
    // _IOR('E', 0x40 + code, struct input_absinfo): dir=READ(2)<<30 | size<<16 | 'E'<<8 | nr.
    let nr = 0x40u64 + abs_code as u64;
    let req: u64 = (2u64 << 30) | (24u64 << 16) | ((b'E' as u64) << 8) | nr;
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, info.as_mut_ptr()) };
    if rc < 0 {
        return None;
    }
    Some((info[1], info[2])) // (minimum, maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: u16, c: u16, v: i32) -> (u16, u16, i32) {
        (t, c, v)
    }

    // Replays a scripted evdev stream through the state machine (no device needed).
    fn run(script: &[(u16, u16, i32)]) -> Vec<(u8, i32, i32)> {
        let mut t = Touch {
            fd: -1,
            x_min: 0,
            x_max: 1000,
            y_min: 0,
            y_max: 1000,
            cur_slot: 0,
            raw_x: 0,
            raw_y: 0,
            down: false,
            pending_down: false,
            pending_up: false,
            seen_pos: false,
            moved: false,
            buf: [0u8; EVENT_SIZE * 64],
        };
        let mut out = Vec::new();
        let mut evs = Vec::new();
        for &(ty, c, v) in script {
            t.feed(ty, c, v, &mut evs);
        }
        for e in evs {
            let tag = match e.phase {
                Phase::Down => 0u8,
                Phase::Move => 1,
                Phase::Up => 2,
            };
            out.push((tag, e.x, e.y));
        }
        out
    }

    #[test]
    fn tap_emits_down_then_up() {
        // A finger touches at raw (500,500) and lifts. Ranges are 0..1000 → screen scales by (W-1)/1000.
        let seq = run(&[
            ev(EV_ABS, ABS_MT_SLOT, 0),
            ev(EV_ABS, ABS_MT_TRACKING_ID, 77),
            ev(EV_ABS, ABS_MT_POSITION_X, 500),
            ev(EV_ABS, ABS_MT_POSITION_Y, 500),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_ABS, ABS_MT_TRACKING_ID, -1),
            ev(EV_SYN, SYN_REPORT, 0),
        ]);
        assert_eq!(seq.len(), 2);
        assert_eq!(seq[0].0, 0, "first event is Down");
        assert_eq!(seq[1].0, 2, "second event is Up");
        // 500/1000 * 953 ≈ 476, 500/1000 * 1695 ≈ 847
        assert_eq!(seq[0].1, 476);
        assert_eq!(seq[0].2, 847);
    }

    #[test]
    fn drag_emits_moves_between_down_and_up() {
        let seq = run(&[
            ev(EV_ABS, ABS_MT_TRACKING_ID, 5),
            ev(EV_ABS, ABS_MT_POSITION_X, 100),
            ev(EV_ABS, ABS_MT_POSITION_Y, 100),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_ABS, ABS_MT_POSITION_X, 300),
            ev(EV_ABS, ABS_MT_POSITION_Y, 300),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_ABS, ABS_MT_TRACKING_ID, -1),
            ev(EV_SYN, SYN_REPORT, 0),
        ]);
        let tags: Vec<u8> = seq.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0, 1, 2], "Down, Move, Up");
    }

    #[test]
    fn second_finger_is_ignored() {
        // Slot 1 activity must not produce events while slot 0 owns the gesture.
        let seq = run(&[
            ev(EV_ABS, ABS_MT_SLOT, 0),
            ev(EV_ABS, ABS_MT_TRACKING_ID, 1),
            ev(EV_ABS, ABS_MT_POSITION_X, 200),
            ev(EV_ABS, ABS_MT_POSITION_Y, 200),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_ABS, ABS_MT_SLOT, 1),
            ev(EV_ABS, ABS_MT_TRACKING_ID, 2),
            ev(EV_ABS, ABS_MT_POSITION_X, 900),
            ev(EV_ABS, ABS_MT_POSITION_Y, 900),
            ev(EV_SYN, SYN_REPORT, 0),
        ]);
        // Only the slot-0 Down; the slot-1 contact produces nothing.
        assert_eq!(seq.len(), 1);
        assert_eq!(seq[0].0, 0);
    }

    #[test]
    fn missed_release_is_closed_before_next_down() {
        // Two presses with the release of the first one dropped (e.g. a lost SYN). The safety net
        // must inject an Up for the stale gesture before the new Down, so the stream stays balanced.
        let seq = run(&[
            ev(EV_ABS, ABS_MT_TRACKING_ID, 1),
            ev(EV_ABS, ABS_MT_POSITION_X, 100),
            ev(EV_ABS, ABS_MT_POSITION_Y, 100),
            ev(EV_SYN, SYN_REPORT, 0),
            // No tracking-id -1 / release here — a new contact starts while we still think we're down.
            ev(EV_ABS, ABS_MT_TRACKING_ID, 2),
            ev(EV_ABS, ABS_MT_POSITION_X, 800),
            ev(EV_ABS, ABS_MT_POSITION_Y, 800),
            ev(EV_SYN, SYN_REPORT, 0),
        ]);
        let tags: Vec<u8> = seq.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0, 2, 0], "Down, (injected) Up, Down");
    }

    #[test]
    fn down_without_a_position_is_not_emitted() {
        // A tracking-id with no accompanying position must not emit a Down at stale (0,0) coords.
        let seq = run(&[
            ev(EV_ABS, ABS_MT_TRACKING_ID, 9),
            ev(EV_SYN, SYN_REPORT, 0),
        ]);
        assert!(seq.is_empty(), "no Down without a fresh position");
    }

    fn test_pen() -> Pen {
        Pen {
            fd: -1,
            x_min: 0,
            x_max: 1000,
            y_min: 0,
            y_max: 1000,
            raw_x: 0,
            raw_y: 0,
            down: false,
            pending_down: false,
            pending_up: false,
            have_x: false,
            have_y: false,
            moved: false,
            in_range: false,
            last_emit: (0, 0),
            buf: [0u8; EVENT_SIZE * 64],
        }
    }

    fn run_pen(p: &mut Pen, script: &[(u16, u16, i32)]) -> Vec<(u8, i32, i32)> {
        let mut evs = Vec::new();
        for &(ty, c, v) in script {
            p.feed(ty, c, v, &mut evs);
        }
        evs.iter()
            .map(|e| {
                let tag = match e.phase {
                    Phase::Down => 0u8,
                    Phase::Move => 1,
                    Phase::Up => 2,
                };
                (tag, e.x, e.y)
            })
            .collect()
    }

    #[test]
    fn pen_stroke_down_move_up() {
        let mut p = test_pen();
        let seq = run_pen(
            &mut p,
            &[
                ev(EV_KEY, BTN_TOOL_PEN, 1), // nib enters hover range
                ev(EV_ABS, ABS_X, 500),
                ev(EV_ABS, ABS_Y, 500),
                ev(EV_SYN, SYN_REPORT, 0), // hover: no event
                ev(EV_KEY, BTN_TOUCH, 1), // nib touches
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_ABS, ABS_X, 700), // drag
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_KEY, BTN_TOUCH, 0), // lift
                ev(EV_SYN, SYN_REPORT, 0),
            ],
        );
        let tags: Vec<u8> = seq.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0, 1, 2], "Down, Move, Up — hover alone emitted nothing");
        assert!(p.in_range(), "still hovering after the stroke");
    }

    #[test]
    fn pen_hover_tracks_in_range_without_events() {
        let mut p = test_pen();
        let seq = run_pen(
            &mut p,
            &[ev(EV_KEY, BTN_TOOL_PEN, 1), ev(EV_ABS, ABS_X, 100), ev(EV_ABS, ABS_Y, 100), ev(EV_SYN, SYN_REPORT, 0)],
        );
        assert!(seq.is_empty(), "hover emits no pointer events");
        assert!(p.in_range());
        let seq = run_pen(&mut p, &[ev(EV_KEY, BTN_TOOL_PEN, 0), ev(EV_SYN, SYN_REPORT, 0)]);
        assert!(seq.is_empty());
        assert!(!p.in_range(), "leaving range clears the palm-rejection flag");
    }

    #[test]
    fn pen_contact_before_position_waits_for_coords() {
        let mut p = test_pen();
        // BTN_TOUCH arrives before any ABS_X/Y: the Down must wait for real coordinates.
        let seq = run_pen(&mut p, &[ev(EV_KEY, BTN_TOUCH, 1), ev(EV_SYN, SYN_REPORT, 0)]);
        assert!(seq.is_empty(), "no Down at (0,0) before coordinates");
        let seq = run_pen(
            &mut p,
            &[ev(EV_ABS, ABS_X, 400), ev(EV_ABS, ABS_Y, 400), ev(EV_SYN, SYN_REPORT, 0)],
        );
        assert_eq!(seq.len(), 1);
        assert_eq!(seq[0].0, 0, "Down lands once coordinates exist");
    }

    #[test]
    fn pen_move_deadband_suppresses_jitter() {
        let mut p = test_pen();
        let seq = run_pen(
            &mut p,
            &[
                ev(EV_KEY, BTN_TOUCH, 1),
                ev(EV_ABS, ABS_X, 500),
                ev(EV_ABS, ABS_Y, 500),
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_ABS, ABS_X, 501), // sub-deadband jitter (raw 1/1000 ≈ 1 screen px)
                ev(EV_SYN, SYN_REPORT, 0),
                ev(EV_ABS, ABS_X, 900), // real motion
                ev(EV_SYN, SYN_REPORT, 0),
            ],
        );
        let tags: Vec<u8> = seq.iter().map(|e| e.0).collect();
        assert_eq!(tags, vec![0, 1], "jitter Move suppressed; real Move passes");
    }
}
