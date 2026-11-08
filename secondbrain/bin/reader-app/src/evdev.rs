//! Direct kernel input for the reader: the **touch** panel (`event3`) for navigation taps and the
//! **pen** digitizer (`event2`, "Elan marker input") for drag-to-highlight. Reading straight from
//! `/dev/input/*` bypasses the qtfb compositor, which stops forwarding input across a long suspend
//! (the "stuck" trap the planner hit) — the kernel devices resume normally regardless of sleep depth.
//!
//! The pure state machines run + test on the host; the syscall paths only execute on the device.
//! Pen coordinate range/orientation is read from the device (`EVIOCGABS`) but must be **calibrated
//! on-device** — each press logs `raw → screen` so we can confirm the mapping.

use std::io;
use std::os::unix::io::RawFd;

const TOUCH_DEV: &str = "/dev/input/event3"; // Elan touch (multitouch type-B)
const PEN_DEV: &str = "/dev/input/event2"; // Elan marker input (stylus)

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

/// Minimum screen-px delta before a Move is emitted (Down/Up always emit at exact coords). The
/// digitizer reports every raw unit, so a resting nib streams sub-pixel jitter — this dead-band is
/// ≈0.4mm, an order of magnitude below glyph size, so selection precision is unaffected.
const MOVE_DEADBAND_PX: i32 = 2;

/// `struct input_event` on aarch64 = 16-byte timeval + u16 type + u16 code + i32 value = 24 bytes.
const EVENT_SIZE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

pub struct PointerEvent {
    pub phase: Phase,
    pub x: i32,
    pub y: i32,
}

/// Shared axis calibration + first-contact state machine, driven differently by touch vs pen.
struct Axis {
    x_min: i32,
    x_max: i32,
    y_min: i32,
    y_max: i32,
    raw_x: i32,
    raw_y: i32,
    down: bool,
    pending_down: bool,
    pending_up: bool,
    moved: bool,
    /// Whether a real position has ever been reported on each axis. A Down is deferred until both
    /// are true, so a `BTN_TOUCH` arriving before the first coordinates can't anchor at (0,0).
    have_x: bool,
    have_y: bool,
    /// Screen position of the last emitted event — Moves inside [`MOVE_DEADBAND_PX`] of it are
    /// suppressed (the digitizer streams every raw unit of jitter).
    last_emit: (i32, i32),
}

