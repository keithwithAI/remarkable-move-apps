//! Safe-ish Rust wrapper over the `wpeengine` C ABI (engine/wpeengine.{c,h}).
//!
//! The engine owns a GLib main loop on its own thread; every method here is safe to call from the
//! app's main thread. Frames come back as packed BGRA (4 bytes/px, top-down, stride = width*4).
//! Linux/device only — WPE is not present on the Mac host.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

#[allow(non_camel_case_types)]
type WpeEngine = std::ffi::c_void;

extern "C" {
    fn wpe_engine_new(width: u32, height: u32, scale: f32, data_dir: *const c_char) -> *mut WpeEngine;
    fn wpe_engine_load_uri(e: *mut WpeEngine, uri: *const c_char);
    fn wpe_engine_reload(e: *mut WpeEngine);
    fn wpe_engine_run_js(e: *mut WpeEngine, js: *const c_char);
    fn wpe_engine_eval_js(e: *mut WpeEngine, js: *const c_char);
    fn wpe_engine_js_result(e: *mut WpeEngine, buf: *mut c_char, len: usize) -> u32;
    fn wpe_engine_set_zoom(e: *mut WpeEngine, zoom: f64);
    fn wpe_engine_take_frame(e: *mut WpeEngine, dst: *mut u8, dst_len: usize) -> c_int;
    fn wpe_engine_touch(e: *mut WpeEngine, phase: c_int, x: i32, y: i32);
    fn wpe_engine_key(e: *mut WpeEngine, keysym: u32, pressed: c_int);
    fn wpe_engine_current_uri(e: *mut WpeEngine, buf: *mut c_char, len: usize);
    fn wpe_engine_load_progress(e: *mut WpeEngine) -> c_int;
    fn wpe_engine_load_failed(e: *mut WpeEngine) -> c_int;
    fn wpe_engine_kb_focus(e: *mut WpeEngine) -> c_int;
    fn wpe_engine_set_active(e: *mut WpeEngine, active: c_int);
    fn wpe_engine_resize(e: *mut WpeEngine, scale: f32, width: u32, height: u32);
    fn wpe_engine_destroy(e: *mut WpeEngine);
}

/// Touch phase forwarded to the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    Down = 0,
    Move = 1,
    Up = 2,
}

/// The X keysyms the on-screen keyboard sends.
pub mod keysym {
    pub const RETURN: u32 = 0xFF0D;
    pub const BACKSPACE: u32 = 0xFF08;
    pub const SPACE: u32 = 0x20;
}

pub struct Engine {
    ptr: *mut WpeEngine,
}

impl Engine {
    /// `width`/`height` are PHYSICAL output pixels (the frame size); `scale` is the device scale
    /// factor — the page sees devicePixelRatio == scale and lays out at width/scale CSS px, so
    /// content rasterizes at native panel density instead of being bitmap-upscaled by page zoom.
    pub fn new(width: u32, height: u32, scale: f64, data_dir: Option<&str>) -> anyhow::Result<Engine> {
        // Don't panic on an interior NUL — this fn returns Result, so surface it as an error.
        let cdir = data_dir
            .map(CString::new)
            .transpose()
            .map_err(|_| anyhow::anyhow!("data_dir contains an interior NUL byte"))?;
        let dptr = cdir.as_ref().map(|c| c.as_ptr()).unwrap_or(std::ptr::null());
        let ptr = unsafe { wpe_engine_new(width, height, scale as f32, dptr) };
        if ptr.is_null() {
            anyhow::bail!("wpe_engine_new failed (EGL/WPE init — check env.sh + glvnd vendor json)");
        }
        Ok(Engine { ptr })
    }

