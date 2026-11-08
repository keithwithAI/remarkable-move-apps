//! `browserapp` — a full-screen web browser for reading Kindle Cloud Reader on the reMarkable
//! Paper Pro Move. Embeds WPE WebKit (the C `wpeengine` shim) rendering off-screen to SHM at the
//! native panel size, blits frames into the qtfb framebuffer, and forwards evdev touch straight to
//! the page (Cloud Reader's own tap/swipe zones turn pages). Cookies persist so the Amazon login
//! survives restarts.
//!
//! Device usage: launched by AppLoad via deploy/appload/browser/external.manifest.json.
//!   browserapp [--url <start-url>] [--data-dir <dir>] [--config <config.toml>]
//!
//! Requires the WPE runtime env (see /home/root/wpe-test/env.sh + the glvnd EGL vendor json) —
//! the AppLoad wrapper sources it before exec.

#[cfg(target_os = "linux")]
mod engine;
#[cfg(target_os = "linux")]
mod evdev;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("browserapp embeds WPE WebKit and runs on the reMarkable device only.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::engine::{keysym, Engine, Touch as Phase};
    use crate::evdev::{Pen, Phase as EvPhase, Touch, TouchEvent};
    use anyhow::{Context, Result};
    use image::{Rgb, RgbImage};
    use qtfb::{Connection, RefreshMode};
    use render::draw::{draw_text, fill_rect, hline, measure};
    use render::ui::{chip, draw_keyboard, even_slots, icon_btn, Icon, KbLayer, KeyCap, Rect, KB_H};
    use render::Fonts;
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Default start page: the library directly (the bare host just redirects here — one hop saved).
    /// When a last-book URL has been persisted, startup prefers that (see `pick_start_url`).
    const DEFAULT_URL: &str = "https://read.amazon.co.uk/kindle-library";
    const DEFAULT_DATA_DIR: &str = "/home/root/secondbrain/browser";

    /// Bottom action bar strip height — the reader app's FOOTER_H, so the bars match across apps.
    /// The engine renders ABOVE this strip (954×1600 → logical 800 at scale 2, both integral), so
    /// the web page — including Kindle's own tap-to-reveal chrome and its page footer — is never
    /// covered by our UI. Kindle owns the reading experience; the bar carries only what Kindle
    /// can't do (reload the web app, exit to AppLoad).
    const BAR_H: i32 = 96;
    /// Bar layout constants mirrored from the reader (`crates/reader/src/view.rs`): content margin
    /// and the square tap-cell the icons centre in.
    const MARGIN: i32 = 44;
    const ICELL: i32 = 60;
    const PAPER: Rgb<u8> = Rgb([255, 255, 255]);
    const INK: Rgb<u8> = Rgb([30, 30, 30]);
    const FAINT: Rgb<u8> = Rgb([180, 180, 180]);
    const MUTED: Rgb<u8> = Rgb([120, 120, 120]);

    /// Loading screen: shown from launch (and ↺ reload) until the page shows REAL content — the
    /// network progress hits 100 early in the boot (Kindle keeps booting via JS), so the hide
    /// trigger is content-based: ink coverage of the LATEST frame past this threshold (per-mille).
    /// Kindle's blank shell/spinner stays under it; a book page (or the login form) clears it.
    /// A sparse text page at native density sits well under the first cut's 50‰ — combined with
    /// the gate only being evaluated on frame arrival, that made every cold boot ride the 45s
    /// backstop (see `boot_gate`, which is now checked every tick against the cached latest frame).
    const BOOT_INK_PERMILLE: u32 = 15;
    /// Fallback arm: a STABLE frame with at least some ink also counts as booted — covers a very
    /// light page (big margins, sparse text) that never reaches BOOT_INK_PERMILLE.
    const BOOT_INK_MIN: u32 = 8;
    /// How long the latest frame must have held still for the low-ink fallback arm to trust it.
    const BOOT_STABLE: Duration = Duration::from_secs(2);
    /// Third gate arm: a page that finished loading and has held STILL this long is the page,
    /// however sparse its ink (chapter-break pages sit below BOOT_INK_MIN forever — the observed
    /// stuck-on-splash-for-45s case). Failed navigations never reach this arm (load_failed
    /// branch retries instead), so the blank error page can't ride it in.
    const BOOT_SETTLED_MIN: Duration = Duration::from_secs(3);
    const BOOT_SETTLED_STILL: Duration = Duration::from_secs(3);
    /// Never trust a frame as "booted" in the first moment after a (re)load starts — the previous
    /// page's inky frame can still be in flight while load-progress hasn't dropped yet.
    const BOOT_MIN_ELAPSED: Duration = Duration::from_secs(1);
    /// Backstop: whatever the page looks like, stop covering it after this long.
    const LOADING_TIMEOUT: Duration = Duration::from_secs(25);
    /// The chrome-dismiss tap fires BEHIND the splash the moment the boot gate passes; the reveal
    /// waits until the tap's repaints settle (bounded by this) so the ONE full refresh lands
    /// directly on the clean, chrome-free page — one flash instead of two.
    const BOOT_TAP_SETTLE: Duration = Duration::from_secs(2);
    /// Stillness required at the boot reveal — Kindle kept painting ~1.4s past the old 300ms
    /// settle, adding a straggler full refresh right after the reveal.
    const BOOT_REVEAL_STILL: Duration = Duration::from_millis(1200);
    /// Gap between load retries while the navigation keeps failing (offline at wake — the
    /// splash shows "No connection — retrying…" and self-heals once WiFi is back).
    const BOOT_RETRY_INTERVAL: Duration = Duration::from_secs(5);
    /// Focus mode's deterministic exit: a finger tap in the bottom strip (where the bar lives
    /// when visible) leaves focus. In immersive reading Kindle shows only non-interactive
    /// progress text there, so the strip costs nothing.
    const FOCUS_EXIT_STRIP: i32 = 72;

    /// We present while the page keeps changing (a load renders progressively) and go **completely
    /// quiet** once it has been stable for this long — no more framebuffer writes, even across
    /// suspend. Content-driven, not a fixed timer, so a slow SPA (Kindle Cloud Reader renders its
    /// library/book via JS well after the network load "finishes") is shown as it paints instead of
    /// freezing on an intermediate loading frame.
    const IDLE_GRACE: Duration = Duration::from_millis(1500);
    /// Hard cap on continuous activity since the last user input — the backstop against runaway
    /// refresh. Even if a page repaints forever (a JS animation the anti-animation CSS didn't catch),
    /// we go quiet this long after the last tap/keystroke; the next interaction re-engages.
    const MAX_ACTIVE: Duration = Duration::from_secs(20);
    /// Right after a **keystroke** we present every new frame for this long, regardless of the
    /// frame-hash — a single typed glyph can be too small for the sparse hash to notice, and the user
    /// must always see their keypress echo immediately. Keyed off `last_key` (a real keystroke), NOT
    /// `last_input` — a page-turn *tap* must not trigger per-frame echo presents (that is what made a
    /// single page turn flash 6–7 times). Outside this window the settle logic takes over.
    const INPUT_ECHO: Duration = Duration::from_millis(700);
    /// Coalesces a rapid typing burst — the only pacing floor now that presents are settle-driven, not
    /// per-frame. Settle (≥SETTLE_DELAY) and the heartbeat (≥PROGRESS_INTERVAL) both clear this easily,
    /// so it only ever gates back-to-back keystrokes.
    const MIN_PRESENT_GAP: Duration = Duration::from_millis(120);
    /// The browser polls WPE's frames, so a Kindle page turn arrives as a burst of intermediate frames
    /// over ~1 s. We suppress those and present the page **once**, this long after it stops changing —
    /// one crisp refresh per page turn instead of one per intermediate frame.
    const SETTLE_DELAY: Duration = Duration::from_millis(300);
    /// A long JS load (the library painting over several seconds) keeps changing without ever settling;
    /// present a progress frame at least this often so it never looks frozen on one intermediate frame.
    /// A page turn settles well before this fires, so it never adds a page-turn flash.
    const PROGRESS_INTERVAL: Duration = Duration::from_millis(1500);
    /// Whole-screen refreshes between deep (de-ghosting) `request_full_refresh` passes — clears
    /// accumulated residue so text stays clean over a long session. Mirrors reader-app's cadence, so a
    /// deep de-ghost lands roughly every 8 page turns.
    const FULLS_BETWEEN_DEEP: u32 = 8;
    /// A change taller than this fraction of the screen refreshes the whole panel; anything smaller
    /// (a keystroke into a field, a toggled key) refreshes just its rectangle.
    const FULL_REFRESH_ROWS_NUM: i32 = 3;
    const FULL_REFRESH_ROWS_DEN: i32 = 5;
    /// Palm rejection: while the nib hovers within EMR range, and for this long after it lifts,
    /// finger Down/Move are ignored (a resting palm must not page or scroll). Reader-app's value.
    const PEN_TOUCH_COOLDOWN: Duration = Duration::from_millis(400);

    /// Pen = highlighter, mirroring the reader app: nib down + drag = highlight, no stillness
    /// required. Kindle's web reader only starts selection from a stationary long-press, so the
    /// gate SYNTHESIZES one — the Down is pinned at the contact point while Kindle's timer runs,
    /// then the live drag streams. A pen contact can therefore never complete as a click (no page
    /// turns): grazes shorter than this vanish entirely, and everything longer goes through the
    /// long-press synthesis.
    const PEN_GRAZE_MAX: Duration = Duration::from_millis(180);
    /// How long the synthesized touch stays pinned at its anchor before the drag streams — must
    /// outlast Kindle's own long-press timer (~500-600ms of JS time on this CPU).
    const PEN_ANCHOR: Duration = Duration::from_millis(700);
    /// Nib wobble tolerated while dwelling (screen px); moving further classifies the gesture as a
    /// swipe, which is swallowed whole (a pen swipe must not turn pages).
    const PEN_HOLD_SLOP: i32 = 12;
    /// After a forwarded pen gesture lifts (a highlight — Kindle's colour menu is now up), the pen
    /// acts as a full finger for this long so the colour/Note taps work naturally with the nib.
    const PEN_GRACE: Duration = Duration::from_secs(6);

    /// Which device produced a pointer event — pen gestures bound for the page go through
    /// [`PenGate`]; finger events never do.
    #[derive(Clone, Copy, PartialEq)]
    enum Src {
        Finger,
        Pen,
    }

    enum GateState {
        Idle,
        /// Brand-new contact, nothing forwarded yet — a graze-quick lift vanishes entirely.
        TapBuffer { started: Instant, x0: i32, y0: i32 },
        /// Down(anchor) has been forwarded and is held stationary while Kindle's long-press timer
        /// runs; the nib's real position is tracked in (cx, cy) but not sent yet.
        Anchoring { anchor_until: Instant, cx: i32, cy: i32 },
        /// Selection engaged — the drag streams live (also the grace-window passthrough state).
        Streaming,
        /// The nib lifted while still Anchoring: the release is queued until the long-press has
        /// certainly fired, then Move(final) + Up complete the (short) highlight — never a click.
        DelayingUp { release_at: Instant, x: i32, y: i32 },
        ReleasingUp { x: i32, y: i32 },
    }

    /// The pen-as-highlighter gate, mirroring the reader app's stylus: touch text and drag — the
    /// selection follows. Kindle's touch UI needs a stationary long-press to enter selection mode,
    /// so the gate synthesizes it (anchor-pin, then stream). Emits zero or more `(phase, x, y)`
    /// events per input into `out`; pure state machine — `now` is injected, tests drive a fake
    /// clock. A completed gesture arms a grace window during which the pen passes unfiltered
    /// (Kindle's colour menu is up — nib-tap the colour).
    struct PenGate {
        state: GateState,
        grace_until: Option<Instant>,
    }

    impl PenGate {
        fn new() -> PenGate {
            PenGate { state: GateState::Idle, grace_until: None }
        }
        /// A contact (or queued synthetic release) is owned by the gate.
        fn engaged(&self) -> bool {
            !matches!(self.state, GateState::Idle)
        }
        fn in_grace(&self, now: Instant) -> bool {
            self.grace_until.is_some_and(|t| now < t)
        }
        /// A queued synthetic release must complete before any new contact opens — the C shim
        /// multiplexes ONE touch id, so streams must never interleave. Arms grace as usual.
        fn flush_pending_up(&mut self, now: Instant, out: &mut Vec<(Phase, i32, i32)>) {
            match self.state {
                GateState::DelayingUp { x, y, .. } => {
                    out.push((Phase::Move, x, y));
                    out.push((Phase::Up, x, y));
                }
                GateState::ReleasingUp { x, y } => out.push((Phase::Up, x, y)),
                _ => return,
            }
            self.state = GateState::Idle;
            self.grace_until = Some(now + PEN_GRACE);
        }
        fn on_down(&mut self, now: Instant, x: i32, y: i32, out: &mut Vec<(Phase, i32, i32)>) {
            if self.in_grace(now) {
                // Kindle's menu is up: the pen is a full finger, taps land naturally.
                self.state = GateState::Streaming;
                out.push((Phase::Down, x, y));
            } else {
                self.state = GateState::TapBuffer { started: now, x0: x, y0: y };
            }
        }
        fn on_move(&mut self, now: Instant, x: i32, y: i32, out: &mut Vec<(Phase, i32, i32)>) {
            if let GateState::Anchoring { cx, cy, .. } = &mut self.state {
                *cx = x; // tracked silently; sent when the anchor releases
                *cy = y;
                return;
            }
            match self.state {
                GateState::TapBuffer { x0, y0, .. } => {
                    if (x - x0).abs().max((y - y0).abs()) > PEN_HOLD_SLOP {
                        // The drag has begun: pin the touch at the contact point so Kindle's
                        // long-press engages there; the live drag streams once it has.
                        out.push((Phase::Down, x0, y0));
                        self.state =
                            GateState::Anchoring { anchor_until: now + PEN_ANCHOR, cx: x, cy: y };
                        log::debug!("pen gate: drag — anchoring at ({x0},{y0})");
                    }
                }
                GateState::Streaming => out.push((Phase::Move, x, y)),
                _ => {}
            }
        }
        fn on_up(&mut self, now: Instant, x: i32, y: i32, out: &mut Vec<(Phase, i32, i32)>) {
            match self.state {
                GateState::TapBuffer { .. } => {
                    self.state = GateState::Idle; // a graze — vanishes entirely
                    log::debug!("pen gate: graze swallowed");
                }
                GateState::Anchoring { anchor_until, .. } => {
                    // Lifted before selection engaged: queue the release for after it has, so the
                    // gesture completes as a (short) highlight and can never be a click.
                    self.state = GateState::DelayingUp { release_at: anchor_until, x, y };
                }
                GateState::Streaming => {
                    self.state = GateState::Idle;
                    self.grace_until = Some(now + PEN_GRACE);
                    out.push((Phase::Up, x, y));
                }
                _ => {} // Ups can't arrive in DelayingUp/ReleasingUp/Idle; never drop a queued release
            }
        }
        /// Time-driven transitions; called every loop iteration (~33ms while active), not only on
        /// events. Emits the anchor Down (press past the graze window), the catch-up Move when the
        /// anchor releases into streaming, and the queued Move/Up of an early lift.
        fn tick(&mut self, now: Instant, out: &mut Vec<(Phase, i32, i32)>) {
            match self.state {
                GateState::TapBuffer { started, x0, y0 } => {
                    if now.duration_since(started) >= PEN_GRAZE_MAX {
                        // Held past a graze: a press — anchor it (press-and-hold selects the word
                        // under the nib, exactly like a finger long-press).
                        out.push((Phase::Down, x0, y0));
                        self.state =
                            GateState::Anchoring { anchor_until: now + PEN_ANCHOR, cx: x0, cy: y0 };
                    }
                }
                GateState::Anchoring { anchor_until, cx, cy } => {
                    if now >= anchor_until {
                        // Selection has engaged at the anchor — catch the touch up to the nib.
                        out.push((Phase::Move, cx, cy));
                        self.state = GateState::Streaming;
                    }
                }
                GateState::DelayingUp { release_at, x, y } => {
                    if now >= release_at {
                        out.push((Phase::Move, x, y));
                        self.state = GateState::ReleasingUp { x, y };
                    }
                }
                GateState::ReleasingUp { x, y } => {
                    // One tick (~33ms) after the Move — realistic spacing for the final Up.
                    self.state = GateState::Idle;
                    self.grace_until = Some(now + PEN_GRACE);
                    out.push((Phase::Up, x, y));
                }
                _ => {}
            }
        }
    }

    /// Scroll the focused field above the on-screen keyboard so typing is visible.
    const KB_SCROLL_JS: &str = "(function(){var e=document.activeElement;\
                                if(e&&e.scrollIntoView)e.scrollIntoView({block:'center'});})();";

    /// Diagnostic JS (BROWSER_PROBE=1, run after each settle): logs the density the page actually
    /// renders at — devicePixelRatio, layout viewport, and each canvas's backing size vs CSS size
    /// (Kindle draws book pages into canvases; backing < CSS×DPR means it's being upscaled),
    /// walking same-origin iframes. Pair with BROWSER_CONSOLE=1 to land it in the wrapper's log.
    const PROBE_JS: &str = "(function(){try{var o=['dpr='+window.devicePixelRatio,'vp='+innerWidth+'x'+innerHeight];function s(d,t){try{var c=d.querySelectorAll('canvas');o.push(t+':nc='+c.length);for(var i=0;i<c.length&&i<4;i++){var e=c[i],r=e.getBoundingClientRect();o.push(t+':c'+i+'='+e.width+'x'+e.height+'@'+Math.round(r.width)+'x'+Math.round(r.height));}var ov=[],all=d.querySelectorAll('*');for(var k=0;k<all.length&&ov.length<6;k++){var b=all[k].getBoundingClientRect();if(b.width>0&&b.width<innerWidth&&(b.right>innerWidth+1||b.left<-1)){var el=all[k];var cn=String(el.className&&el.className.baseVal!==undefined?'(svg)':el.className).split(' ')[0];var al=el.getAttribute&&(el.getAttribute('aria-label')||el.id)||'';ov.push(el.tagName+'.'+cn+(al?'['+al+']':'')+'@'+Math.round(b.left)+'..'+Math.round(b.right));}}if(ov.length)o.push(t+':ov='+ov.join(','));var f=d.querySelectorAll('iframe');o.push(t+':nf='+f.length);for(var j=0;j<f.length&&j<4;j++){try{if(f[j].contentDocument)s(f[j].contentDocument,t+'/f'+j);else o.push(t+'/f'+j+':xo');}catch(x){o.push(t+'/f'+j+':xo');}}}catch(x){o.push(t+':err');}}s(document,'top');console.log('[probe] '+o.join(' '));}catch(e){console.log('[probe] fail '+e);}})();";

    static QUIT: AtomicBool = AtomicBool::new(false);
    /// Screenshot request (data_dir/shot.png — exactly what we send to the panel). Triggered by
    /// `touch <data_dir>/shot.req`, checked (and consumed) every loop. A SIGUSR1 handler was tried
    /// first, but WebKit/JSC resets USR1 to default deep in startup and the signal then KILLS the
    /// process — a file request can't be clobbered by libraries.
    static SHOT: AtomicBool = AtomicBool::new(false);

    /// Which surface owns the in-flight touch contact. Latched at `Down` so a drag that wanders into
    /// the chrome/keyboard band still delivers its `Move`/`Up` to the surface that started it — in
    /// particular the web view always gets its `Up`, so a scroll can't leave the page stuck.
    #[derive(Clone, Copy, PartialEq)]
    enum Owner {
        None,
        WebView,
        Chrome,
        Keyboard,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Action {
        Exit,
        Reload,
        ToggleKeys,
        RotateLeft,
        RotateRight,
        /// Enter focus mode (bar hidden, full-height book) — the deterministic path that
        /// needs no Kindle-DOM detection. Exit = a tap in the bottom strip.
        Focus,
    }

    /// Screen orientation. The panel/framebuffer is always portrait (954×1696); in landscape the
    /// whole app works in USER space (1696×954, what the reader sees with the device turned) and
    /// two edge transforms do the rest: touch fb→user at ingress, canvas user→fb at egress.
    /// Everything between — engine, router, PenGate, keyboard, bar — is orientation-blind.
    /// `Landscape` and `LandscapeAlt` are the two 180°-apart holds; the accelerometer's tilt sign
    /// picks whichever keeps the content upright.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Orientation {
        Portrait,
        Landscape,
        LandscapeAlt,
    }

    impl Orientation {
        fn user_dims(self, fb_w: i32, fb_h: i32) -> (i32, i32) {
            match self {
                Orientation::Portrait => (fb_w, fb_h),
                Orientation::Landscape | Orientation::LandscapeAlt => (fb_h, fb_w),
            }
        }
    }

    /// fb→user for one point (touch ingress). `Landscape`: portrait-top on the user's right;
    /// `LandscapeAlt`: the opposite hold.
    fn user_from_fb(o: Orientation, uw: i32, uh: i32, x_fb: i32, y_fb: i32) -> (i32, i32) {
        match o {
            Orientation::Portrait => (x_fb, y_fb),
            Orientation::Landscape => (y_fb, uh - 1 - x_fb),
            Orientation::LandscapeAlt => (uw - 1 - y_fb, x_fb),
        }
    }

    /// user→fb for one point (inverse of [`user_from_fb`]). Production code uses `fb_rect_of` /
    /// `compose_fb`'s inlined mapping; this point form exists for the round-trip tests.
    #[cfg(test)]
    fn fb_from_user(o: Orientation, uw: i32, uh: i32, x_u: i32, y_u: i32) -> (i32, i32) {
        match o {
            Orientation::Portrait => (x_u, y_u),
            Orientation::Landscape => (uh - 1 - y_u, x_u),
            Orientation::LandscapeAlt => (y_u, uw - 1 - x_u),
        }
    }

    /// user-space rect → its fb-space bounding box (for partial refresh rects).
    fn fb_rect_of(
        o: Orientation, uw: i32, uh: i32, x: i32, y: i32, w: i32, h: i32,
    ) -> (i32, i32, i32, i32) {
        match o {
            Orientation::Portrait => (x, y, w, h),
            Orientation::Landscape => (uh - y - h, x, h, w),
            Orientation::LandscapeAlt => (y, uw - x - w, h, w),
        }
    }

    /// Compose the user-space canvas into the portrait framebuffer image: identity in portrait,
    /// a 90° rotation (either hand) in landscape. ~1.6M px per present — fine at e-ink cadence.
    fn compose_fb(canvas: &RgbImage, screen: &mut RgbImage, o: Orientation) {
        let src = canvas.as_raw();
        let (uw, uh) = (canvas.width() as usize, canvas.height() as usize);
        let fw = screen.width() as usize;
        let dst: &mut [u8] = screen;
        match o {
            Orientation::Portrait => dst.copy_from_slice(src),
            Orientation::Landscape => {
                // fb(x,y) ← user(y_fb, uh−1−x_fb)  [fw == uh]
                for y_fb in 0..uw {
                    let x_u = y_fb;
                    let row = &mut dst[y_fb * fw * 3..(y_fb + 1) * fw * 3];
                    for x_fb in 0..fw {
                        let y_u = uh - 1 - x_fb;
                        let s = (y_u * uw + x_u) * 3;
                        row[x_fb * 3..x_fb * 3 + 3].copy_from_slice(&src[s..s + 3]);
                    }
                }
            }
            Orientation::LandscapeAlt => {
                // fb(x,y) ← user(uw−1−y_fb, x_fb)
                for y_fb in 0..uw {
                    let x_u = uw - 1 - y_fb;
                    let row = &mut dst[y_fb * fw * 3..(y_fb + 1) * fw * 3];
                    for x_fb in 0..fw {
                        let y_u = x_fb;
                        let s = (y_u * uw + x_u) * 3;
                        row[x_fb * 3..x_fb * 3 + 3].copy_from_slice(&src[s..s + 3]);
                    }
                }
            }
        }
    }

    fn orientation_path(data_dir: &str) -> String {
        format!("{data_dir}/orient")
    }
    /// Orientation persists across daemon restarts — auto-rotate and the Rot control write it,
    /// boots honour it (unless the accelerometer says the device is clearly tilted otherwise).
    fn read_orientation(data_dir: &str) -> Orientation {
        match std::fs::read_to_string(orientation_path(data_dir)).as_deref().map(str::trim) {
            Ok("landscape") => Orientation::Landscape,
            Ok("landscape-alt") => Orientation::LandscapeAlt,
            _ => Orientation::Portrait,
        }
    }
    fn persist_orientation(data_dir: &str, o: Orientation) {
        let s = match o {
            Orientation::Portrait => "portrait",
            Orientation::Landscape => "landscape",
            Orientation::LandscapeAlt => "landscape-alt",
        };
        let path = orientation_path(data_dir);
        let tmp = format!("{path}.tmp");
        let _ = std::fs::write(&tmp, s).and_then(|_| std::fs::rename(&tmp, &path));
    }

    /// Minimum gap after a completed rotation before the next one — with the manual buttons this
    /// only needs to swallow an accidental double-tap (the masked phase already blocks re-entry
    /// while a rotate is in flight). Auto-rotate (and its accelerometer) is GONE: on this panel a
    /// rotation costs seconds, so it must be a deliberate act — the ⟲/⟳ bar buttons.
    const ROTATE_COOLDOWN: Duration = Duration::from_secs(2);

    /// Rotation is allowed only outside the cooldown window from the previous one.
    fn rotate_allowed(now: Instant, last_rotate: Option<Instant>) -> bool {
        last_rotate.map_or(true, |t| now.duration_since(t) >= ROTATE_COOLDOWN)
    }

    /// Where a rotate button lands from the current orientation: from Portrait the two buttons
    /// pick the landscape hand (⟳ right-turn vs ⟲ left-turn — the job the accelerometer's tilt
    /// sign used to do); from either landscape hand both buttons return to Portrait.
    fn rotate_dir(o: Orientation, right: bool) -> Orientation {
        match o {
            // Hand mapping calibrated ON-DEVICE (from device testing: "reverse left and right"): ⟳ feels like
            // turning the DEVICE clockwise, which needs the content rotated counter-clockwise on
            // the panel — the `LandscapeAlt` transform; ⟲ is the `Landscape` hand.
            Orientation::Portrait if right => Orientation::LandscapeAlt,
            Orientation::Portrait => Orientation::Landscape,
            Orientation::Landscape | Orientation::LandscapeAlt => Orientation::Portrait,
        }
    }

    /// Live rotate: how long to wait for the page to actually REFLOW to the new width before
    /// falling back to a silent reload behind the "Rotating…" card. WebKit resizing its buffer is
    /// not enough — Kindle's SPA can keep its old layout, which used to get presented as a
    /// landscape frame with portrait-width content (tiny and unusable).
    const ROTATE_REFLOW_TIMEOUT: Duration = Duration::from_secs(4);
    /// Rotate + reload still unverified after this long → restart into the persisted orientation
    /// (the wrapper reopens us cold — the pre-live-rotate fallback).
    const ROTATE_RELOAD_TIMEOUT: Duration = Duration::from_secs(30);
    /// Cadence of the layout-width JS probe while a rotate is masked.
    const ROTATE_PROBE_INTERVAL: Duration = Duration::from_millis(250);
    /// clientWidth may differ from the dispatched logical width by scrollbar/rounding slop.
    const ROTATE_WIDTH_TOL: i32 = 8;
    /// Stillness before the rotate reveal. Deliberately SHORT: waiting for Kindle to fully stop
    /// repainting held the reveal to 7s+ on-device (frames kept coming until ~5.6s) — instead the
    /// reveal fires fast once the width is verified, and the post-reveal calm window
    /// (ROTATE_CALM/ROTATE_CALM_STILL) coalesces the residual churn so it can't flash-train.
    /// There is deliberately no frame-ink fallback verifier: on-device it judged a stale frame
    /// "reflowed" (530‰) and revealed garbage — a mute JS probe rides into the reload fallback.
    const ROTATE_REVEAL_STILL: Duration = Duration::from_millis(800);
    /// Width verified but the page keeps repainting: reveal anyway by this point — the layout is
    /// correct, and residual churn beats an endless card.
    const ROTATE_LIVE_MAX: Duration = Duration::from_secs(10);
    /// Gap between the masked poke taps (and before the reveal after the last one) — long enough
    /// for Kindle to process the tap and paint, short enough to keep the whole rotate ~5s.
    const ROTATE_TAP_GAP: Duration = Duration::from_secs(1);
    /// After a rotate reveal, presents stay extra-calm for this long…
    const ROTATE_CALM: Duration = Duration::from_secs(6);
    /// …requiring this much stillness per present (vs the usual SETTLE_DELAY 300ms), so Kindle's
    /// post-reflow re-render passes coalesce into at most one follow-up refresh.
    const ROTATE_CALM_STILL: Duration = Duration::from_millis(1200);
    /// Focus (height-only) resizes settle in ONE repaint pass — tighter reveal stillness and a
    /// much shorter post-reveal calm than a real rotate. Measured: the first new-size frame
    /// arrives 0.9-1.7s after the resize; total transition lands ~1.5-2.5s.
    const FOCUS_REVEAL_STILL: Duration = Duration::from_millis(400);
    const FOCUS_CALM: Duration = Duration::from_secs(2);

    /// Fired right after a live resize — some SPAs only re-layout on an explicit resize event.
    const RESIZE_NUDGE_JS: &str =
        "window.dispatchEvent(new Event('resize'));window.dispatchEvent(new Event('orientationchange'));";

    // ---- Keyboard gating: the page-state probe ---------------------------------------------------
    //
    // One question: is the page's FOCUSED element itself a visible, enabled text input?
    // Result format "pp:<a>|<diag>" — the prefix keeps stale rotate-probe results from being
    // misread; the diag tail (active element as TAG@top..bottom) is ignored by the parser and
    // exists for the device log.
    //
    // The ACTIVE element only: the keyboard shows for a field the user is IN, never for Kindle's
    // invisible key-capture input (observed focused after Save and after resizes), and a merely
    // visible search box can't fool it. Walks into same-origin iframes via their activeElement.
    const PAGE_PROBE_JS: &str = "String((function(){try{var a=0;\
        function edok(n){var ty=(n.type||'').toLowerCase();\
        if(n.tagName==='INPUT'&&['hidden','button','checkbox','radio','submit','range','file','image','reset','color'].indexOf(ty)>=0)return false;\
        return !n.disabled;}\
        function act(d,dep){var e=d.activeElement;\
        if(e&&e.tagName==='IFRAME'&&dep<3){try{if(e.contentDocument)return act(e.contentDocument,dep+1);}catch(x){}return null;}return e;}\
        var e=act(document,0),dga='';\
        if(e&&e.tagName){var tn=e.tagName,\
        ed=tn==='TEXTAREA'||(tn==='INPUT'&&edok(e))||e.isContentEditable;\
        var r2=e.getBoundingClientRect();\
        if(ed&&edok(e)&&r2.width>1&&r2.height>1)a=1;\
        dga=tn+'@'+Math.round(r2.top)+'..'+Math.round(r2.bottom);}\
        return 'pp:'+a+'|'+dga;}catch(x){return 'pp:err';}})())";

    /// Parse a [`PAGE_PROBE_JS`] result: `Some(active_editable)`, `None` for anything else
    /// (a rotate-probe leftover, `pp:err`, garbage). The `|diag` tail is ignored.
    fn parse_page_probe(res: &str) -> Option<bool> {
        let body = res.trim().strip_prefix("pp:")?;
        let body = body.split('|').next()?;
        Some(body.trim().parse::<u32>().ok()? != 0)
    }

    /// Post-interaction wait before probing page state (DOM settle) — used by the tap.req/js.req
    /// harness hooks.
    const TAP_PROBE_DELAY: Duration = Duration::from_millis(400);
    /// Poll cadence for the editable-count probe while the keyboard is up.
    const KB_PROBE_INTERVAL: Duration = Duration::from_secs(1);
    /// Never auto-close the keyboard this soon after a keystroke — mid-typing DOM churn.
    const KB_TYPING_GRACE: Duration = Duration::from_secs(2);
    /// A gated keyboard raise waits at most this long for the probe's visible-editable answer
    /// before showing anyway (a probe-blind page must still get its keyboard).
    const KB_GATE_TIMEOUT: Duration = Duration::from_millis(1500);

    /// Undo [`kb_shift_js`] — the page back at its natural position.
    const KB_UNSHIFT_JS: &str = "document.documentElement.style.transform='';";

    /// Shift the page up (root transform) just enough that the focused field AND the button row
    /// under it (Save/Cancel, ~170 CSS px allowance) clear the on-screen keyboard; identity when
    /// nothing is covered. A root transform moves position:fixed dialogs too (they reparent to
    /// the transformed root) — plain scrolling can't move Kindle's fixed note dialog, which is
    /// exactly how the Save button ended up buried under the keyboard. WebKit hit-tests through
    /// the transform, so taps keep landing on what's visible.
    fn kb_shift_js(kb_css: i32) -> String {
        format!(
            "(function(){{var e=document.activeElement;var t=document.documentElement.style;\
             t.transition='none';if(!e||e===document.body){{t.transform='';return;}}\
             var r=e.getBoundingClientRect();\
             var need=Math.max(0,Math.min(Math.round(r.bottom)+170-(innerHeight-{kb_css}),{kb_css}));\
             t.transform=need>0?'translateY(-'+need+'px)':'';}})();"
        )
    }

    /// Everything a keyboard SHOW does, in one place: scroll the field into view, shift the page
    /// clear of the keyboard, re-arm the auto-close, and (in focus mode) request the bar back.
    #[allow(clippy::too_many_arguments)]
    fn show_keyboard(
        engine: &Engine, kb_css: i32, focus_mode: bool, kb_shown: &mut bool,
        kb_field_seen: &mut bool, kb_zero_streak: &mut u8, focus_want: &mut Option<bool>,
        force_redraw: &mut bool,
    ) {
        *kb_shown = true;
        *kb_field_seen = false;
        *kb_zero_streak = 0;
        engine.run_js(KB_SCROLL_JS);
        engine.run_js(&kb_shift_js(kb_css));
        if focus_mode {
            *focus_want = Some(false); // typing needs the bar + the ⌄
        }
        *force_redraw = true;
    }

    /// Rotate-verification probe: layout-viewport width, widest canvas (walking same-origin
    /// iframes — Kindle draws book pages into canvases, sometimes inside an iframe), and canvas
    /// count. Returns "vw,maxCanvasW,nCanvas" — parsed by [`rotate_probe_ok`].
    const ROTATE_PROBE_JS: &str = "String((function(){var w=document.documentElement.clientWidth,m=0,n=0;function s(d){var c=d.querySelectorAll('canvas');n+=c.length;for(var i=0;i<c.length;i++){var r=c[i].getBoundingClientRect();if(r.width>m)m=r.width;}var f=d.querySelectorAll('iframe');for(var j=0;j<f.length;j++){try{if(f[j].contentDocument)s(f[j].contentDocument);}catch(e){}}}s(document);return w+','+Math.round(m)+','+n;})())";

    /// Parse the rotate probe's "vw,maxCanvasW,nCanvas" and decide whether the page has genuinely
    /// reflowed to `want_lw` logical px: the layout viewport must match, AND — when the page draws
    /// into canvases (a Kindle book) — the widest canvas must span most of it (a stale portrait
    /// canvas letterboxed in a landscape viewport is exactly the bug this catches). `None` = an
    /// unparsable result (treated as no answer).
    fn rotate_probe_ok(probe: &str, want_lw: i32) -> Option<bool> {
        let mut it = probe.trim().split(',');
        let vw: i32 = it.next()?.trim().parse().ok()?;
        let maxc: i32 = it.next()?.trim().parse().ok()?;
        let nc: i32 = it.next()?.trim().parse().ok()?;
        let vw_ok = (vw - want_lw).abs() <= ROTATE_WIDTH_TOL;
        let canvas_ok = nc == 0 || maxc * 100 >= want_lw * 55;
        Some(vw_ok && canvas_ok)
    }

    /// The instant-feedback frame shown while WebKit reflows to the new orientation (~2 s): clean
    /// paper + a quiet centred label, in the NEW orientation, over the whole content area.
    fn draw_rotating(canvas: &mut RgbImage, fonts: &Fonts, uw: i32, uh: i32) {
        fill_rect(canvas, 0.0, 0.0, uw as f32, (uh - BAR_H) as f32, PAPER);
        let label = "Rotating…";
        let tw = measure(&fonts.sans, 34.0, label);
        draw_text(canvas, &fonts.sans, 34.0, (uw as f32 - tw) / 2.0, uh as f32 * 0.45, label, MUTED);
    }

    /// Why a session's inner loop ended: `Quit` = ⭲/SIGTERM (daemon exits fully); `Detach` = the
    /// framebuffer or attach client went away (app switch) — the daemon stays warm.
    enum End {
        Quit,
        Detach,
    }

    /// The device scale factor the view renders at. Native-density rendering (the grainy-text fix:
    /// content rasterizes at exact device scale, no bitmap upscaling) — the VALUE picks the CSS
    /// viewport width, 954/scale. 1.8 → 530 CSS px: wide enough for Kindle's full reader toolbar
    /// (at 2.0's 477 px its last two buttons clipped off the edge — the ⋮ menu was unreachable,
    /// verified on device), while text stays sharp at any exact device scale. WebKit rounds the
    /// buffer to 954×1600 despite 1600/1.8 being fractional (confirmed by the first-frame log).
    /// Override with BROWSER_SCALE / --scale; 1.0 restores the legacy 1:1 behaviour.
    const SCALE_DEFAULT: f64 = 1.8;

    /// Fixed full-page zoom for a given device scale. Native density (scale > 1) renders 1:1 — the
    /// sharp point (any other zoom re-scales Kindle's page bitmaps); text size is Kindle's own Aa
    /// menu's job now that its chrome is reachable. Scale 1 keeps the legacy enlarged layout.
    fn zoom_for(scale: f64) -> f64 {
        if scale > 1.01 {
            1.0
        } else {
            1.8
        }
    }

    /// How this process participates in the warm-daemon architecture. `Single` is the legacy
    /// one-process mode (and the wrapper's last-resort fallback). `Daemon` owns WebKit and stays
    /// resident across app opens; `Attach` is the tiny foreground client AppLoad watches — it hands
    /// its framebuffer key to the daemon and blocks until the daemon ends the session.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Single,
        Daemon,
        Attach,
    }

    /// The daemon exits after this long with no one attached — bounds the resident-memory cost of
    /// keeping the booted browser warm.
    const ATTACH_IDLE_EXIT: Duration = Duration::from_secs(6 * 3600);
    /// ⭲ held at least this long = a real quit (daemon exits, memory freed, updates apply on next
    /// start). A plain tap just leaves the app with the daemon warm — the fast-reopen default, so
    /// the button people naturally press does the thing they want.
    const EXIT_HOLD_QUIT: Duration = Duration::from_millis(700);

    struct Args {
        url: String,
        url_is_flag: bool, // --url given explicitly → never overridden by the last-book file
        data_dir: String,
        scale: f64,
        mode: Mode,
    }

    fn attach_sock_path(data_dir: &str) -> String {
        format!("{data_dir}/attach.sock")
    }

    /// Parse the attach handshake line: `attach <QTFB key>`.
    fn parse_attach_line(line: &str) -> Option<u32> {
        line.trim().strip_prefix("attach ")?.trim().parse().ok()
    }

    /// True for a URL worth reopening next launch — an open book in the Kindle web reader. Kindle
    /// is an SPA, so these arrive via notify::uri (pushState), not page loads. Matcher is loose
    /// on purpose (asin param or a reader path); `stored_url_ok` provides the safety floor.
    fn is_book_url(u: &str) -> bool {
        stored_url_ok(u) && (u.contains("asin=") || u.contains("ASIN=") || u.contains("/reader"))
    }

    /// A URL read back from disk must be https on an EXACT read.amazon host — the char after the
    /// host must terminate the authority, or `read.amazon.co.uk.evil.com` would pass a bare prefix
    /// check. The file is app-owned, but it's auto-navigated at every launch: never trust it blindly.
    fn stored_url_ok(u: &str) -> bool {
        if u.contains(char::is_whitespace) || u.len() >= 2000 {
            return false;
        }
        const HOSTS: [&str; 2] = ["https://read.amazon.co.uk", "https://read.amazon.com"];
        HOSTS.iter().any(|h| {
            u.strip_prefix(h).is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', '?', '#']))
        })
    }

    /// Startup URL: an explicit --url always wins; else the persisted last-book URL (Kindle's own
    /// sync then restores the furthest page); else the library.
    fn pick_start_url(args_url: &str, url_is_flag: bool, stored: Option<&str>) -> String {
        if url_is_flag {
            return args_url.to_string();
        }
        match stored {
            Some(s) if is_book_url(s) => s.to_string(),
            _ => args_url.to_string(),
        }
    }

    /// The attach client: the foreground process AppLoad launched and watches. Hands the session's
    /// framebuffer key to the warm daemon, then blocks. Our death (AppLoad switch-away kill) is the
    /// daemon's detach signal; the daemon closing the socket (⭲ quit) is our exit signal. Any error
    /// returns Err so the wrapper can fall back to spawning a daemon / legacy single-process mode.
    fn run_attach(args: &Args) -> Result<()> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        let key = qtfb::key_from_env().context("QTFB_KEY not set — launch via the AppLoad manifest")?;
        let mut s = UnixStream::connect(attach_sock_path(&args.data_dir))
            .context("no warm daemon (attach socket)")?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        writeln!(s, "attach {key}")?;
        let mut buf = [0u8; 8];
        let n = s.read(&mut buf).context("daemon handshake read")?;
        if &buf[..n] != b"ok\n" {
            anyhow::bail!("daemon refused attach");
        }
        log::info!("attached to warm daemon (key {key})");
        s.set_read_timeout(None)?;
        // Block until the daemon ends the session; tolerate EINTR, treat everything else as done.
        let mut b = [0u8; 8];
        loop {
            match s.read(&mut b) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        log::info!("daemon session ended — attach client exiting");
        Ok(())
    }

    /// Bind the daemon's attach listener, replacing any stale socket file from a dead daemon.
    fn bind_attach(data_dir: &str) -> Result<std::os::unix::net::UnixListener> {
        let path = attach_sock_path(data_dir);
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path).context("bind attach socket")?;
        l.set_nonblocking(true)?;
        Ok(l)
    }

    /// Wait for the next app open: accept + handshake on the attach socket. Returns `None` on the
    /// idle timeout (daemon should exit and free its memory) or when QUIT was signalled. Bad
    /// handshakes (a client that died queued in the backlog) are dropped and the wait continues.
    fn wait_attach(
        l: &std::os::unix::net::UnixListener, timeout: Duration,
    ) -> Option<(std::os::unix::net::UnixStream, u32)> {
        use std::io::{Read, Write};
        use std::os::unix::io::AsRawFd;
        let deadline = Instant::now() + timeout;
        loop {
            if QUIT.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return None;
            }
            // Poll in short chunks so SIGTERM/idle-exit stay responsive.
            let mut fds =
                [libc::pollfd { fd: l.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, 30_000) };
            if rc <= 0 {
                continue; // timeout chunk or EINTR — loop re-checks QUIT/deadline
            }
            let Ok((mut stream, _)) = l.accept() else { continue };
            // Handshake: one line, bounded wait — a dead queued client just gets dropped.
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n]);
            let Some(key) = parse_attach_line(&line) else {
                log::warn!("attach handshake rejected: {line:?}");
                continue;
            };
            if stream.write_all(b"ok\n").is_err() {
                continue;
            }
            let _ = stream.set_read_timeout(None);
            let _ = stream.set_nonblocking(true);
            return Some((stream, key));
        }
    }

    pub fn run() -> Result<()> {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
        let args = parse_args();
        if args.mode == Mode::Attach {
            return run_attach(&args);
        }
        install_signals();
        std::fs::create_dir_all(&args.data_dir).ok();
        // BROWSER_PROBE=1: after each settle, console.log DPR/canvas metrics from the page (pair
        // with BROWSER_CONSOLE=1 so it reaches the log) — remote diagnosis of rendering density.
        let probe = std::env::var_os("BROWSER_PROBE").is_some();
        // Screenshot request file (see SHOT): touching this saves shot.png next to it.
        let shot_req = format!("{}/shot.req", args.data_dir);
        // Remote-drive hook: write JavaScript into this file over SSH and the loop runs it in the
        // page (fire-and-forget) — synthesizing taps/clicks for headless on-device validation.
        let js_req = format!("{}/js.req", args.data_dir);
        // Remote-drive hook: "x y" in user-space px → a real engine-level tap (the same path as
        // the boot poke taps, which Kindle's gesture handling actually honours — JS-synthesized
        // events are ignored as untrusted).
        let tap_req = format!("{}/tap.req", args.data_dir);

        // Session bootstrap. Daemon: bind the attach socket and wait for the first client (the
        // wrapper attaches right after spawning us) — its key names the panel surface. Single
        // (legacy/fallback): the key is in our own environment.
        let listener =
            if args.mode == Mode::Daemon { Some(bind_attach(&args.data_dir)?) } else { None };
        let (mut conn, mut attach_stream) = match &listener {
            Some(l) => {
                let (s, key) = wait_attach(l, Duration::from_secs(60))
                    .context("no attach within 60s of daemon start")?;
                (Connection::connect(key, qtfb::FBFMT_RMPPM_RGB888, None)?, Some(s))
            }
            None => {
                let key = qtfb::key_from_env()
                    .context("QTFB_KEY not set — launch via the AppLoad manifest")?;
                (Connection::connect(key, qtfb::FBFMT_RMPPM_RGB888, None)?, None)
            }
        };
        let _ = conn.set_refresh_mode(RefreshMode::Ui);
        let (w, h) = (conn.width as i32, conn.height as i32);

        // Orientation: LAST USED, full stop — the ⟲/⟳ bar buttons are the only way it changes.
        // (An accelerometer auto-rotate shipped and was pulled: boot-time reads misfired and tilt
        // rotations surprised mid-read on a panel where each rotation costs seconds.) All layout
        // below is USER space; the fb stays portrait.
        let mut orient = read_orientation(&args.data_dir);
        let (mut uw, mut uh) = orient.user_dims(w, h);

        // The engine renders ABOVE the bottom bar strip (user-space bottom) — the web page,
        // including Kindle's own chrome and footer, is never covered by our UI. Landscape forces
        // scale 2.0: the engine width must divide the panel edge integrally (1696/2.0 = 848 CSS —
        // comfortably wider than Kindle's toolbar minimum), while portrait keeps the tuned 1.8.
        let mut view_w = uw as u32;
        let mut view_h = (uh - BAR_H) as u32;
        let mut scale = if orient == Orientation::Portrait { args.scale } else { 2.0 };
        log::info!(
            "qtfb {w}x{h}; {orient:?} user {uw}x{uh}; view {view_w}x{view_h} @ scale {scale}; zoom {}; loading {}",
            zoom_for(scale),
            args.url
        );

        let t_engine = Instant::now();
        let engine = Engine::new(view_w, view_h, scale, Some(&args.data_dir))
            .context("engine init (WPE/EGL) failed")?;
        log::info!("boot: engine created in {}ms", t_engine.elapsed().as_millis());
        engine.set_zoom(zoom_for(scale));
        // Fast start: reopen the last book directly (Kindle's own sync restores the page) unless
        // --url was explicit. The file holds one URL, written on settle whenever the book changes.
        let last_book_path = format!("{}/last-book-url", args.data_dir);
        let stored = std::fs::read_to_string(&last_book_path).ok().map(|s| s.trim().to_string());
        let start_url = pick_start_url(&args.url, args.url_is_flag, stored.as_deref());
        let mut last_saved_book = if start_url != args.url { start_url.clone() } else { String::new() };
        if !last_saved_book.is_empty() {
            log::info!("fast start → last book {start_url}");
        }
        engine.load_uri(&start_url);

        let fonts = Fonts::embedded();
        // `canvas` is USER space (everything draws here); `screen` is the portrait fb image the
        // diff/present pipeline consumes — compose_fb bridges them each present.
        let mut canvas = RgbImage::from_pixel(uw as u32, uh as u32, PAPER);
        let mut screen = RgbImage::from_pixel(w as u32, h as u32, PAPER);
        // Previously-presented RGB bytes, for the dirty-rectangle diff.
        let mut prev_screen = vec![0u8; (w * h * 3) as usize];
        let mut prev_valid = false;
        let mut frame = vec![0u8; (view_w * view_h * 4) as usize];
        let mut touch = Touch::open().ok();
        if touch.is_none() {
            log::warn!("evdev touch open failed — input disabled");
        }
        // The pen is a precise finger: its events run the same surface router, driving Kindle's own
        // hold→drag→pick-colour highlight flow with nib precision. Optional — finger still works.
        let mut pen = Pen::open().ok();
        if pen.is_none() {
            log::warn!("evdev pen open failed — stylus disabled");
        }
        let mut pen_up_at: Option<Instant> = None;
        // Pen = highlighter only, via PenGate. BROWSER_PEN_FULL=1 restores pen-as-finger.
        let pen_full = std::env::var_os("BROWSER_PEN_FULL").is_some();
        let mut pen_gate = PenGate::new();
        let mut pen_out: Vec<(Phase, i32, i32)> = Vec::new();

        // Bar/keyboard state. The keyboard is focus-driven (native feel): WebKit's input-method
        // notifications raise it when an editable is focused and drop it on blur; `last_focus`
        // tracks the edge so a manual ⌄ dismiss holds until the page re-requests it. BROWSER_KEYS=1
        // adds a manual "Keys" toggle in the bar — the fallback if a page never fires focus events.
        let mut kb_shown = false;
        let mut kb_layer = KbLayer::Letters;
        let mut kb_shift = false;
        let mut last_focus = false;
        let keys_env = std::env::var_os("BROWSER_KEYS").is_some();
        let mut chrome_hits: Vec<(Rect, Action)> = Vec::new();
        let mut kb_hits: Vec<(Rect, KeyCap)> = Vec::new();
        let mut touch_owner = Owner::None;

        // Refresh bookkeeping.
        let mut last_hash: u64 = 0;
        let mut content_dirty = false;
        let mut force_redraw = true; // draw the chrome immediately
        // The watch window is content-driven: `last_input` = last user action (launch counts, so the
        // initial load is watched), `last_change` = last time the rendered content actually changed,
        // `last_key` = last real keystroke (drives the typing-echo present, distinct from a page-turn tap).
        let now0 = Instant::now();
        let mut last_input = now0; // launch acts as an action → the first load gets watched
        let mut last_change = now0;
        let mut last_key = now0 - INPUT_ECHO; // no keystroke yet → outside the echo window
        let mut last_present = now0 - MIN_PRESENT_GAP;
        // Whole-screen refreshes since the last deep de-ghost (reader-app's periodic clean-up).
        let mut fulls_since_deep = 0u32;
        // Loading screen: covers the boot until the page shows real content (see boot_gate).
        let mut loading = true;
        let mut loading_started = now0;
        let mut frames_since_load = 0u32; // engine frames seen since this load began
        let mut last_ink: Option<u32> = None; // ink of the LATEST frame — the gate's cached input
        let mut boot_status = BootStatus::Connecting;
        let mut boot_reveal_at: Option<Instant> = None; // chrome-dismiss running behind the splash
        let mut boot_retry_at: Option<Instant> = None; // next offline-retry not-before time
        let mut prev_lp = -1i32; // last seen load-progress, for edge logging
        let mut last_boot_log = now0; // paces the once-a-second boot telemetry line
        // Telemetry for the active→quiet transition (one info line per settle).
        let mut was_watching = true;
        let mut active_started = now0;
        let mut presents_full = 0u32;
        let mut presents_partial = 0u32;
        // Warm-daemon session tracking: `just_attached` forces one blit of the retained frame so a
        // re-attach shows the live page immediately; `attached_at` times it for the log.
        let mut just_attached = false;
        let mut attached_at = now0;
        // ⭲ semantics: tap = leave warm (Detach), hold ≥ EXIT_HOLD_QUIT = real quit. The press time
        // is latched on Down over the button and judged on Up.
        let mut exit_pressed_at: Option<Instant> = None;
        let mut detach_requested = false;
        // Rotation (⟲/⟳ buttons only): an attempt enters the MASKED phase (`rot_masked_since`) —
        // only the "Rotating…" card is on screen while WebKit resizes AND the page verifiably
        // reflows to the new width (JS probe); a page that won't reflow is reloaded behind the
        // card, and only then do we give up and restart.
        let mut rot_masked_since: Option<Instant> = None;
        let mut rot_reloaded = false; // the behind-the-card reload has been fired
        let mut rot_new_frame = false; // ≥1 frame at the new size has arrived
        let mut rot_width_ok = false; // the page's layout verifiably spans the new width
        let mut rot_js_seq = 0u32; // last JS-result sequence consumed
        let mut rot_probe_at = now0; // next probe send time
        let mut rot_tap_at: Option<Instant> = None; // next poke-tap / reveal not-before time
        let mut rot_taps = 0u8; // masked poke taps fired this rotate (see the verification block)
        let mut rot_calm_until: Option<Instant> = None; // post-reveal churn-coalescing window
        // The reveal must blit the RETAINED frame: the newest frame was consumed during the mask,
        // so `frame_changed` is false on the reveal iteration — without this flag the canvas still
        // holds the card and the "reveal" re-presents it (observed stuck-on-card on-device).
        let mut rot_present_frame = false;
        let mut last_rotate: Option<Instant> = None;
        // The active mask is a FOCUS resize (height-only): no mask card (the held page is the
        // mask), no width probe (width unchanged by construction), tighter reveal + calm.
        let mut masked_focus = false;

        // Focus mode: bar hidden + the view resized to FULL height, so the strip becomes book
        // rows. Deterministic controls only: the 📖 bar button enters, a tap in the bottom strip
        // exits (and a rising keyboard exits — typing needs the bar). Kindle's own tap-to-reveal
        // chrome is its own business; the bar no longer follows it. The resize rides the rotate
        // machinery's masked settle on the fast (height-only) path.
        let mut focus_mode = false;
        let mut focus_want: Option<bool> = None; // an edge waiting for the apply block
        let mut page_probe_at: Option<Instant> = None; // next PAGE_PROBE_JS send (kb poll/gate)
        let mut page_js_seq = 0u32; // last js_result seq consumed by the page-probe reader
        // Keyboard auto-close: armed only once the probe has SEEN an editable this keyboard
        // session — a note editor the probe can't see (cross-origin) then never falsely closes it.
        let mut kb_field_seen = false;
        let mut kb_zero_streak = 0u8;
        // Keyboard-raise gate: a kb_focus rising edge doesn't show the keyboard until the probe
        // confirms a VISIBLE editable. Kindle focuses an invisible key-capture input (observed
        // after Save and after focus-mode resizes) — ungated, that popped the keyboard over
        // nothing and kicked focus mode straight back off. Holds the edge's Instant for timeout.
        let mut kb_gate: Option<Instant> = None;

        'outer: loop {
        let end = 'inner: loop {
            if QUIT.load(Ordering::Relaxed) {
                break 'inner End::Quit;
            }
            if detach_requested {
                break 'inner End::Detach; // ⭲ tap: leave the app, stay warm (reset on re-attach)
            }
            // Orientation change requested this iteration (a settled tilt).
            let mut rotate_to: Option<Orientation> = None;
            // ~30 Hz while active is ample for e-ink and halves the take_frame/hash churn vs 60 Hz;
            // 300 ms when idle keeps us near-dormant.
            let timeout = if watching_decision(last_input.elapsed(), last_change.elapsed()) {
                33
            } else {
                300
            };

            // Negative fds are legal in poll(2) — ignored, revents 0 — so absent devices just
            // leave their slot inert instead of needing a variable-length array.
            let mut fds = [
                libc::pollfd { fd: conn.raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd {
                    fd: touch.as_ref().map(|t| t.raw_fd()).unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: pen.as_ref().map(|p| p.raw_fd()).unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: attach_stream.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e.into());
                }
            }

            // Compositor socket: only watch for EOF (it closed us → the app was switched away).
            if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                match conn.poll_msg(0) {
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        break 'inner End::Detach;
                    }
                    _ => {}
                }
            }

            // Attach client: its death is AppLoad tearing the app down — detach and stay warm.
            if fds[3].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                if let Some(s) = attach_stream.as_mut() {
                    use std::io::Read;
                    let mut b = [0u8; 16];
                    match s.read(&mut b) {
                        Ok(0) => break 'inner End::Detach,
                        Ok(_) => {}
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break 'inner End::Detach,
                    }
                }
            }

            // Input. A read error (e.g. a node vanishing across a suspend/re-enumerate) must not
            // kill the browser — log it, drop that device, and keep rendering. Finger and pen merge
            // into ONE pointer stream through the same surface router below; palm rejection drops
            // finger Down/Move while the pen is in play (in EMR range or just lifted), but lets a
            // finger Up through so a gesture that was live when the pen arrived can't leave the
            // page stuck mid-drag.
            let mut events: Vec<(Src, TouchEvent)> = Vec::new();
            if touch.is_some() && fds[1].revents & libc::POLLIN != 0 {
                match touch.as_mut().unwrap().poll_events() {
                    Ok(evs) => {
                        let pen_in_play = pen.as_ref().is_some_and(|p| p.in_range())
                            || pen_up_at.is_some_and(|t| t.elapsed() < PEN_TOUCH_COOLDOWN);
                        for mut e in evs {
                            if !pen_in_play || matches!(e.phase, EvPhase::Up) {
                                // Ingress transform: evdev is fb-space, everything downstream is
                                // user-space.
                                (e.x, e.y) = user_from_fb(orient, uw, uh, e.x, e.y);
                                events.push((Src::Finger, e));
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("touch read failed ({e}) — disabling finger input");
                        touch = None;
                    }
                }
            }
            if pen.is_some() && fds[2].revents & libc::POLLIN != 0 {
                match pen.as_mut().unwrap().poll_events() {
                    Ok(evs) => {
                        for mut e in evs {
                            if matches!(e.phase, EvPhase::Up) {
                                pen_up_at = Some(Instant::now());
                            }
                            (e.x, e.y) = user_from_fb(orient, uw, uh, e.x, e.y);
                            events.push((Src::Pen, e));
                        }
                    }
                    Err(e) => {
                        log::warn!("pen read failed ({e}) — disabling stylus");
                        pen = None;
                    }
                }
            }
            // Real input during the masked reveal window reveals immediately — the user is already
            // interacting with the page; keeping the splash up would hide their own action.
            if !events.is_empty() && boot_reveal_at.is_some() {
                boot_reveal_at = None;
                loading = false;
                force_redraw = true;
                content_dirty = true;
                fulls_since_deep = 0;
                log::info!("boot: user input during masked window — revealing now");
            }
            // Bottom (exclusive) of the page's touch region this iteration: full height in focus
            // mode (no bar), the bar's top edge otherwise.
            let page_bot = if focus_mode { uh } else { uh - BAR_H };
            for (src, ev) in events {
                // Pen = highlighter only: page-bound pen gestures divert through the gate (quick
                // taps and swipes suppressed; holds forward; grace after a highlight). Pen taps on
                // the bar/keyboard fall through to the shared arms and stay instant.
                if src == Src::Pen && !pen_full {
                    let to_web = match ev.phase {
                        EvPhase::Down => {
                            hit_chrome(&chrome_hits, ev.x, ev.y).is_none()
                                && !(kb_shown && ev.y >= uh - BAR_H - KB_H && ev.y < uh - BAR_H)
                                && ev.y < page_bot
                        }
                        // Moves/Ups belong to whoever owns the contact: the gate if it's engaged,
                        // else the shared arms (a pen press on the bar/keyboard).
                        _ => pen_gate.engaged(),
                    };
                    if to_web {
                        if rot_masked_since.is_some() {
                            continue; // reflow in flight — stale content, don't touch the page
                        }
                        let now = Instant::now();
                        pen_out.clear();
                        // A new contact while a synthesized release is still queued: flush it
                        // first so the stream WebKit sees stays balanced (grace arms as usual).
                        if matches!(ev.phase, EvPhase::Down) {
                            pen_gate.flush_pending_up(now, &mut pen_out);
                        }
                        match ev.phase {
                            EvPhase::Down => pen_gate.on_down(now, ev.x, ev.y, &mut pen_out),
                            EvPhase::Move => pen_gate.on_move(now, ev.x, ev.y, &mut pen_out),
                            EvPhase::Up => pen_gate.on_up(now, ev.x, ev.y, &mut pen_out),
                        }
                        for &(ph, x, y) in pen_out.iter() {
                            engine.touch(ph, x, y);
                        }
                        if !pen_out.is_empty() {
                            last_input = Instant::now();
                        }
                        continue; // consumed by the gate (forwarded or suppressed)
                    }
                }
                match ev.phase {
                    EvPhase::Down => {
                        // Latch which surface owns this contact for the whole gesture. The engine
                        // is top-aligned (engine row == screen row), so web-view pointers pass 1:1.
                        if let Some(act) = hit_chrome(&chrome_hits, ev.x, ev.y) {
                            touch_owner = Owner::Chrome;
                            match act {
                                Action::Exit => {
                                    // Judged on Up: tap = leave warm, hold = real quit.
                                    exit_pressed_at = Some(Instant::now());
                                }
                                Action::Reload => {
                                    engine.reload();
                                    // Reload gets the same boot experience as launch.
                                    focus_want = None;
                                    page_probe_at = None;
                                    kb_gate = None;
                                    engine.run_js(KB_UNSHIFT_JS);
                                    loading = true;
                                    loading_started = Instant::now();
                                    frames_since_load = 0;
                                    last_ink = None;
                                    boot_status = BootStatus::Connecting;
                                    boot_reveal_at = None;
                                    prev_lp = -1;
                                    force_redraw = true;
                                    last_input = Instant::now();
                                }
                                Action::ToggleKeys => {
                                    // ⌄ dismiss while shown; the BROWSER_KEYS fallback chip raises
                                    // it manually when hidden (explicit intent — no gate).
                                    if kb_shown {
                                        kb_shown = false;
                                        kb_gate = None;
                                        engine.run_js(KB_UNSHIFT_JS);
                                        force_redraw = true;
                                    } else {
                                        let kb_css = (KB_H as f64 / scale).round() as i32;
                                        show_keyboard(
                                            &engine, kb_css, focus_mode, &mut kb_shown,
                                            &mut kb_field_seen, &mut kb_zero_streak,
                                            &mut focus_want, &mut force_redraw,
                                        );
                                    }
                                    last_input = Instant::now();
                                }
                                Action::Focus => {
                                    // Deterministic focus entry — no DOM detection involved.
                                    if !loading && rot_masked_since.is_none() {
                                        focus_want = Some(true);
                                    }
                                    last_input = Instant::now();
                                }
                                Action::RotateLeft | Action::RotateRight => {
                                    // A deliberate rotate. Ignored mid-boot, mid-rotate, and for
                                    // ROTATE_COOLDOWN after one completes (double-tap guard).
                                    let now = Instant::now();
                                    if !loading
                                        && rot_masked_since.is_none()
                                        && rotate_allowed(now, last_rotate)
                                    {
                                        rotate_to = Some(rotate_dir(
                                            orient,
                                            act == Action::RotateRight,
                                        ));
                                    }
                                    last_input = now;
                                }
                            }
                        } else if kb_shown && ev.y >= uh - BAR_H - KB_H && ev.y < uh - BAR_H {
                            touch_owner = Owner::Keyboard;
                            if let Some(cap) = hit_key(&kb_hits, ev.x, ev.y) {
                                apply_key(&engine, cap, &mut kb_layer, &mut kb_shift, &mut force_redraw);
                                last_input = Instant::now();
                                last_key = Instant::now(); // a real keystroke → echo window
                            }
                        } else if focus_mode && ev.y >= uh - FOCUS_EXIT_STRIP {
                            // Focus mode's deterministic exit: tap where the bar used to be.
                            // Swallowed — never forwarded to the page.
                            touch_owner = Owner::Chrome;
                            if rot_masked_since.is_none() {
                                focus_want = Some(false);
                            }
                            last_input = Instant::now();
                        } else if ev.y < page_bot {
                            if rot_masked_since.is_some() {
                                // Reflow in flight — swallow page-bound touches (bar stays live).
                                touch_owner = Owner::Chrome;
                                continue;
                            }
                            // A queued synthetic pen release must complete before the finger's
                            // Down — the C shim multiplexes one touch id; never interleave.
                            if !pen_full {
                                pen_out.clear();
                                pen_gate.flush_pending_up(Instant::now(), &mut pen_out);
                                for &(ph, px, py) in pen_out.iter() {
                                    engine.touch(ph, px, py);
                                }
                            }
                            touch_owner = Owner::WebView;
                            engine.touch(Phase::Down, ev.x, ev.y);
                            last_input = Instant::now();
                        } else {
                            // Bar background (between slots): swallow — a stray tap in the strip
                            // must never reach the page.
                            touch_owner = Owner::Chrome;
                        }
                    }
                    EvPhase::Move => {
                        // Route to the gesture's owner, not the current position — a scroll that
                        // drifts over the chrome must keep scrolling the page.
                        if touch_owner == Owner::WebView {
                            engine.touch(Phase::Move, ev.x, ev.y);
                            last_input = Instant::now();
                        }
                    }
                    EvPhase::Up => {
                        if touch_owner == Owner::WebView {
                            // Always deliver the Up so the page never sticks mid-drag, even if the
                            // pointer lifted over the bar/keyboard band.
                            engine.touch(Phase::Up, ev.x, ev.y);
                            last_input = Instant::now();
                        }
                        if let Some(t) = exit_pressed_at.take() {
                            if t.elapsed() >= EXIT_HOLD_QUIT {
                                log::info!("exit held — full quit (daemon exits)");
                                QUIT.store(true, Ordering::Relaxed);
                            } else {
                                log::info!("exit tapped — leaving app, daemon stays warm");
                                detach_requested = true;
                            }
                        }
                        touch_owner = Owner::None;
                    }
                }
            }

            // Pen gate timers fire on time, not only on events: the graze buffer promotes to an
            // anchored press, the anchor releases into streaming once Kindle's long-press has
            // fired, and a queued early-lift release completes as Move + Up across ticks.
            if !pen_full {
                pen_out.clear();
                pen_gate.tick(Instant::now(), &mut pen_out);
                for &(ph, x, y) in pen_out.iter() {
                    engine.touch(ph, x, y);
                }
                if !pen_out.is_empty() {
                    last_input = Instant::now();
                }
            }

            // Apply an orientation change (a ⟲/⟳ bar tap). Persist first, then the LIVE path:
            // re-size/re-scale the engine and swap the whole user-space layout; the masked
            // verification below confirms the reflow or falls back to reload / restart.
            if let Some(next) = rotate_to.take() {
                if next != orient {
                    persist_orientation(&args.data_dir, next);
                    orient = next;
                    (uw, uh) = orient.user_dims(w, h);
                    scale = if orient == Orientation::Portrait { args.scale } else { 2.0 };
                    view_w = uw as u32;
                    view_h = (uh - BAR_H) as u32;
                    // A fresh orientation starts with the standard chrome — focus mode off.
                    focus_mode = false;
                    focus_want = None;
                    page_probe_at = None;
                    engine.resize(scale, view_w, view_h);
                    engine.set_zoom(zoom_for(scale));
                    frame = vec![0u8; (view_w * view_h * 4) as usize];
                    canvas = RgbImage::from_pixel(uw as u32, uh as u32, PAPER);
                    content_dirty = false;
                    // Keyboard re-sync goes through the gate: a still-focused REAL field re-raises
                    // it once the probe confirms; the invisible key-capture input doesn't.
                    kb_shown = false;
                    engine.run_js(KB_UNSHIFT_JS);
                    last_focus = engine.kb_focus();
                    kb_gate = if last_focus { Some(Instant::now()) } else { None };
                    pen_gate = PenGate::new();
                    touch_owner = Owner::None;
                    last_input = Instant::now();
                    last_change = Instant::now();
                    // Enter the MASKED phase: nothing but the card below reaches the screen until
                    // the page verifiably reflows to the new width. The cooldown (`last_rotate`)
                    // starts at REVEAL, so a double-tap mid-mask can't re-enter.
                    rot_masked_since = Some(Instant::now());
                    masked_focus = false; // a real rotate — full card + probe machinery
                    rot_reloaded = false;
                    rot_new_frame = false;
                    rot_width_ok = false;
                    rot_js_seq = engine.js_result().0;
                    rot_probe_at = Instant::now() + Duration::from_millis(300);
                    rot_tap_at = None;
                    rot_taps = 0;
                    // Some SPAs only re-layout on an explicit resize/orientation event.
                    engine.run_js(RESIZE_NUDGE_JS);
                    log::info!("rotate → {orient:?} live attempt (view {view_w}x{view_h} @ {scale})");
                    // Instant feedback: a clean new-orientation frame ("Rotating…" + the bar) on
                    // the flash-free waveform — no ~2s freeze on stale old-orientation content.
                    redraw_ui(
                        &mut canvas, &fonts, focus_mode, kb_shown, keys_env, kb_layer, kb_shift,
                        &mut chrome_hits, &mut kb_hits, uw, uh,
                    );
                    draw_rotating(&mut canvas, &fonts, uw, uh);
                    compose_fb(&canvas, &mut screen, orient);
                    conn.frame_mut().copy_from_slice(screen.as_raw());
                    let _ = conn.set_refresh_mode(RefreshMode::Fast);
                    if conn.update_all().is_err() {
                        break 'inner End::Detach;
                    }
                    let _ = conn.set_refresh_mode(RefreshMode::Ui);
                    prev_screen.copy_from_slice(screen.as_raw());
                    prev_valid = true;
                    force_redraw = true; // the reflowed page still repaints fully when it arrives
                }
            }

            // Apply a focus-mode edge (probe-driven, or keyboard-forced exit): resize the live
            // view to full height (focus on) or back above the bar (off), riding the rotate
            // machinery's masked settle so the reflow never flashes. The width is unchanged, so
            // the width probe verifies on its first answer — the mask is mostly the poke taps +
            // reveal-stillness window.
            if let Some(want) = focus_want {
                let now = Instant::now();
                // The keyboard rising while in focus can't wait out the cooldown — an undrawn
                // keyboard would swallow taps. Everything else respects the resize cooldown; a
                // pending edge stays queued (it targets a STATE, so applying late still lands
                // on whatever Kindle currently shows).
                let urgent = !want && kb_shown;
                if want == focus_mode {
                    focus_want = None; // already there (self-healed) — nothing to do
                } else if !loading
                    && rot_masked_since.is_none()
                    && (urgent || rotate_allowed(now, last_rotate))
                {
                    focus_want = None;
                    focus_mode = want;
                    view_h = (if focus_mode { uh } else { uh - BAR_H }) as u32;
                    engine.resize(scale, view_w, view_h);
                    frame = vec![0u8; (view_w * view_h * 4) as usize];
                    pen_gate = PenGate::new();
                    touch_owner = Owner::None;
                    last_input = now;
                    last_change = now;
                    rot_masked_since = Some(now);
                    masked_focus = true;
                    rot_reloaded = false;
                    rot_new_frame = false;
                    // Width is unchanged by construction — no JS probe needed; the reveal waits
                    // only for the first new-size frame + a short stillness.
                    rot_width_ok = true;
                    rot_js_seq = engine.js_result().0;
                    page_js_seq = rot_js_seq;
                    rot_tap_at = None;
                    // NO poke taps for focus transitions, ever: a poke TOGGLES Kindle's chrome
                    // (desyncing the very state focus mirrors) and can press random page
                    // elements (observed on-device: a poke focused something and popped the
                    // keyboard mid-mask). Height-only resizes repaint fine without them.
                    rot_taps = 2;
                    engine.run_js(KB_UNSHIFT_JS);
                    engine.run_js(RESIZE_NUDGE_JS);
                    // NO mask card and NO entry present: the held page (near-identical content)
                    // IS the mask — the whole transition is one full refresh at the reveal.
                    force_redraw = true;
                    log::info!(
                        "focus {} → view {view_w}x{view_h} (fast masked settle)",
                        if focus_mode { "on" } else { "off" }
                    );
                }
            }

            // Focus-driven keyboard (native feel): WebKit notifies when an editable gains/loses
            // focus. Act on EDGES only, so a manual ⌄ dismiss (kb_shown=false while the field stays
            // focused) holds until the page re-requests a keyboard. A RISING edge is GATED on the
            // probe confirming a visible editable (see kb_gate); a falling edge hides immediately.
            let focus = engine.kb_focus();
            if focus != last_focus {
                last_focus = focus;
                if focus {
                    if !kb_shown {
                        kb_gate = Some(Instant::now());
                        let due = Instant::now();
                        page_probe_at = Some(page_probe_at.map_or(due, |cur| cur.min(due)));
                        log::debug!("kb focus edge → gated (probing for a visible editable)");
                    }
                } else {
                    kb_gate = None;
                    if kb_shown {
                        kb_shown = false;
                        engine.run_js(KB_UNSHIFT_JS);
                        force_redraw = true;
                        last_input = Instant::now();
                        log::debug!("kb focus edge → keyboard hidden");
                    }
                }
            }
            // The gate can only be answered while the probe slot is ours — pause its clock during
            // masks/boot, and time out to "show anyway" on probe-blind pages.
            if rot_masked_since.is_some() || loading {
                if let Some(g) = kb_gate.as_mut() {
                    *g = Instant::now();
                }
            } else if kb_gate.is_some_and(|t| t.elapsed() >= KB_GATE_TIMEOUT) {
                kb_gate = None;
                if !kb_shown {
                    let kb_css = (KB_H as f64 / scale).round() as i32;
                    show_keyboard(
                        &engine, kb_css, focus_mode, &mut kb_shown, &mut kb_field_seen,
                        &mut kb_zero_streak, &mut focus_want, &mut force_redraw,
                    );
                    last_input = Instant::now();
                    log::debug!("kb gate timed out (no probe answer) — showing keyboard");
                }
            }

            // ---- Page-state probe: the keyboard follows the page's input area. Sends happen
            // while the keyboard is up (KB_PROBE_INTERVAL poll), while a gated raise awaits its
            // answer, and after harness interactions. Never touches the js_result slot while a
            // rotate/focus resize owns it (rot_masked_since).
            if rot_masked_since.is_none() && !loading {
                if (kb_shown || kb_gate.is_some()) && page_probe_at.is_none() {
                    page_probe_at = Some(Instant::now()); // keyboard poll / gate answer
                }
                if page_probe_at.is_some_and(|t| Instant::now() >= t) {
                    engine.eval_js(PAGE_PROBE_JS);
                    page_probe_at = kb_shown.then(|| Instant::now() + KB_PROBE_INTERVAL);
                }
                let (seq, res) = engine.js_result();
                if seq != page_js_seq {
                    page_js_seq = seq;
                    if let Some(active) = parse_page_probe(&res) {
                        log::debug!("page probe: active_editable={active} raw={res:?}");
                        // Resolve a gated keyboard raise: show only when the FOCUSED element is
                        // a real, visible field. Kindle's invisible key-capture input (focused
                        // after Save and after resizes) is exactly what this suppresses — and
                        // the toolbar search box can't fool it (it isn't the focused element).
                        if kb_gate.take().is_some() {
                            if active {
                                if !kb_shown {
                                    let kb_css = (KB_H as f64 / scale).round() as i32;
                                    show_keyboard(
                                        &engine, kb_css, focus_mode, &mut kb_shown,
                                        &mut kb_field_seen, &mut kb_zero_streak, &mut focus_want,
                                        &mut force_redraw,
                                    );
                                    last_input = Instant::now();
                                    log::debug!("kb gate passed → keyboard shown");
                                }
                            } else {
                                log::info!("keyboard suppressed (focused element not a visible editable)");
                            }
                        }
                        // Keyboard auto-close: the field the user was typing in is gone (note
                        // Saved, editor dismissed). Armed only after the probe has actually seen
                        // the active field, and never right after a keystroke.
                        if kb_shown {
                            if active {
                                kb_field_seen = true;
                                kb_zero_streak = 0;
                            } else if kb_field_seen && last_key.elapsed() >= KB_TYPING_GRACE {
                                kb_zero_streak += 1;
                                if kb_zero_streak >= 2 {
                                    kb_shown = false;
                                    kb_field_seen = false;
                                    kb_zero_streak = 0;
                                    kb_gate = None;
                                    engine.run_js(KB_UNSHIFT_JS);
                                    force_redraw = true;
                                    log::info!("keyboard auto-closed — the input field is gone");
                                }
                            }
                        }
                    }
                }
            }

            // Latest rendered frame → change detection. A genuine content change (hash differs) keeps
            // the content-stability watch window alive so a load is shown as it renders; during the
            // input-echo window we also present unconditionally (a typed glyph can be smaller than the
            // hash's sample stride).
            let frame_changed = engine.take_frame(&mut frame);
            if frame_changed {
                let hsh = frame_hash(&frame);
                if hsh != last_hash {
                    last_hash = hsh;
                    last_change = Instant::now();
                    content_dirty = true;
                } else if last_key.elapsed() < INPUT_ECHO {
                    // A typed glyph can be too small for the sparse hash — force an echo present while
                    // the keystroke is fresh. Keyed off last_key, so a page-turn tap doesn't echo.
                    content_dirty = true;
                }
                if loading {
                    // Boot audit: every frame's ink + progress, timestamped from load start. The
                    // cached ink is what boot_gate consumes each tick.
                    frames_since_load += 1;
                    let ink = frame_ink_permille(&frame);
                    last_ink = Some(ink);
                    log::debug!(
                        "boot: frame #{frames_since_load} t=+{}ms ink={ink}‰ lp={}",
                        loading_started.elapsed().as_millis(),
                        engine.load_progress()
                    );
                }
            }

            // Live-rotate verification (masked phase): a frame at the new BUFFER size is necessary
            // (the C gate guarantees the size) but not sufficient — the page must verifiably span
            // the new WIDTH, else we'd present the old bug (landscape frame, portrait-width
            // content). Probe the layout via JS; fall back to a frame-ink heuristic if JS is mute;
            // reload behind the card if the page won't reflow; restart as the last resort.
            if let Some(started) = rot_masked_since {
                if frame_changed {
                    rot_new_frame = true;
                }
                let now = Instant::now();
                // Focus resizes skip the whole width-probe machinery: the width can't have
                // changed, so `rot_width_ok` was preset true and the reveal waits only for the
                // first new-size frame + stillness.
                if !masked_focus {
                    if now >= rot_probe_at {
                        rot_probe_at = now + ROTATE_PROBE_INTERVAL;
                        engine.eval_js(ROTATE_PROBE_JS);
                    }
                    let want_lw = (view_w as f64 / scale).round() as i32;
                    let (seq, res) = engine.js_result();
                    if seq != rot_js_seq {
                        rot_js_seq = seq;
                        match rotate_probe_ok(&res, want_lw) {
                            Some(ok) => {
                                if ok != rot_width_ok {
                                    log::info!(
                                        "rotate: probe {res:?} (want vw {want_lw}) → {} t=+{}ms",
                                        if ok { "reflowed" } else { "NOT reflowed" },
                                        started.elapsed().as_millis()
                                    );
                                }
                                rot_width_ok = ok;
                            }
                            None => log::debug!("rotate: unparsable probe result {res:?}"),
                        }
                    }
                }
                // No frame-heuristic fallback: a mute JS probe rides into the reload fallback —
                // on-device the ink heuristic once judged a stale frame "reflowed" and revealed it.
                //
                // POKE SEQUENCE: after a live reflow Kindle paints a loading/blank view and then
                // IDLES — zero frames for 36s on-device until someone manually tapped the page,
                // whose chrome-toggle forced fresh paints of the real page. So we do that click
                // for him, masked: tap #1 raises Kindle's chrome, tap #2 dismisses it — each
                // forces a full fresh render, so the frame we reveal is the REAL page. The reload
                // path lands on the controls view instead, so ONE dismissing tap suffices there.
                let ready = rot_new_frame && rot_width_ok;
                let taps_wanted: u8 = if rot_reloaded { 1 } else { 2 };
                if ready
                    && rot_taps < taps_wanted
                    && rot_tap_at.map_or(true, |t| Instant::now() >= t)
                {
                    let (tx, ty) = (uw / 2, (uh - BAR_H) / 2);
                    engine.touch(Phase::Down, tx, ty);
                    engine.touch(Phase::Up, tx, ty);
                    last_input = Instant::now();
                    rot_taps += 1;
                    rot_tap_at = Some(Instant::now() + ROTATE_TAP_GAP);
                    log::info!(
                        "rotate: masked poke tap #{rot_taps} at ({tx},{ty}) t=+{}ms",
                        started.elapsed().as_millis()
                    );
                } else if ready
                    && rot_taps >= taps_wanted
                    && rot_tap_at.map_or(true, |t| Instant::now() >= t)
                    && (last_change.elapsed()
                        >= if masked_focus { FOCUS_REVEAL_STILL } else { ROTATE_REVEAL_STILL }
                        || started.elapsed() >= ROTATE_LIVE_MAX)
                {
                    // REVEAL: one full Ui refresh straight onto the finished page.
                    rot_masked_since = None;
                    rot_tap_at = None;
                    rot_calm_until =
                        Some(Instant::now() + if masked_focus { FOCUS_CALM } else { ROTATE_CALM });
                    rot_present_frame = true; // blit the retained frame — see the flag's comment
                    prev_valid = false;
                    force_redraw = true;
                    content_dirty = true;
                    fulls_since_deep = 0;
                    last_rotate = Some(Instant::now());
                    log::info!(
                        "{} complete in {}ms (path={})",
                        if masked_focus { "focus switch" } else { "rotate" },
                        started.elapsed().as_millis(),
                        if rot_reloaded { "reload" } else { "live" }
                    );
                    masked_focus = false;
                } else if !rot_reloaded && !rot_width_ok && started.elapsed() >= ROTATE_REFLOW_TIMEOUT {
                    // The page kept its old layout — reload it at the new size behind the card.
                    // (Gated on !rot_width_ok: a verified width just waiting out stillness must
                    // NOT be thrown away for a reload.)
                    rot_reloaded = true;
                    rot_new_frame = false;
                    rot_width_ok = false;
                    rot_taps = 0;
                    rot_tap_at = None;
                    let uri = engine.current_uri();
                    let target = if is_book_url(&uri) {
                        uri
                    } else if !last_saved_book.is_empty() {
                        last_saved_book.clone()
                    } else {
                        args.url.clone()
                    };
                    log::info!(
                        "rotate: page did not reflow in {}ms — reloading {target} behind the card",
                        started.elapsed().as_millis()
                    );
                    engine.load_uri(&target);
                    last_input = Instant::now();
                } else if started.elapsed() >= ROTATE_RELOAD_TIMEOUT {
                    log::warn!(
                        "rotate: unverified after {}ms — restarting into {orient:?}",
                        started.elapsed().as_millis()
                    );
                    break 'inner End::Quit;
                }
            }

            // Loading screen lifecycle. The gate is evaluated EVERY tick against the cached latest
            // frame (not only on frame arrival — that was the 45s-boot bug). When it passes, the
            // chrome-dismiss tap fires immediately BEHIND the splash and the reveal waits for its
            // repaints to settle — one full refresh total, landing on the clean page.
            if loading {
                let lp = engine.load_progress();
                if lp >= 100 && prev_lp >= 0 && prev_lp < 100 {
                    log::info!(
                        "boot: network done (lp=100) at t=+{}ms",
                        loading_started.elapsed().as_millis()
                    );
                }
                prev_lp = lp;
                // Boot audit: the gate's three inputs, once a second.
                if last_boot_log.elapsed() >= Duration::from_secs(1) {
                    last_boot_log = Instant::now();
                    log::debug!(
                        "boot: t=+{}ms lp={lp} frames={frames_since_load} ink={last_ink:?}",
                        loading_started.elapsed().as_millis()
                    );
                }
                // A FAILED navigation (DNS/offline — the wake-before-WiFi race) renders only a
                // blank error page: the gate would never pass and the old code stranded the
                // splash forever. Say so and retry until the network is back; each retry resets
                // the per-attempt clocks so the gate (and backstop) judge the attempt freshly.
                if boot_reveal_at.is_none() && engine.load_failed() {
                    if boot_retry_at.map_or(true, |t| Instant::now() >= t) {
                        boot_retry_at = Some(Instant::now() + BOOT_RETRY_INTERVAL);
                        let cur = engine.current_uri();
                        let target = if cur.is_empty() { start_url.clone() } else { cur };
                        log::info!("boot: load failed (offline?) — retrying {target}");
                        engine.load_uri(&target);
                        loading_started = Instant::now();
                        frames_since_load = 0;
                        last_ink = None;
                        prev_lp = -1;
                        last_input = Instant::now();
                    }
                } else if boot_reveal_at.is_none() {
                    let booted = boot_gate(
                        lp,
                        loading_started.elapsed(),
                        frames_since_load,
                        last_ink,
                        last_change.elapsed(),
                    );
                    let timed_out = loading_started.elapsed() >= LOADING_TIMEOUT;
                    if timed_out && !booted {
                        log::warn!(
                            "boot: backstop timeout after {}ms (lp={lp} frames={frames_since_load} \
                             ink={last_ink:?}) — revealing anyway",
                            loading_started.elapsed().as_millis()
                        );
                    }
                    if booted || timed_out {
                        // Deep-linked book opens land on Kindle's controls view — dismiss its
                        // chrome now, masked, so the reveal shows the clean full-screen page.
                        let (tx, ty) = (uw / 2, (uh - BAR_H) / 2);
                        engine.touch(Phase::Down, tx, ty);
                        engine.touch(Phase::Up, tx, ty);
                        last_input = Instant::now();
                        boot_reveal_at = Some(Instant::now() + BOOT_TAP_SETTLE);
                        log::info!(
                            "boot: gate {} in {}ms (lp={lp} frames={frames_since_load} \
                             ink={last_ink:?}) — masked chrome-dismiss tap at ({tx},{ty})",
                            if booted { "satisfied" } else { "timed out" },
                            loading_started.elapsed().as_millis()
                        );
                    }
                } else if boot_reveal_at.is_some_and(|t| {
                    let now = Instant::now();
                    now >= t && (last_change.elapsed() >= BOOT_REVEAL_STILL
                        || now >= t + Duration::from_secs(3))
                }) {
                    // REVEAL: the one full Ui refresh of the boot.
                    boot_reveal_at = None;
                    loading = false;
                    force_redraw = true;
                    content_dirty = true;
                    fulls_since_deep = 0; // a deep de-ghost must not land on the reveal
                    log::info!("boot complete in {}ms", loading_started.elapsed().as_millis());
                }
                // One-line status (≤3 changes per boot), presented as a small crisp Ui partial —
                // no bar, no DU shimmer. Only when the label actually changes.
                if loading && prev_valid {
                    let status = if engine.load_failed() {
                        BootStatus::Offline
                    } else {
                        BootStatus::derive(lp, frames_since_load, boot_reveal_at.is_some())
                    };
                    if status != boot_status {
                        boot_status = status;
                        draw_loading_status(&mut canvas, &fonts, status.label(), uw, uh);
                        compose_fb(&canvas, &mut screen, orient);
                        let (ux, uy, ubw, ubh) = loading_status_rect(uw, uh);
                        let (bx, by, bw2, bh2) = fb_rect_of(orient, uw, uh, ux, uy, ubw, ubh);
                        copy_band(conn.frame_mut(), screen.as_raw(), w, by, bh2);
                        if conn.update_partial(bx, by, bw2, bh2).is_err() {
                            break 'inner End::Detach;
                        }
                        copy_band(&mut prev_screen, screen.as_raw(), w, by, bh2);
                    }
                }
            }

            // Present while the user recently acted OR the page is still changing (bounded by
            // MAX_ACTIVE); otherwise stay quiet. This shows an SPA as it paints instead of freezing on
            // an intermediate loading frame, without ever re-arming forever (the old runaway bug).
            let watching = watching_decision(last_input.elapsed(), last_change.elapsed());

            // Rows the web view occupies this frame in USER space (the keyboard overlays the
            // bottom of the content when shown; the bar owns the strip below). In portrait these
            // are also fb rows; in landscape the fb diff just scans the full height.
            let content_bottom_u = content_bottom(focus_mode, kb_shown, uh);
            let diff_bottom_fb =
                if orient == Orientation::Portrait { content_bottom_u } else { h };
            // Present the current frame only when there's a reason to — and never mid-animation. The
            // browser polls WPE's frames, so a page turn arrives as a burst; present_due() suppresses
            // the intermediates and fires once on settle (or promptly for a keystroke echo, or on the
            // long-load heartbeat). Chrome changes (force_redraw) present immediately. Everything runs
            // under the crisp Ui waveform set once at connect — no per-frame Fast flicker.
            // While loading, engine frames never reach the screen — the loading screen owns the
            // content area (its bar updates present themselves above); only force_redraw (initial
            // paint, bar-state changes, boot-complete) goes through here.
            // While a rotate is masked NOTHING presents — the "Rotating…" card stays alone on
            // screen until the verified reveal above re-arms force_redraw. Right after the reveal
            // the calm window keeps content presents to ROTATE_CALM_STILL of stillness, so
            // Kindle's post-reflow re-render passes coalesce instead of flashing one by one.
            let calm_ok = rot_calm_until.map_or(true, |t| {
                Instant::now() >= t || last_change.elapsed() >= ROTATE_CALM_STILL
            });
            let want = rot_masked_since.is_none()
                && (force_redraw
                    || (!loading
                        && watching
                        && content_dirty
                        && calm_ok
                        && last_present.elapsed() >= MIN_PRESENT_GAP
                        && present_due(last_key.elapsed(), last_change.elapsed(), last_present.elapsed())));
            if want {
                let t0 = Instant::now();
                // Bar/keyboard are static between state changes — redraw only on force_redraw.
                if force_redraw {
                    redraw_ui(
                        &mut canvas, &fonts, focus_mode, kb_shown, keys_env, kb_layer, kb_shift,
                        &mut chrome_hits, &mut kb_hits, uw, uh,
                    );
                }
                if loading {
                    if force_redraw {
                        draw_loading(&mut canvas, &fonts, boot_status.label(), uw, uh);
                    }
                } else if frame_changed || just_attached || rot_present_frame
                    || (force_redraw && prev_valid)
                {
                    // Re-blit the web view when a new frame arrived, when force_redraw
                    // exposed/covered rows (keyboard hide, boot-complete swap), on a warm
                    // re-attach, or at a rotate reveal (both present the RETAINED frame — no new
                    // export may ever come if the page is already done rendering).
                    blit_view(&mut canvas, &frame, uw, view_w, view_h, focus_mode, kb_shown);
                }
                // Egress: user-space canvas → portrait framebuffer (identity in portrait, 90°
                // rotation in landscape); the diff/present pipeline below is fb-space as ever.
                compose_fb(&canvas, &mut screen, orient);
                let cur = screen.as_raw();

                // Diff only the band that could have changed: the whole screen when the chrome was
                // redrawn, else just the web-view band. Avoids a full-height memcmp on every keystroke.
                let full_threshold = h * FULL_REFRESH_ROWS_NUM / FULL_REFRESH_ROWS_DEN;
                let region = if !prev_valid {
                    Some((0, 0, w, h)) // first present
                } else if force_redraw {
                    diff_bbox(cur, &prev_screen, w, h, 0, h, full_threshold)
                } else {
                    diff_bbox(cur, &prev_screen, w, h, 0, diff_bottom_fb, full_threshold)
                };
                let scope = match region {
                    None => "none",
                    Some((_, _, _, rh)) if rh >= full_threshold => {
                        // Large change (a page turn / load) → one whole-screen Ui refresh, plus a deep
                        // de-ghost every FULLS_BETWEEN_DEEP to clear accumulated residue.
                        conn.frame_mut().copy_from_slice(cur);
                        if conn.update_all().is_err() {
                            break 'inner End::Detach;
                        }
                        prev_screen.copy_from_slice(cur);
                        fulls_since_deep += 1;
                        if fulls_since_deep >= FULLS_BETWEEN_DEEP {
                            let _ = conn.request_full_refresh();
                            fulls_since_deep = 0;
                        }
                        presents_full += 1;
                        "full"
                    }
                    Some((x, y, rw, rh)) => {
                        // Small change (a keystroke) → copy just the changed rows, Ui partial (crisp
                        // and quick because the region is small).
                        copy_band(conn.frame_mut(), cur, w, y, rh);
                        if conn.update_partial(x, y, rw, rh).is_err() {
                            break 'inner End::Detach;
                        }
                        copy_band(&mut prev_screen, cur, w, y, rh);
                        presents_partial += 1;
                        "partial"
                    }
                };
                if scope != "none" {
                    prev_valid = true;
                    content_dirty = false;
                    force_redraw = false;
                    rot_present_frame = false;
                    if just_attached {
                        just_attached = false;
                        log::info!("warm attach presented in {}ms", attached_at.elapsed().as_millis());
                    }
                    let gap = last_present.elapsed();
                    last_present = Instant::now();
                    log::debug!(
                        "present {scope} region={:?} cpu={}ms gap={}ms",
                        region, t0.elapsed().as_millis(), gap.as_millis()
                    );
                }
            }

            // Telemetry: one info line per active→quiet settle — visible even at RUST_LOG=info, so
            // the load's present/full counts are always in the log without enabling debug.
            if was_watching && !watching {
                log::info!(
                    "browser settled: {} presents ({} full / {} partial) over {}ms",
                    presents_full + presents_partial,
                    presents_full,
                    presents_partial,
                    active_started.elapsed().as_millis()
                );
                presents_full = 0;
                presents_partial = 0;
                if probe {
                    engine.run_js(PROBE_JS); // density diagnostics → console → wrapper log
                }
                // Persist the open book for next launch's fast start. notify::uri keeps
                // current_uri() honest across Kindle's pushState navigation; write-on-change only,
                // atomically (tmp + rename), so a crash can't leave a torn file.
                let uri = engine.current_uri();
                if is_book_url(&uri) && uri != last_saved_book {
                    let tmp = format!("{last_book_path}.tmp");
                    match std::fs::write(&tmp, &uri).and_then(|_| std::fs::rename(&tmp, &last_book_path)) {
                        Ok(()) => {
                            log::info!("last book saved: {uri}");
                            last_saved_book = uri;
                        }
                        Err(e) => log::warn!("last-book save failed: {e}"),
                    }
                }
            } else if !was_watching && watching {
                active_started = Instant::now();
                presents_full = 0;
                presents_partial = 0;
            }
            was_watching = watching;

            // Screenshot on request: `touch <data_dir>/shot.req` over SSH → we consume the request
            // file and dump the composed screen — exactly the pixels we send the panel — for remote
            // sharpness inspection (scp shot.png off). remove_file doubles as an atomic test+consume.
            // With BROWSER_PROBE=1 the request also fires the DOM probe — unlike the settle-edge
            // probe this catches transient UI (Kindle's tap-to-reveal toolbar) while it's on screen.
            if SHOT.swap(false, Ordering::Relaxed) || std::fs::remove_file(&shot_req).is_ok() {
                if probe {
                    engine.run_js(PROBE_JS);
                }
                let path = format!("{}/shot.png", args.data_dir);
                // The user-space canvas — a landscape shot comes out landscape-oriented.
                match canvas.save(&path) {
                    Ok(()) => log::info!("screenshot saved to {path}"),
                    Err(e) => log::warn!("screenshot save failed: {e}"),
                }
            }

            // Remote-drive tap request: consume <data_dir>/tap.req ("x y" user-space px) and
            // deliver a real Down+Up to the page. Validation-harness plumbing.
            if let Ok(t) = std::fs::read_to_string(&tap_req) {
                let _ = std::fs::remove_file(&tap_req);
                let mut it = t.split_whitespace();
                if let (Some(Ok(tx)), Some(Ok(ty))) =
                    (it.next().map(str::parse::<i32>), it.next().map(str::parse::<i32>))
                {
                    log::info!("tap.req: engine tap at ({tx},{ty})");
                    engine.touch(Phase::Down, tx, ty);
                    engine.touch(Phase::Up, tx, ty);
                    last_input = Instant::now();
                    let due = Instant::now() + TAP_PROBE_DELAY;
                    page_probe_at = Some(page_probe_at.map_or(due, |cur| cur.min(due)));
                }
            }

            // Remote-drive JS request: consume <data_dir>/js.req and run its contents in the
            // page. Validation-harness plumbing (synthesize the taps a finger would make), same
            // spirit as shot.req.
            if let Ok(js) = std::fs::read_to_string(&js_req) {
                let _ = std::fs::remove_file(&js_req);
                let js = js.trim();
                if !js.is_empty() {
                    log::info!("js.req: running {} bytes", js.len());
                    engine.run_js(js);
                    last_input = Instant::now();
                    // A synthetic interaction is a tap as far as the page is concerned —
                    // schedule the same post-tap probe so chrome edges are observed.
                    let due = Instant::now() + TAP_PROBE_DELAY;
                    page_probe_at = Some(page_probe_at.map_or(due, |cur| cur.min(due)));
                }
            }
        }; // 'inner

        // Session over. ⭲/SIGTERM (or single-process mode) → exit for real; a detach in daemon
        // mode → throttle WebKit, drop this session's surfaces/devices, and stay warm for the
        // next open — that's the whole point: the booted page IS the cache.
        engine.set_active(false);
        if matches!(end, End::Quit) || listener.is_none() {
            break 'outer;
        }
        // Deliberate early-drops: close the client socket + evdev fds NOW rather than holding
        // them across a warm-idle wait of up to 6h (`.take()` drops without a dead-store lint).
        attach_stream.take();
        touch.take();
        pen.take();
        log::info!(
            "detached — staying warm (idle exit after {}h without an open)",
            ATTACH_IDLE_EXIT.as_secs() / 3600
        );
        let (next_conn, next_stream) = {
            let l = listener.as_ref().expect("daemon mode has a listener");
            loop {
                match wait_attach(l, ATTACH_IDLE_EXIT) {
                    Some((s, k)) => match Connection::connect(k, qtfb::FBFMT_RMPPM_RGB888, None) {
                        Ok(c) => break (c, s),
                        Err(e) => {
                            // The handshake already answered "ok", so the client is blocked
                            // waiting on this stream — drop it EXPLICITLY so it sees EOF and
                            // exits instead of wedging until the idle timeout.
                            drop(s);
                            log::warn!("qtfb connect for attach failed: {e}");
                        }
                    },
                    None => break 'outer, // idle timeout or QUIT while waiting
                }
            }
        };
        conn = next_conn;
        let _ = conn.set_refresh_mode(RefreshMode::Ui);
        attach_stream = Some(next_stream);

        // Per-attach resets: fresh input devices (robust across suspends), full repaint, keyboard
        // re-synced from the page's real focus state, watch window re-armed.
        let now = Instant::now();
        attached_at = now;
        just_attached = true;
        touch = Touch::open().ok();
        pen = Pen::open().ok();
        pen_up_at = None;
        pen_gate = PenGate::new();
        touch_owner = Owner::None;
        exit_pressed_at = None;
        detach_requested = false;
        rot_masked_since = None;
        masked_focus = false;
        rot_reloaded = false;
        rot_tap_at = None;
        rot_taps = 0;
        rot_calm_until = None;
        rot_present_frame = false;
        // Focus mode itself persists across warm detach/attach (the view is still sized for it);
        // the transient probe state doesn't.
        focus_want = None;
        page_probe_at = None;
        page_js_seq = engine.js_result().0;
        kb_field_seen = false;
        kb_zero_streak = 0;
        prev_valid = false;
        force_redraw = true;
        content_dirty = true;
        // Keyboard re-sync goes through the gate (see the rotate path).
        kb_shown = false;
        engine.run_js(KB_UNSHIFT_JS);
        last_focus = engine.kb_focus();
        kb_gate = if last_focus { Some(Instant::now()) } else { None };
        last_input = now;
        last_change = now;
        last_present = now - MIN_PRESENT_GAP;
        fulls_since_deep = 0;
        was_watching = true;
        active_started = now;
        presents_full = 0;
        presents_partial = 0;
        engine.set_active(true);
        } // 'outer
        // Clean daemon exits take their socket file with them — a stale file made the wrapper's
        // spawn-wait pass instantly and attach into nothing (the bootstrap race). Crashes can still
        // leave one; the wrapper removes any stale file before spawning as the backstop.
        if listener.is_some() {
            let _ = std::fs::remove_file(attach_sock_path(&args.data_dir));
        }
        log::info!("exiting cleanly");
        Ok(())
    }

    /// Bottom (exclusive) of the rows the web view occupies: the engine renders `[0, h - BAR_H)`
    /// (the bar has its own strip), and the keyboard overlays the bottom `KB_H` of that when shown.
    /// In focus mode the bar is gone and the view IS the whole height (the keyboard never shows in
    /// focus — raising it leaves focus first).
    fn content_bottom(focus: bool, kb_shown: bool, h: i32) -> i32 {
        if focus {
            h
        } else {
            h - BAR_H - if kb_shown { KB_H } else { 0 }
        }
    }

    /// Whether an unpresented content change is due to be shown this loop. The browser polls WPE's
    /// frames, so a page turn is a burst of intermediates we want to *suppress*, presenting once when
    /// it settles. Present when: the content has held still past `SETTLE_DELAY` (page turn / load
    /// finished), OR a keystroke is still fresh (`< INPUT_ECHO` — echo it promptly), OR a long
    /// continuous change has gone `PROGRESS_INTERVAL` without a present (heartbeat so a slow load isn't
    /// frozen on one frame). The caller has already checked there *is* a pending change and that
    /// `MIN_PRESENT_GAP` has elapsed.
    fn present_due(since_key: Duration, since_change: Duration, since_present: Duration) -> bool {
        since_key < INPUT_ECHO
            || since_change >= SETTLE_DELAY
            || since_present >= PROGRESS_INTERVAL
    }

    /// The boot's one-line status — at most three changes across a whole boot, so the loading
    /// screen stays visually still (the old progress bar's DU-waveform ticks shimmered).
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum BootStatus {
        Connecting,
        Rendering,
        AlmostReady,
        /// The navigation FAILED (DNS/offline — e.g. the app opened before WiFi finished
        /// re-associating after a wake). Shown while the retry loop re-issues the load.
        Offline,
    }

    impl BootStatus {
        fn label(self) -> &'static str {
            match self {
                BootStatus::Connecting => "Connecting…",
                BootStatus::Rendering => "Rendering…",
                BootStatus::AlmostReady => "Almost ready…",
                BootStatus::Offline => "No connection — retrying…",
            }
        }
        /// Derived from the boot's real milestones; `masked` = the gate passed and the
        /// chrome-dismiss is running behind the splash.
        fn derive(lp: i32, frames_since_load: u32, masked: bool) -> BootStatus {
            if masked {
                BootStatus::AlmostReady
            } else if lp >= 100 || frames_since_load > 0 {
                BootStatus::Rendering
            } else {
                BootStatus::Connecting
            }
        }
    }

    /// The boot-complete decision, evaluated EVERY tick while loading against the CACHED latest
    /// frame (`last_ink`) — the first cut required all conditions to line up on the exact loop
    /// iteration a new frame arrived; Kindle's last boot frame lands seconds before progress/ink
    /// qualify, so the gate never re-fired and every cold boot rode the 45s backstop.
    /// `frames_since_load >= 1` guards the pre-load phase where `load_progress` still reads its
    /// initial 100. Two acceptance arms: clearly inky content, or some ink that has held stable.
    fn boot_gate(
        lp: i32, elapsed: Duration, frames_since_load: u32, last_ink: Option<u32>,
        since_change: Duration,
    ) -> bool {
        let Some(ink) = last_ink else { return false };
        lp >= 100
            && elapsed >= BOOT_MIN_ELAPSED
            && frames_since_load >= 1
            && (ink >= BOOT_INK_PERMILLE
                || (ink >= BOOT_INK_MIN && since_change >= BOOT_STABLE)
                || (elapsed >= BOOT_SETTLED_MIN && since_change >= BOOT_SETTLED_STILL))
    }

    /// Per-mille of sampled pixels that are "ink" (luma < 200) in a BGRA frame — sparse prime-stride
    /// sample in the spirit of `frame_hash`. Separates Kindle's near-blank loading shell from a real
    /// page of content for the boot-complete decision.
    fn frame_ink_permille(frame: &[u8]) -> u32 {
        let (mut ink, mut n) = (0u32, 0u32);
        let mut i = 0usize;
        while i + 3 < frame.len() {
            let (b, g, r) = (frame[i] as u32, frame[i + 1] as u32, frame[i + 2] as u32);
            let luma = (77 * r + 150 * g + 29 * b) >> 8;
            if luma < 200 {
                ink += 1;
            }
            n += 1;
            i += 251 * 4; // prime pixel stride — ~1.5k samples across the frame
        }
        if n == 0 {
            0
        } else {
            ink * 1000 / n
        }
    }

    /// The padded band the one-line boot status occupies — re-presented alone (a small, crisp Ui
    /// partial, at most 2-3 times per boot) while the rest of the loading screen stays untouched.
    fn loading_status_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
        let bw = 600;
        let x = (w - bw) / 2;
        let y = (h as f32 * 0.42) as i32 + 16;
        (x, y, bw, 64)
    }

    /// Draw just the status line (clearing its band first so labels never overstrike).
    fn draw_loading_status(screen: &mut RgbImage, fonts: &Fonts, status: &str, w: i32, h: i32) {
        let (bx, by, bw, bh) = loading_status_rect(w, h);
        fill_rect(screen, bx as f32, by as f32, bw as f32, bh as f32, PAPER);
        let sw = measure(&fonts.sans, 30.0, status);
        draw_text(screen, &fonts.sans, 30.0, (w as f32 - sw) / 2.0, h as f32 * 0.42 + 56.0, status, MUTED);
    }

    /// The startup/reload loading screen — the sibling apps' visual voice (serif masthead, quiet
    /// one-line status) while Amazon's app boots. Drawn ONCE per boot and then left alone apart
    /// from ≤3 status-line changes — a static splash can't flicker. Fills the content area only;
    /// the bottom bar stays live, so ↺ and ⭲ work mid-boot.
    fn draw_loading(screen: &mut RgbImage, fonts: &Fonts, status: &str, w: i32, h: i32) {
        fill_rect(screen, 0.0, 0.0, w as f32, (h - BAR_H) as f32, PAPER);
        let base_y = h as f32 * 0.42;
        let title = "Kindle";
        let tw = measure(&fonts.serif_bold, 64.0, title);
        draw_text(screen, &fonts.serif_bold, 64.0, (w as f32 - tw) / 2.0, base_y, title, INK);
        draw_loading_status(screen, fonts, status, w, h);
    }

    /// The panel is colour e-ink (Gallery 3) — real but naturally muted colour. Kindle's pastel
    /// tints (highlight bands, the drag selection, the palette swatches) would wash out to
    /// near-white, so colourful pixels get their saturation BOOSTED (pushed away from their luma)
    /// and are darkened in proportion to chroma·luma — pale tints become vivid, clearly visible
    /// washes that KEEP their hue: yellow reads yellow in the colour palette. (The first cut
    /// collapsed colours to grey on a mono-panel assumption — the swatches all looked alike.)
    /// Greys (chroma 0): body text, paper — pass through untouched.
    fn ink_px(r: u8, g: u8, b: u8) -> [u8; 3] {
        let (ri, gi, bi) = (r as i32, g as i32, b as i32);
        let maxc = ri.max(gi).max(bi);
        let minc = ri.min(gi).min(bi);
        let chroma = maxc - minc;
        if chroma == 0 {
            return [r, g, b];
        }
        let luma = (77 * ri + 150 * gi + 29 * bi) >> 8; // BT.601, integer
        let darken = (chroma * luma * 140) >> 16; // milder than the old grey wash — hue works too
        let vivid = |c: i32| -> u8 {
            let sat = luma + ((c - luma) * 9) / 5; // 1.8× saturation for the muted panel
            (sat - darken).clamp(0, 255) as u8
        };
        [vivid(ri), vivid(gi), vivid(bi)]
    }

    /// Copy the engine's BGRA frame into the screen's web-view rows (engine row == screen row,
    /// top-aligned). Fills `[0, content_bottom)` — the whole view, minus the keyboard overlay when
    /// it's shown. Colour is remapped through [`ink_px`] so highlights survive greyscale. The
    /// per-row slices let `chunks_exact` elide bounds checks in the hot per-pixel loop.
    fn blit_view(
        screen: &mut RgbImage, frame: &[u8], w: i32, view_w: u32, view_h: u32, focus: bool,
        kb_shown: bool,
    ) {
        // In focus mode view_h == h (full height); otherwise view_h == h - BAR_H.
        let bottom =
            if focus { view_h as i32 } else { content_bottom(false, kb_shown, view_h as i32 + BAR_H) };
        let sw = w as usize;
        let vw = view_w as usize;
        let raw: &mut [u8] = screen;
        for y in 0..bottom as usize {
            let d0 = y * sw * 3;
            let s0 = y * vw * 4;
            let dst = &mut raw[d0..d0 + vw * 3];
            let src = &frame[s0..s0 + vw * 4];
            for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
                d.copy_from_slice(&ink_px(s[2], s[1], s[0])); // frame is BGRA
            }
        }
    }

    /// Copy the horizontal band of rows `[y, y+rh)` (one contiguous memcpy) from `src` to `dst`.
    /// Used to push only the changed rows into the framebuffer / prev-screen instead of the whole
    /// screen each present.
    fn copy_band(dst: &mut [u8], src: &[u8], w: i32, y: i32, rh: i32) {
        let stride = w as usize * 3;
        let a = y as usize * stride;
        let b = a + rh as usize * stride;
        dst[a..b].copy_from_slice(&src[a..b]);
    }

    /// Draw the bottom action bar (and, when shown, the keyboard above it). Called only when their
    /// state changes — records the hit rects for input routing.
    ///
    /// The bar is the reader app's footer recipe verbatim: a hairline rule, then icons inked
    /// directly on paper in 60px cells spread by `even_slots` — ↺ reload on the left corner,
    /// ⭲ exit on the right. Kindle's own chrome does everything else, so the bar stays this quiet.
    /// While the keyboard is up a centre ⌄ dismisses it (insurance if a blur never fires);
    /// with BROWSER_KEYS=1 a centre "Keys" chip raises it manually instead (fallback for pages
    /// that never fire focus events).
    #[allow(clippy::too_many_arguments)]
    fn redraw_ui(
        screen: &mut RgbImage, fonts: &Fonts, focus: bool, kb_shown: bool, keys_env: bool,
        kb_layer: KbLayer, kb_shift: bool, chrome_hits: &mut Vec<(Rect, Action)>,
        kb_hits: &mut Vec<(Rect, KeyCap)>, w: i32, h: i32,
    ) {
        chrome_hits.clear();
        kb_hits.clear();
        if focus {
            // Focus mode: no bar, no hits — the page owns the whole height (blit_view fills it).
            return;
        }

        let fy = h - BAR_H;
        let icon_y = fy + 18;
        fill_rect(screen, 0.0, fy as f32, w as f32, BAR_H as f32, PAPER);
        hline(screen, MARGIN as f32, (w - MARGIN) as f32, fy as f32, FAINT, 1);
        if kb_shown || keys_env {
            // Typing mode (or the BROWSER_KEYS fallback): corners + a centre keyboard control —
            // rotating mid-typing isn't a thing, so the rotate pair steps aside.
            let slots = even_slots(w, MARGIN, ICELL, 2);
            chrome_hits.push((icon_btn(screen, fonts, Icon::Undo, slots[0], icon_y, ICELL, 60, INK), Action::Reload));
            chrome_hits.push((icon_btn(screen, fonts, Icon::Exit, slots[1], icon_y, ICELL, 60, INK), Action::Exit));
            if kb_shown {
                let cx = (w - ICELL) / 2;
                chrome_hits.push((icon_btn(screen, fonts, Icon::ChevronDown, cx, icon_y, ICELL, 60, INK), Action::ToggleKeys));
            } else {
                let chip_w = 120;
                chrome_hits.push((chip(screen, fonts, "Keys", (w - chip_w) / 2, fy + 22, chip_w, 52, false), Action::ToggleKeys));
            }
        } else {
            // Reading mode: ↺ reload, ⟲/⟳ rotate (the pair also picks the landscape hand),
            // book-fill = focus mode (just the book, full height), ⭲ exit.
            let slots = even_slots(w, MARGIN, ICELL, 5);
            chrome_hits.push((icon_btn(screen, fonts, Icon::Undo, slots[0], icon_y, ICELL, 60, INK), Action::Reload));
            chrome_hits.push((icon_btn(screen, fonts, Icon::RotateLeft, slots[1], icon_y, ICELL, 60, INK), Action::RotateLeft));
            chrome_hits.push((icon_btn(screen, fonts, Icon::RotateRight, slots[2], icon_y, ICELL, 60, INK), Action::RotateRight));
            chrome_hits.push((icon_btn(screen, fonts, Icon::BookFill, slots[3], icon_y, ICELL, 60, INK), Action::Focus));
            chrome_hits.push((icon_btn(screen, fonts, Icon::Exit, slots[4], icon_y, ICELL, 60, INK), Action::Exit));
        }

        // Keyboard overlay above the bar (or nothing — blit_view refills the rows when hidden).
        // Clear the strip first: draw_keyboard paints keys, not the background, and the stale web
        // pixels underneath would otherwise bleed through between the rows.
        if kb_shown {
            let ky = h - BAR_H - KB_H;
            fill_rect(screen, 0.0, ky as f32, w as f32, KB_H as f32, PAPER);
            *kb_hits = draw_keyboard(screen, fonts, ky, w, kb_layer, kb_shift, "Go");
        }
    }

    fn apply_key(
        engine: &Engine, cap: KeyCap, layer: &mut KbLayer, shift: &mut bool, force_redraw: &mut bool,
    ) {
        match cap {
            KeyCap::Char(c) => {
                engine.tap_key(c as u32); // basic-latin keysym == codepoint (pre-cased by the layout)
                if *shift {
                    *shift = false; // one-shot shift
                    *force_redraw = true;
                }
            }
            KeyCap::Backspace => engine.tap_key(keysym::BACKSPACE),
            KeyCap::Space => engine.tap_key(keysym::SPACE),
            KeyCap::Enter => engine.tap_key(keysym::RETURN),
            KeyCap::Shift => {
                *shift = !*shift;
                *force_redraw = true;
            }
            KeyCap::Layer => {
                *layer = if *layer == KbLayer::Letters { KbLayer::Symbols } else { KbLayer::Letters };
                *shift = false; // a pending one-shot shift shouldn't leak across a layer switch
                *force_redraw = true;
            }
        }
    }

    fn hit_chrome(hits: &[(Rect, Action)], x: i32, y: i32) -> Option<Action> {
        hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, a)| *a)
    }
    fn hit_key(hits: &[(Rect, KeyCap)], x: i32, y: i32) -> Option<KeyCap> {
        hits.iter().find(|(r, _)| r.contains(x, y)).map(|(_, c)| *c)
    }

    /// Bounding box of pixels that differ between the freshly composed screen and the last one
    /// presented (both RGB, w×h). Only rows in `[y_start, y_end)` are scanned — callers pass the band
    /// that could have changed (whole screen, or just the web-view rows) to skip a full-height
    /// memcmp. Returns `None` if nothing changed. Rows are compared with a fast slice `!=` (memcmp);
    /// if the dirty rows already span a large fraction of the screen the column scan is skipped (the
    /// caller will full-refresh anyway). Coordinates are framebuffer pixels; padding clamps to `h`.
    fn diff_bbox(
        cur: &[u8], prev: &[u8], w: i32, h: i32, y_start: i32, y_end: i32, full_threshold: i32,
    ) -> Option<(i32, i32, i32, i32)> {
        let stride = w as usize * 3;
        let (mut y0, mut y1) = (y_end, -1);
        for y in y_start as usize..y_end as usize {
            let a = &cur[y * stride..y * stride + stride];
            let b = &prev[y * stride..y * stride + stride];
            if a != b {
                if (y as i32) < y0 {
                    y0 = y as i32;
                }
                y1 = y as i32;
            }
        }
        if y1 < 0 {
            return None;
        }
        let row_span = y1 - y0 + 1;
        if row_span >= full_threshold {
            return Some((0, y0, w, row_span)); // large — skip column scan, caller full-refreshes
        }
        // Tighten to the changed columns within the dirty rows.
        let (mut x0, mut x1) = (w, -1);
        for y in y0 as usize..=y1 as usize {
            let base = y * stride;
            for x in 0..w as usize {
                let p = base + x * 3;
                if cur[p] != prev[p] || cur[p + 1] != prev[p + 1] || cur[p + 2] != prev[p + 2] {
                    if (x as i32) < x0 {
                        x0 = x as i32;
                    }
                    if (x as i32) > x1 {
                        x1 = x as i32;
                    }
                }
            }
        }
        if x1 < 0 {
            return None;
        }
        // Pad a little so anti-aliased edges refresh cleanly.
        let pad = 2;
        let px0 = (x0 - pad).max(0);
        let py0 = (y0 - pad).max(0);
        let px1 = (x1 + pad).min(w - 1);
        let py1 = (y1 + pad).min(h - 1);
        Some((px0, py0, px1 - px0 + 1, py1 - py0 + 1))
    }

    /// Fast content fingerprint: FNV-1a over a sparse sample of the frame. Two visually identical
    /// frames hash equal, so a static page never triggers a refresh.
    fn frame_hash(frame: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        let mut i = 0;
        while i < frame.len() {
            hash ^= frame[i] as u64;
            hash = hash.wrapping_mul(0x100000001b3);
            i += 617; // prime stride — samples the whole frame cheaply
        }
        hash
    }

    /// Content-stability watch decision: we present (and poll fast) while the user acted recently OR
    /// the page changed recently, but never longer than `MAX_ACTIVE` since the last user input — the
    /// backstop that guarantees the app goes quiet even if a page repaints forever.
    fn watching_decision(since_input: Duration, since_change: Duration) -> bool {
        since_input < MAX_ACTIVE && (since_input < IDLE_GRACE || since_change < IDLE_GRACE)
    }

    /// The library page to open when there's no last book: `KINDLE_DEFAULT_URL` if it's a valid
    /// read.amazon URL, otherwise the built-in default.
    fn resolve_default_url(configured: Option<&str>) -> String {
        configured.filter(|u| stored_url_ok(u)).unwrap_or(DEFAULT_URL).to_string()
    }

    fn parse_args() -> Args {
        let mut a = Args {
            url: resolve_default_url(std::env::var("KINDLE_DEFAULT_URL").ok().as_deref()),
            url_is_flag: false,
            data_dir: DEFAULT_DATA_DIR.into(),
            scale: 0.0,
            mode: Mode::Single,
        };
        let mut scale_flag: Option<String> = None;
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--url" => {
                    a.url = it.next().unwrap_or(a.url);
                    a.url_is_flag = true;
                }
                "--data-dir" => a.data_dir = it.next().unwrap_or(a.data_dir),
                "--scale" => scale_flag = it.next(),
                "--daemon" => a.mode = Mode::Daemon,
                "--attach" => a.mode = Mode::Attach,
                "--config" => {
                    let _ = it.next(); // reserved; data-dir may derive from config later
                }
                "-V" | "--version" => {
                    println!(env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-h" | "--help" => {
                    eprintln!(
                        "browserapp [--daemon|--attach] [--url <start-url>] [--data-dir <dir>] [--scale <1..4>] [--config <toml>]"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("ignoring unknown arg: {other}"),
            }
        }
        a.scale = resolve_scale(scale_flag.as_deref(), std::env::var("BROWSER_SCALE").ok().as_deref());
        a
    }

    /// Device scale resolution: `--scale` beats `BROWSER_SCALE` (wrapper-editable, so tuning needs no
    /// re-release) beats `SCALE_DEFAULT`. Unparseable or out-of-range values fall back to the default
    /// — a bad wrapper edit must not produce an unusable view.
    fn resolve_scale(flag: Option<&str>, env: Option<&str>) -> f64 {
        flag.or(env)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|s| (1.0..=4.0).contains(s))
            .unwrap_or(SCALE_DEFAULT)
    }

    fn install_signals() {
        extern "C" fn on_term(_: libc::c_int) {
            QUIT.store(true, Ordering::Relaxed);
        }
        unsafe {
            libc::signal(libc::SIGTERM, on_term as *const () as usize as libc::sighandler_t);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn watching_decision_stays_active_while_content_changes() {
            use std::time::Duration;
            let ms = Duration::from_millis;
            // Just acted → active.
            assert!(watching_decision(ms(0), ms(9999)));
            // No input for a while but content changed recently → still active (a load rendering).
            assert!(watching_decision(ms(5000), ms(200)));
            // Neither input nor content recent → quiet.
            assert!(!watching_decision(ms(5000), ms(5000)));
            // Content still changing but past the MAX_ACTIVE cap since last input → quiet (backstop).
            assert!(!watching_decision(MAX_ACTIVE + ms(1), ms(0)));
            // Right at the grace boundary edges.
            assert!(watching_decision(IDLE_GRACE - ms(1), MAX_ACTIVE));
            assert!(!watching_decision(IDLE_GRACE + ms(1), IDLE_GRACE + ms(1)));
        }

        #[test]
        fn diff_bbox_none_when_identical() {
            let w = 8;
            let h = 4;
            let buf = vec![0u8; (w * h * 3) as usize];
            assert_eq!(diff_bbox(&buf, &buf, w, h, 0, h, h), None);
        }

        #[test]
        fn diff_bbox_finds_single_pixel() {
            let w = 8;
            let h = 4;
            let a = vec![0u8; (w * h * 3) as usize];
            let mut b = a.clone();
            // Change pixel (x=3, y=2).
            let p = ((2 * w + 3) * 3) as usize;
            b[p] = 255;
            // full_threshold high so it stays a partial; padding ±2 clamped to the buffer.
            let (x, y, rw, rh) = diff_bbox(&b, &a, w, h, 0, h, h).unwrap();
            assert!(x <= 3 && x + rw > 3, "x band {x}..{}", x + rw);
            assert!(y <= 2 && y + rh > 2, "y band {y}..{}", y + rh);
        }

        #[test]
        fn diff_bbox_band_ignores_change_outside_range() {
            let w = 8;
            let h = 6;
            let a = vec![0u8; (w * h * 3) as usize];
            let mut b = a.clone();
            b[0] = 255; // change on row 0
            // Scanning only rows [3, 6) must miss the row-0 change.
            assert_eq!(diff_bbox(&b, &a, w, h, 3, h, h), None);
            // Scanning the full range must find it.
            assert!(diff_bbox(&b, &a, w, h, 0, h, h).is_some());
        }

        #[test]
        fn diff_bbox_large_change_skips_column_scan() {
            let w = 8;
            let h = 10;
            let a = vec![0u8; (w * h * 3) as usize];
            let mut b = a.clone();
            for byte in b.iter_mut() {
                *byte = 200; // every row differs
            }
            // full_threshold small → the whole span is reported at full width, no column tightening.
            let (x, _, rw, rh) = diff_bbox(&b, &a, w, h, 0, h, 2).unwrap();
            assert_eq!((x, rw), (0, w));
            assert!(rh >= 2);
        }

        #[test]
        fn copy_band_copies_only_its_rows() {
            let w = 4;
            let h = 4;
            let src: Vec<u8> = (0..(w * h * 3) as u8).collect();
            let mut dst = vec![0u8; (w * h * 3) as usize];
            copy_band(&mut dst, &src, w, 1, 2); // rows 1..3
            let stride = (w * 3) as usize;
            assert_eq!(&dst[..stride], &vec![0u8; stride][..]); // row 0 untouched
            assert_eq!(&dst[stride..3 * stride], &src[stride..3 * stride]); // rows 1,2 copied
            assert_eq!(&dst[3 * stride..], &vec![0u8; stride][..]); // row 3 untouched
        }

        #[test]
        fn content_bottom_tracks_bar_keyboard_and_focus() {
            let h = 1696;
            // Web view fills everything above the bar strip…
            assert_eq!(content_bottom(false, false, h), h - BAR_H);
            // …minus the keyboard overlay when it's up.
            assert_eq!(content_bottom(false, true, h), h - BAR_H - KB_H);
            // Focus mode: the page owns the whole height (no bar; kb forces focus off first).
            assert_eq!(content_bottom(true, false, h), h);
            // The engine width must divide exactly at the default scale (954/1.8 = 530) so the
            // SHM buffer width matches the panel; the height WebKit rounds (device-verified).
            assert_eq!(954.0 / SCALE_DEFAULT, (954.0 / SCALE_DEFAULT).round());
        }

        #[test]
        fn present_due_suppresses_mid_animation() {
            use std::time::Duration;
            let ms = Duration::from_millis;
            // Settled past SETTLE_DELAY (page turn / load finished), no key, well within the heartbeat
            // → present once.
            assert!(present_due(INPUT_ECHO, SETTLE_DELAY, ms(0)));
            // Fresh keystroke → echo it promptly even though nothing has settled.
            assert!(present_due(ms(0), ms(0), ms(0)));
            // Mid-animation: change < SETTLE_DELAY ago, no recent key, present was recent → suppress
            // (this is the intermediate page-turn frame we must NOT flash).
            assert!(!present_due(INPUT_ECHO, SETTLE_DELAY - ms(1), ms(0)));
            // Long continuous load: still changing, but PROGRESS_INTERVAL since the last present →
            // heartbeat one progress frame so it isn't frozen.
            assert!(present_due(INPUT_ECHO, ms(0), PROGRESS_INTERVAL));
        }

        #[test]
        fn orientation_transforms_round_trip() {
            let (fw, fh) = (954, 1696);
            for o in [Orientation::Portrait, Orientation::Landscape, Orientation::LandscapeAlt] {
                let (uw, uh) = o.user_dims(fw, fh);
                // Every corner and the centre round-trips fb→user→fb exactly.
                for (xf, yf) in [(0, 0), (fw - 1, 0), (0, fh - 1), (fw - 1, fh - 1), (fw / 2, fh / 2)] {
                    let (xu, yu) = user_from_fb(o, uw, uh, xf, yf);
                    assert!(xu >= 0 && xu < uw && yu >= 0 && yu < uh, "{o:?} in-bounds");
                    assert_eq!(fb_from_user(o, uw, uh, xu, yu), (xf, yf), "{o:?} round trip");
                }
            }
            // Landscape dims swap; portrait doesn't. The two landscape hands are 180° apart:
            // the same fb point maps to opposite user corners.
            assert_eq!(Orientation::Landscape.user_dims(fw, fh), (fh, fw));
            assert_eq!(Orientation::Portrait.user_dims(fw, fh), (fw, fh));
            let (uw, uh) = (fh, fw);
            let a = user_from_fb(Orientation::Landscape, uw, uh, 0, 0);
            let b = user_from_fb(Orientation::LandscapeAlt, uw, uh, 0, 0);
            assert_eq!((a.0 + b.0, a.1 + b.1), (uw - 1, uh - 1), "opposite corners");
        }

        #[test]
        fn fb_rect_of_maps_the_bar_strip() {
            let (fw, fh) = (954, 1696);
            let (uw, uh) = Orientation::Landscape.user_dims(fw, fh); // 1696×954
            // The user-bottom bar strip (full width × BAR_H) becomes a full-height fb column —
            // on opposite sides for the two landscape hands.
            let (x, y, w, h) = fb_rect_of(Orientation::Landscape, uw, uh, 0, uh - BAR_H, uw, BAR_H);
            assert_eq!((x, y, w, h), (0, 0, BAR_H, uw));
            let (x2, y2, w2, h2) =
                fb_rect_of(Orientation::LandscapeAlt, uw, uh, 0, uh - BAR_H, uw, BAR_H);
            assert_eq!((x2, y2, w2, h2), (uh - BAR_H, 0, BAR_H, uw));
            assert!(x2 >= 0 && y2 >= 0 && x2 + w2 <= fw && y2 + h2 <= fh, "within the fb");
            // Portrait is identity.
            assert_eq!(fb_rect_of(Orientation::Portrait, fw, fh, 5, 7, 11, 13), (5, 7, 11, 13));
        }

        #[test]
        fn compose_fb_rotates_both_hands_correctly() {
            // 3×2 user canvas → 2×3 fb: every tagged pixel must land where fb_from_user says.
            let (uw, uh) = (3u32, 2u32);
            let mut canvas = RgbImage::from_pixel(uw, uh, Rgb([0, 0, 0]));
            for y in 0..uh {
                for x in 0..uw {
                    canvas.put_pixel(x, y, Rgb([x as u8, y as u8, 0]));
                }
            }
            for o in [Orientation::Landscape, Orientation::LandscapeAlt] {
                let mut screen = RgbImage::from_pixel(uh, uw, Rgb([9, 9, 9]));
                compose_fb(&canvas, &mut screen, o);
                for y_u in 0..uh as i32 {
                    for x_u in 0..uw as i32 {
                        let (xf, yf) = fb_from_user(o, uw as i32, uh as i32, x_u, y_u);
                        let px = screen.get_pixel(xf as u32, yf as u32);
                        assert_eq!((px[0], px[1]), (x_u as u8, y_u as u8), "{o:?} user({x_u},{y_u})");
                    }
                }
            }
        }

        #[test]
        fn orientation_persistence_parses() {
            // read_orientation defaults to portrait for absent/garbage files. (Temp dir.)
            let dir = std::env::temp_dir().join(format!("bapp-orient-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let dd = dir.to_str().unwrap();
            assert_eq!(read_orientation(dd), Orientation::Portrait, "absent → portrait");
            for o in [Orientation::Landscape, Orientation::LandscapeAlt, Orientation::Portrait] {
                persist_orientation(dd, o);
                assert_eq!(read_orientation(dd), o);
            }
            std::fs::write(orientation_path(dd), "garbage").unwrap();
            assert_eq!(read_orientation(dd), Orientation::Portrait, "garbage → portrait");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn rotate_cooldown_gates_double_taps() {
            let t0 = Instant::now();
            assert!(rotate_allowed(t0, None), "first rotation always allowed");
            assert!(!rotate_allowed(t0 + Duration::from_millis(500), Some(t0)), "inside cooldown");
            assert!(rotate_allowed(t0 + ROTATE_COOLDOWN, Some(t0)), "cooldown elapsed");
        }

        #[test]
        fn rotate_dir_picks_hands_and_returns_to_portrait() {
            // From portrait the two buttons pick opposite landscape hands…
            let right = rotate_dir(Orientation::Portrait, true);
            let left = rotate_dir(Orientation::Portrait, false);
            assert_ne!(right, left, "the buttons choose opposite hands");
            assert_ne!(right, Orientation::Portrait);
            assert_ne!(left, Orientation::Portrait);
            // …and from either landscape hand, both buttons return to portrait.
            for o in [Orientation::Landscape, Orientation::LandscapeAlt] {
                assert_eq!(rotate_dir(o, true), Orientation::Portrait);
                assert_eq!(rotate_dir(o, false), Orientation::Portrait);
            }
        }

        #[test]
        fn pen_gate_swallows_a_graze() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            g.on_down(t0, 100, 100, &mut out);
            assert!(out.is_empty(), "nothing forwarded on contact");
            g.tick(t0 + ms(100), &mut out);
            g.on_up(t0 + ms(120), 100, 100, &mut out);
            assert!(out.is_empty(), "a graze vanishes entirely");
            assert!(!g.engaged());
            assert!(!g.in_grace(t0 + ms(200)));
        }

        #[test]
        fn pen_gate_drag_highlights_like_the_reader() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            // Nib down and drag immediately — the reader-app gesture. No stillness required.
            g.on_down(t0, 100, 100, &mut out);
            g.on_move(t0 + ms(40), 130, 100, &mut out);
            assert_eq!(out, vec![(Phase::Down, 100, 100)], "anchor pinned at the contact point");
            out.clear();
            // While Kindle's long-press engages, the drag is tracked but not sent.
            g.on_move(t0 + ms(200), 220, 100, &mut out);
            g.tick(t0 + ms(400), &mut out);
            assert!(out.is_empty(), "anchored — nothing streams yet");
            // Anchor releases → the touch catches up to the nib, then streams live.
            g.tick(t0 + ms(40) + PEN_ANCHOR, &mut out);
            assert_eq!(out, vec![(Phase::Move, 220, 100)]);
            out.clear();
            g.on_move(t0 + ms(900), 300, 100, &mut out);
            assert_eq!(out, vec![(Phase::Move, 300, 100)]);
            out.clear();
            g.on_up(t0 + ms(1200), 320, 100, &mut out);
            assert_eq!(out, vec![(Phase::Up, 320, 100)]);
            assert!(g.in_grace(t0 + ms(1300)), "colour menu is pen-tappable");
        }

        #[test]
        fn pen_gate_press_selects_word_never_clicks() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            // A stationary press past the graze window anchors (press = select word under nib)…
            g.on_down(t0, 100, 100, &mut out);
            g.tick(t0 + PEN_GRAZE_MAX, &mut out);
            assert_eq!(out, vec![(Phase::Down, 100, 100)]);
            out.clear();
            // …and an early lift is QUEUED until the long-press has fired — never a click.
            g.on_up(t0 + ms(400), 102, 100, &mut out);
            assert!(out.is_empty());
            assert!(g.engaged(), "gate owns the queued release");
            g.tick(t0 + ms(500), &mut out);
            assert!(out.is_empty(), "not due yet");
            let due = t0 + PEN_GRAZE_MAX + PEN_ANCHOR;
            g.tick(due, &mut out);
            assert_eq!(out, vec![(Phase::Move, 102, 100)]);
            out.clear();
            g.tick(due + ms(33), &mut out);
            assert_eq!(out, vec![(Phase::Up, 102, 100)]);
            assert!(!g.engaged());
            assert!(g.in_grace(due + ms(100)));
        }

        #[test]
        fn pen_gate_fast_swipe_becomes_short_highlight() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            g.on_down(t0, 100, 100, &mut out);
            g.on_move(t0 + ms(50), 160, 100, &mut out); // drag starts → anchor
            out.clear();
            g.on_up(t0 + ms(200), 240, 100, &mut out); // lifted long before the anchor releases
            assert!(out.is_empty(), "release queued — a swipe can never click/page-turn");
            let due = t0 + ms(50) + PEN_ANCHOR;
            g.tick(due, &mut out);
            g.tick(due + ms(33), &mut out);
            assert_eq!(
                out,
                vec![(Phase::Move, 240, 100), (Phase::Up, 240, 100)],
                "completes as a short highlight to the lift point"
            );
        }

        #[test]
        fn pen_gate_flush_completes_queued_release_before_new_down() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            g.on_down(t0, 100, 100, &mut out);
            g.tick(t0 + PEN_GRAZE_MAX, &mut out);
            out.clear();
            g.on_up(t0 + ms(300), 110, 100, &mut out); // queued release
            let re = t0 + ms(380);
            g.flush_pending_up(re, &mut out);
            assert_eq!(out, vec![(Phase::Move, 110, 100), (Phase::Up, 110, 100)]);
            out.clear();
            assert!(g.in_grace(re));
            g.on_down(re + ms(10), 300, 400, &mut out);
            assert_eq!(out, vec![(Phase::Down, 300, 400)], "rides grace as a live tap");
        }

        #[test]
        fn pen_gate_grace_expires_back_to_gating() {
            let ms = Duration::from_millis;
            let t0 = Instant::now();
            let mut g = PenGate::new();
            let mut out = Vec::new();
            // Full drag-highlight to arm grace.
            g.on_down(t0, 100, 100, &mut out);
            g.on_move(t0 + ms(30), 150, 100, &mut out);
            g.tick(t0 + ms(30) + PEN_ANCHOR, &mut out);
            g.on_up(t0 + ms(1200), 200, 100, &mut out);
            out.clear();
            // In grace: a tap passes both events (the colour pick).
            let in_grace = t0 + ms(1300);
            g.on_down(in_grace, 400, 500, &mut out);
            g.on_up(in_grace + ms(80), 400, 500, &mut out);
            assert_eq!(out, vec![(Phase::Down, 400, 500), (Phase::Up, 400, 500)]);
            out.clear();
            // Past expiry: gating resumes.
            let expired = in_grace + ms(80) + PEN_GRACE + ms(1);
            assert!(!g.in_grace(expired));
            g.on_down(expired, 100, 100, &mut out);
            assert!(out.is_empty(), "back to gating after grace expires");
        }

        #[test]
        fn attach_handshake_parses_key() {
            assert_eq!(parse_attach_line("attach 1014606\n"), Some(1014606));
            assert_eq!(parse_attach_line("  attach 7 "), Some(7));
            assert_eq!(parse_attach_line("attach"), None);
            assert_eq!(parse_attach_line("attach abc"), None);
            assert_eq!(parse_attach_line("detach 5"), None);
            assert_eq!(parse_attach_line(""), None);
        }

        #[test]
        fn boot_gate_fires_on_the_latest_frame_not_only_on_arrival() {
            let s = Duration::from_secs;
            // The 45s-boot regression: progress finished and an inky frame is CACHED — the gate
            // must pass on any later tick, no new frame required.
            assert!(boot_gate(100, s(10), 3, Some(200), s(5)));
            // Progress still in the network phase → not booted, however inky the frame.
            assert!(!boot_gate(80, s(10), 3, Some(200), s(5)));
            // No frame at all since the load began: lp==100 is the C side's pre-load initial
            // value — never trust it alone.
            assert!(!boot_gate(100, s(10), 0, None, s(5)));
            // Too early after the (re)load started.
            assert!(!boot_gate(100, Duration::from_millis(500), 1, Some(200), s(5)));
            // Sparse text page under the main threshold: accepted only once it has held stable.
            assert!(!boot_gate(100, s(10), 3, Some(BOOT_INK_MIN), Duration::from_millis(100)));
            assert!(boot_gate(100, s(10), 3, Some(BOOT_INK_MIN), BOOT_STABLE));
            // Near-blank page: still-churning renders stay covered…
            assert!(!boot_gate(100, s(10), 3, Some(BOOT_INK_MIN - 1), Duration::from_millis(500)));
            // …but once loading is done and the render has SETTLED, it reveals whatever its ink —
            // a chapter-break page below the ink floors must not sit on the splash until the
            // backstop (failed navigations retry before ever reaching the gate).
            assert!(boot_gate(100, s(10), 3, Some(BOOT_INK_MIN - 1), BOOT_SETTLED_STILL));
            assert!(boot_gate(100, BOOT_SETTLED_MIN, 1, Some(0), BOOT_SETTLED_STILL));
            // The settled arm still needs the load finished and the minimum elapsed time.
            assert!(!boot_gate(80, s(10), 3, Some(0), s(60)));
            assert!(!boot_gate(100, Duration::from_millis(500), 1, Some(0), s(60)));
        }

        #[test]
        fn boot_status_progresses_and_never_regresses_visibly() {
            assert_eq!(BootStatus::derive(40, 0, false), BootStatus::Connecting);
            assert_eq!(BootStatus::derive(100, 0, false), BootStatus::Rendering);
            assert_eq!(BootStatus::derive(40, 1, false), BootStatus::Rendering);
            assert_eq!(BootStatus::derive(100, 9, true), BootStatus::AlmostReady);
            assert_eq!(BootStatus::Connecting.label(), "Connecting…");
        }

        #[test]
        fn rotate_probe_verifies_layout_and_canvas_width() {
            // Landscape target: 848 logical. Viewport + canvas both spanning → reflowed.
            assert_eq!(rotate_probe_ok("848,830,2", 848), Some(true));
            // Viewport reflowed but the book canvas is still portrait-sized — the exact bug the
            // user saw (landscape frame, portrait-width content): NOT verified.
            assert_eq!(rotate_probe_ok("848,410,2", 848), Some(false));
            // Viewport itself still portrait-width: not reflowed.
            assert_eq!(rotate_probe_ok("530,520,1", 848), Some(false));
            // Library page (no canvases): viewport match is enough.
            assert_eq!(rotate_probe_ok("848,0,0", 848), Some(true));
            // Rounding slop tolerated.
            assert_eq!(rotate_probe_ok("846,800,1", 848), Some(true));
            // Garbage → no answer (caller falls back to the frame heuristic).
            assert_eq!(rotate_probe_ok("undefined", 848), None);
            assert_eq!(rotate_probe_ok("", 848), None);
        }


        #[test]
        fn frame_ink_permille_separates_blank_from_content() {
            // All white (BGRA) → no ink.
            let white = vec![255u8; 954 * 64 * 4];
            assert!(frame_ink_permille(&white) < BOOT_INK_PERMILLE);
            // Top half black → roughly half the samples are ink, far past the boot threshold.
            let mut half = white.clone();
            let mid = half.len() / 2;
            for b in half[..mid].iter_mut() {
                *b = 0;
            }
            let p = frame_ink_permille(&half);
            assert!(p > 300, "half-ink frame should read high, got {p}‰");
            assert!(p >= BOOT_INK_PERMILLE);
            // Empty frame is safely "no ink".
            assert_eq!(frame_ink_permille(&[]), 0);
        }

        #[test]
        fn ink_px_keeps_hue_and_makes_tints_vivid() {
            // Greys pass through exactly (body text, paper).
            assert_eq!(ink_px(255, 255, 255), [255, 255, 255]);
            assert_eq!(ink_px(0, 0, 0), [0, 0, 0]);
            assert_eq!(ink_px(128, 128, 128), [128, 128, 128]);
            // Kindle's pale yellow: stays YELLOW — r,g bright, blue crushed — and distinct from
            // paper (the old grey-collapse made every palette swatch look alike).
            let y = ink_px(255, 245, 157);
            assert!(y[0] > 180 && y[1] > 180, "yellow stays bright, got {y:?}");
            assert!(y[2] < 120, "blue channel crushed → saturated yellow, got {y:?}");
            // Pale selection blue: stays blue-dominant and visible.
            let s = ink_px(180, 215, 255);
            assert!(s[2] > s[0] + 40, "hue preserved (blue dominant), got {s:?}");
            assert!(s[0] < 200, "tint deepened enough to see, got {s:?}");
            // Colourful input no longer collapses to uniform grey.
            let p = ink_px(200, 120, 40);
            assert!(!(p[0] == p[1] && p[1] == p[2]), "hue kept, got {p:?}");
        }

        #[test]
        fn default_url_accepts_kindle_hosts_and_rejects_others() {
            let us = "https://read.amazon.com/kindle-library";
            assert_eq!(resolve_default_url(Some(us)), us);
            assert_eq!(resolve_default_url(Some("https://evil.example/")), DEFAULT_URL);
            assert_eq!(resolve_default_url(Some("https://read.amazon.com.evil.io/")), DEFAULT_URL);
            assert_eq!(resolve_default_url(None), DEFAULT_URL);
        }

        #[test]
        fn book_url_matcher_and_startup_pick() {
            // Book URLs: asin param (either case) or a reader path, on a read.amazon https host.
            assert!(is_book_url("https://read.amazon.co.uk/?asin=B00ABC1234"));
            assert!(is_book_url("https://read.amazon.co.uk/reader?somestate=1"));
            assert!(is_book_url("https://read.amazon.com/kp/notebook?ASIN=B0X"));
            // Not books / not trustworthy.
            assert!(!is_book_url("https://read.amazon.co.uk/kindle-library"));
            assert!(!is_book_url("http://read.amazon.co.uk/?asin=B0X")); // not https
            assert!(!is_book_url("https://evil.example/?asin=B0X")); // wrong host
            assert!(!is_book_url("https://read.amazon.co.uk/?asin=B0X and junk")); // whitespace
            // Unanchored-prefix bypasses: the host must END at the allowlisted name.
            assert!(!is_book_url("https://read.amazon.co.uk.evil.com/?asin=B0X"));
            assert!(!is_book_url("https://read.amazon.competitor.io/reader"));
            assert!(!is_book_url("https://read.amazon.co.ukX/?asin=B0X"));

            // Startup pick: explicit --url always wins; else a valid stored book; else the default.
            let stored = "https://read.amazon.co.uk/?asin=B00ABC1234";
            assert_eq!(pick_start_url("https://x", true, Some(stored)), "https://x");
            assert_eq!(pick_start_url(DEFAULT_URL, false, Some(stored)), stored);
            assert_eq!(pick_start_url(DEFAULT_URL, false, Some("garbage")), DEFAULT_URL);
            assert_eq!(pick_start_url(DEFAULT_URL, false, None), DEFAULT_URL);
        }

        #[test]
        fn resolve_scale_precedence_and_clamps() {
            assert_eq!(resolve_scale(None, None), SCALE_DEFAULT);
            assert_eq!(resolve_scale(Some("1.5"), Some("3")), 1.5); // flag beats env
            assert_eq!(resolve_scale(None, Some("1")), 1.0); // env fallback (legacy behaviour)
            assert_eq!(resolve_scale(Some("garbage"), None), SCALE_DEFAULT);
            assert_eq!(resolve_scale(Some("0.5"), None), SCALE_DEFAULT); // below range
            assert_eq!(resolve_scale(Some("9"), None), SCALE_DEFAULT); // above range
        }

        #[test]
        fn zoom_for_native_density_vs_legacy() {
            // Native density renders 1:1 — the sharp point.
            assert_eq!(zoom_for(2.0), 1.0);
            // Scale 1 (BROWSER_SCALE=1 rollback): the legacy enlarged layout.
            assert_eq!(zoom_for(1.0), 1.8);
        }

        #[test]
        fn frame_hash_changes_with_content() {
            let a = vec![0u8; 4096];
            let mut b = a.clone();
            b[0] = 1; // index 0 is always sampled (stride starts at 0)
            assert_ne!(frame_hash(&a), frame_hash(&b));
        }
    }
}