impl Axis {
    fn new(x: (i32, i32), y: (i32, i32)) -> Self {
        Axis { x_min: x.0, x_max: x.1, y_min: y.0, y_max: y.1, raw_x: 0, raw_y: 0, down: false, pending_down: false, pending_up: false, moved: false, have_x: false, have_y: false, last_emit: (0, 0) }
    }
    fn to_screen(&self) -> (i32, i32) {
        (map_axis(self.raw_x, self.x_min, self.x_max, FB_W), map_axis(self.raw_y, self.y_min, self.y_max, FB_H))
    }
    /// Resolve one report: emit at most one phase transition, mirroring `to_screen()`.
    fn sync(&mut self, out: &mut Vec<PointerEvent>, label: &str) {
        let (x, y) = self.to_screen();
        if self.pending_down && !self.down {
            if !(self.have_x && self.have_y) {
                // Contact reported before any position: keep the Down pending until the first frame
                // that carries real coordinates (never anchor a stroke at a stale/zero position). A
                // contact that ends while still coordinate-less is a phantom — drop it entirely.
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
            log::info!("{label} down raw=({},{}) → screen=({},{})", self.raw_x, self.raw_y, x, y);
            out.push(PointerEvent { phase: Phase::Down, x, y });
        } else if self.pending_up && self.down {
            self.down = false;
            // Up always emits, at the exact final position (bypasses the dead-band) — highlight
            // endpoints stay precise.
            out.push(PointerEvent { phase: Phase::Up, x, y });
        } else if self.down && self.moved {
            self.moved = false;
            if (x - self.last_emit.0).abs() >= MOVE_DEADBAND_PX || (y - self.last_emit.1).abs() >= MOVE_DEADBAND_PX {
                self.last_emit = (x, y);
                out.push(PointerEvent { phase: Phase::Move, x, y });
            }
        }
        self.pending_down = false;
        self.pending_up = false;
    }
}

/// The touch panel: multitouch type-B, first finger (slot 0) only → nav taps.
pub struct Touch {
    fd: RawFd,
    cur_slot: i32,
    axis: Axis,
}

impl Touch {
    pub fn open() -> io::Result<Touch> {
        let fd = open_ro(TOUCH_DEV)?;
        let x = abs_range(fd, ABS_MT_POSITION_X).unwrap_or((0, FB_W - 1));
        let y = abs_range(fd, ABS_MT_POSITION_Y).unwrap_or((0, FB_H - 1));
        log::info!("evdev touch {TOUCH_DEV}: raw x[{}..{}] y[{}..{}] → {FB_W}x{FB_H}", x.0, x.1, y.0, y.1);
        Ok(Touch { fd, cur_slot: 0, axis: Axis::new(x, y) })
    }
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }
    pub fn poll_events(&mut self) -> io::Result<Vec<PointerEvent>> {
        let mut out = Vec::new();
        let fd = self.fd;
        drain(fd, |t, c, v| self.feed(t, c, v, &mut out))?;
        Ok(out)
    }
    fn feed(&mut self, etype: u16, code: u16, value: i32, out: &mut Vec<PointerEvent>) {
        match (etype, code) {
            (EV_ABS, ABS_MT_SLOT) => self.cur_slot = value,
            (EV_ABS, ABS_MT_TRACKING_ID) if self.cur_slot == 0 => {
                if value >= 0 {
                    self.axis.pending_down = true;
                } else {
                    self.axis.pending_up = true;
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) if self.cur_slot == 0 => {
                self.axis.raw_x = value;
                self.axis.have_x = true;
                if self.axis.down {
                    self.axis.moved = true;
                }
            }
            (EV_ABS, ABS_MT_POSITION_Y) if self.cur_slot == 0 => {
                self.axis.raw_y = value;
                self.axis.have_y = true;
                if self.axis.down {
                    self.axis.moved = true;
                }
            }
            (EV_SYN, SYN_REPORT) => self.axis.sync(out, "touch"),
            _ => {}
        }
    }
}

/// The pen digitizer: absolute X/Y + `BTN_TOUCH` (tip contact) → drag selection. Hover
/// (`BTN_TOOL_PEN` without `BTN_TOUCH`) selects nothing, but is tracked (`in_range`) so the backend
/// can ignore the touch panel while the pen is in play — the palm lands before the nib does.
pub struct Pen {
    fd: RawFd,
    axis: Axis,
    in_range: bool,
}

impl Pen {
    pub fn open() -> io::Result<Pen> {
        let fd = open_ro(PEN_DEV)?;
        let x = abs_range(fd, ABS_X).unwrap_or((0, FB_W - 1));
        let y = abs_range(fd, ABS_Y).unwrap_or((0, FB_H - 1));
        log::info!("evdev pen {PEN_DEV}: raw x[{}..{}] y[{}..{}] → {FB_W}x{FB_H} (calibrate on-device)", x.0, x.1, y.0, y.1);
        Ok(Pen { fd, axis: Axis::new(x, y), in_range: false })
    }
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }
    /// Whether the nib is within EMR hover range of the glass (`BTN_TOOL_PEN` held).
    pub fn in_range(&self) -> bool {
        self.in_range
    }
    pub fn poll_events(&mut self) -> io::Result<Vec<PointerEvent>> {
        let mut out = Vec::new();
        let fd = self.fd;
        drain(fd, |t, c, v| self.feed(t, c, v, &mut out))?;
        Ok(out)
    }
    fn feed(&mut self, etype: u16, code: u16, value: i32, out: &mut Vec<PointerEvent>) {
        match (etype, code) {
            (EV_ABS, ABS_X) => {
                self.axis.raw_x = value;
                self.axis.have_x = true;
                if self.axis.down {
                    self.axis.moved = true;
                }
            }
            (EV_ABS, ABS_Y) => {
                self.axis.raw_y = value;
                self.axis.have_y = true;
                if self.axis.down {
                    self.axis.moved = true;
                }
            }
            (EV_KEY, BTN_TOOL_PEN) => self.in_range = value != 0,
            (EV_KEY, BTN_TOUCH) => {
                if value != 0 {
                    self.axis.pending_down = true;
                } else {
                    self.axis.pending_up = true;
                }
            }
            (EV_SYN, SYN_REPORT) => self.axis.sync(out, "pen"),
            _ => {}
        }
    }
}