    pub fn load_uri(&self, uri: &str) {
        match CString::new(uri) {
            Ok(c) => unsafe { wpe_engine_load_uri(self.ptr, c.as_ptr()) },
            Err(_) => log::warn!("load_uri: ignoring URL with an interior NUL byte"),
        }
    }
    pub fn reload(&self) {
        unsafe { wpe_engine_reload(self.ptr) };
    }
    pub fn run_js(&self, js: &str) {
        match CString::new(js) {
            Ok(c) => unsafe { wpe_engine_run_js(self.ptr, c.as_ptr()) },
            Err(_) => log::warn!("run_js: ignoring script with an interior NUL byte"),
        }
    }
    /// Run JavaScript whose stringified result is captured engine-side; poll [`Self::js_result`]
    /// for a bumped sequence number to read it.
    pub fn eval_js(&self, js: &str) {
        match CString::new(js) {
            Ok(c) => unsafe { wpe_engine_eval_js(self.ptr, c.as_ptr()) },
            Err(_) => log::warn!("eval_js: ignoring script with an interior NUL byte"),
        }
    }
    /// Latest [`Self::eval_js`] result: `(seq, value)` — `seq` bumps once per completed script
    /// (0 = none yet), so callers detect freshness by comparing against the last seen seq.
    pub fn js_result(&self) -> (u32, String) {
        let mut buf = vec![0u8; 256];
        let seq = unsafe { wpe_engine_js_result(self.ptr, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        (seq, String::from_utf8_lossy(&buf[..end]).into_owned())
    }
    pub fn set_zoom(&self, zoom: f64) {
        unsafe { wpe_engine_set_zoom(self.ptr, zoom) };
    }

    /// Copy the latest frame into `dst` (must be width*height*4). Returns true if a new frame was
    /// available since the previous call.
    pub fn take_frame(&self, dst: &mut [u8]) -> bool {
        unsafe { wpe_engine_take_frame(self.ptr, dst.as_mut_ptr(), dst.len()) == 1 }
    }

    pub fn touch(&self, phase: Touch, x: i32, y: i32) {
        unsafe { wpe_engine_touch(self.ptr, phase as c_int, x, y) };
    }
    /// Press-then-release a key (a tap).
    pub fn tap_key(&self, keysym: u32) {
        unsafe {
            wpe_engine_key(self.ptr, keysym, 1);
            wpe_engine_key(self.ptr, keysym, 0);
        }
    }

    pub fn current_uri(&self) -> String {
        let mut buf = vec![0u8; 2048];
        unsafe { wpe_engine_current_uri(self.ptr, buf.as_mut_ptr() as *mut c_char, buf.len()) };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }
    pub fn load_progress(&self) -> i32 {
        unsafe { wpe_engine_load_progress(self.ptr) }
    }
    /// True after a navigation failed (DNS/offline); cleared when the next load starts.
    pub fn load_failed(&self) -> bool {
        unsafe { wpe_engine_load_failed(self.ptr) == 1 }
    }
    /// True while an editable element in the page is focused — the page "wants a keyboard".
    pub fn kb_focus(&self) -> bool {
        unsafe { wpe_engine_kb_focus(self.ptr) == 1 }
    }
    /// Foreground/background the view: `false` drops WebKit's activity states so it throttles
    /// rendering and timers while detached (the warm-daemon idle state); `true` restores them.
    pub fn set_active(&self, active: bool) {
        unsafe { wpe_engine_set_active(self.ptr, if active { 1 } else { 0 }) };
    }
    /// Re-size/re-scale the live view (rotation): frames become `width`×`height` physical px at
    /// `scale`; the page reflows. The caller must resize its own frame buffer accordingly.
    pub fn resize(&self, scale: f64, width: u32, height: u32) {
        unsafe { wpe_engine_resize(self.ptr, scale as f32, width, height) };
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { wpe_engine_destroy(self.ptr) };
    }
}

// The engine pointer is only ever used from the app's main thread; the C side does its own
// internal locking + loop-thread marshalling.
unsafe impl Send for Engine {}

#[cfg(test)]
mod navigation_tests {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    extern "C" {
        fn wpe_engine_uri_allowed(uri: *const c_char) -> c_int;
    }

    fn allowed(uri: &str) -> bool {
        let c = CString::new(uri).unwrap();
        unsafe { wpe_engine_uri_allowed(c.as_ptr()) != 0 }
    }

    #[test]
    fn amazon_pages_are_allowed() {
        for u in [
            "https://read.amazon.co.uk/kindle-library",
            "https://read.amazon.com/?asin=B00ABC1234",
            "https://www.amazon.co.uk/ap/signin?openid=1",
            "https://WWW.Amazon.Com/ap/signin",
            "https://m.media-amazon.com/images/x.js",
            "https://amazon.de/",
            "about:blank",
            "blob:https://read.amazon.com/1234",
        ] {
            assert!(allowed(u), "{u} should be allowed");
        }
    }

    #[test]
    fn everything_else_is_blocked() {
        for u in [
            "https://evil.example/",
            "https://evilamazon.com/",
            "https://amazon.com.evil.io/",
            "https://read.amazon.co.uk.evil.com/",
            "https://amazon.com@evil.example/",
            "http://read.amazon.com/",
            "file:///home/root/secondbrain/config/todoist.token",
            "blob:http://evil.example/1",
            "not a url",
            "",
        ] {
            assert!(!allowed(u), "{u} should be blocked");
        }
    }
}