impl Drop for Touch {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}
impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn open_ro(path: &str) -> io::Result<RawFd> {
    let c = std::ffi::CString::new(path).expect("static path has no NUL");
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Read all queued records from `fd` (non-blocking), decoding each 24-byte `input_event` and
/// calling `on_rec(type, code, value)`. Stops at `EAGAIN`.
fn drain(fd: RawFd, mut on_rec: impl FnMut(u16, u16, i32)) -> io::Result<()> {
    let mut buf = [0u8; EVENT_SIZE * 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EAGAIN) => return Ok(()),
                Some(libc::EINTR) => continue,
                _ => return Err(e),
            }
        }
        if n == 0 {
            return Ok(());
        }
        let n = n as usize;
        let mut off = 0;
        while off + EVENT_SIZE <= n {
            let rec = &buf[off..off + EVENT_SIZE];
            let etype = u16::from_ne_bytes([rec[16], rec[17]]);
            let code = u16::from_ne_bytes([rec[18], rec[19]]);
            let value = i32::from_ne_bytes([rec[20], rec[21], rec[22], rec[23]]);
            on_rec(etype, code, value);
            off += EVENT_SIZE;
        }
    }
}

fn map_axis(v: i32, lo: i32, hi: i32, size: i32) -> i32 {
    if hi <= lo {
        return 0;
    }
    let v = v.clamp(lo, hi);
    (((v - lo) as i64 * (size - 1) as i64) / (hi - lo) as i64) as i32
}

/// `EVIOCGABS(code)` — read an absolute axis's min/max.
fn abs_range(fd: RawFd, abs_code: u16) -> Option<(i32, i32)> {
    let mut info = [0i32; 6]; // struct input_absinfo = 6×i32
    let nr = 0x40u64 + abs_code as u64;
    let req: u64 = (2u64 << 30) | (24u64 << 16) | ((b'E' as u64) << 8) | nr;
    let rc = unsafe { libc::ioctl(fd, req as libc::c_ulong, info.as_mut_ptr()) };
    if rc < 0 {
        return None;
    }
    Some((info[1], info[2]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(x: (i32, i32), y: (i32, i32)) -> Touch {
        Touch { fd: -1, cur_slot: 0, axis: Axis::new(x, y) }
    }
    fn pen(x: (i32, i32), y: (i32, i32)) -> Pen {
        Pen { fd: -1, axis: Axis::new(x, y), in_range: false }
    }
    fn tags(evs: &[PointerEvent]) -> Vec<Phase> {
        evs.iter().map(|e| e.phase).collect()
    }

    #[test]
    fn touch_tap_emits_down_up() {
        let mut t = touch((0, 1000), (0, 1000));
        let mut out = Vec::new();
        for (ty, c, v) in [
            (EV_ABS, ABS_MT_TRACKING_ID, 7),
            (EV_ABS, ABS_MT_POSITION_X, 500),
            (EV_ABS, ABS_MT_POSITION_Y, 500),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_MT_TRACKING_ID, -1),
            (EV_SYN, SYN_REPORT, 0),
        ] {
            t.feed(ty, c, v, &mut out);
        }
        assert_eq!(tags(&out), vec![Phase::Down, Phase::Up]);
        assert_eq!((out[0].x, out[0].y), (476, 847));
    }

    #[test]
    fn pen_stroke_emits_down_move_up() {
        let mut p = pen((0, 2000), (0, 2000));
        let mut out = Vec::new();
        for (ty, c, v) in [
            (EV_KEY, BTN_TOUCH, 1),
            (EV_ABS, ABS_X, 100),
            (EV_ABS, ABS_Y, 100),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_X, 400),
            (EV_ABS, ABS_Y, 400),
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_SYN, SYN_REPORT, 0),
        ] {
            p.feed(ty, c, v, &mut out);
        }
        assert_eq!(tags(&out), vec![Phase::Down, Phase::Move, Phase::Up]);
    }

    #[test]
    fn pen_hover_without_touch_emits_nothing() {
        // Position updates with no BTN_TOUCH (hovering) must not select.
        let mut p = pen((0, 2000), (0, 2000));
        let mut out = Vec::new();
        for (ty, c, v) in [(EV_ABS, ABS_X, 100), (EV_ABS, ABS_Y, 100), (EV_SYN, SYN_REPORT, 0)] {
            p.feed(ty, c, v, &mut out);
        }
        assert!(out.is_empty());
    }

    #[test]
    fn pen_jitter_inside_the_deadband_is_suppressed() {
        // Raw range == screen range so raw units map 1:1. A 1px wiggle emits nothing; a real move does.
        let mut p = pen((0, FB_W - 1), (0, FB_H - 1));
        let mut out = Vec::new();
        for (ty, c, v) in [
            (EV_KEY, BTN_TOUCH, 1),
            (EV_ABS, ABS_X, 500),
            (EV_ABS, ABS_Y, 500),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_X, 501), // 1px — jitter
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_Y, 499), // 1px — jitter
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_X, 520), // real travel
            (EV_SYN, SYN_REPORT, 0),
        ] {
            p.feed(ty, c, v, &mut out);
        }
        assert_eq!(tags(&out), vec![Phase::Down, Phase::Move]);
        assert_eq!((out[1].x, out[1].y), (520, 499));
    }

    #[test]
    fn pen_up_emits_at_the_exact_final_position_even_after_suppressed_moves() {
        let mut p = pen((0, FB_W - 1), (0, FB_H - 1));
        let mut out = Vec::new();
        for (ty, c, v) in [
            (EV_KEY, BTN_TOUCH, 1),
            (EV_ABS, ABS_X, 500),
            (EV_ABS, ABS_Y, 500),
            (EV_SYN, SYN_REPORT, 0),
            (EV_ABS, ABS_X, 501), // suppressed
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, BTN_TOUCH, 0),
            (EV_SYN, SYN_REPORT, 0),
        ] {
            p.feed(ty, c, v, &mut out);
        }
        assert_eq!(tags(&out), vec![Phase::Down, Phase::Up]);
        assert_eq!((out[1].x, out[1].y), (501, 500), "Up bypasses the dead-band");
    }

    #[test]
    fn btn_touch_before_any_position_defers_the_down_to_the_first_real_coords() {
        let mut p = pen((0, 2000), (0, 2000));
        let mut out = Vec::new();
        // Contact with no coordinates yet: nothing (a Down at raw (0,0) would anchor a bogus stroke).
        for (ty, c, v) in [(EV_KEY, BTN_TOUCH, 1), (EV_SYN, SYN_REPORT, 0)] {
            p.feed(ty, c, v, &mut out);
        }
        assert!(out.is_empty(), "no Down before the first position report");
        // The first frame carrying real coordinates emits the deferred Down there.
        for (ty, c, v) in [(EV_ABS, ABS_X, 1000), (EV_ABS, ABS_Y, 1000), (EV_SYN, SYN_REPORT, 0)] {
            p.feed(ty, c, v, &mut out);
        }
        assert_eq!(tags(&out), vec![Phase::Down]);
        assert_eq!((out[0].x, out[0].y), (476, 847));
    }

    #[test]
    fn a_contact_that_ends_without_coordinates_is_dropped() {
        let mut p = pen((0, 2000), (0, 2000));
        let mut out = Vec::new();
        for (ty, c, v) in [(EV_KEY, BTN_TOUCH, 1), (EV_SYN, SYN_REPORT, 0), (EV_KEY, BTN_TOUCH, 0), (EV_SYN, SYN_REPORT, 0)] {
            p.feed(ty, c, v, &mut out);
        }
        assert!(out.is_empty(), "a phantom contact emits nothing");
        // And it doesn't leave a latched Down behind: a later hover frame stays silent.
        for (ty, c, v) in [(EV_ABS, ABS_X, 800), (EV_ABS, ABS_Y, 800), (EV_SYN, SYN_REPORT, 0)] {
            p.feed(ty, c, v, &mut out);
        }
        assert!(out.is_empty());
    }

    #[test]
    fn btn_tool_pen_tracks_hover_range() {
        let mut p = pen((0, 2000), (0, 2000));
        let mut out = Vec::new();
        assert!(!p.in_range());
        p.feed(EV_KEY, BTN_TOOL_PEN, 1, &mut out);
        assert!(p.in_range());
        p.feed(EV_KEY, BTN_TOOL_PEN, 0, &mut out);
        assert!(!p.in_range());
        assert!(out.is_empty(), "hover range never emits pointer events");
    }
}
