//! Fetch + cache + prepare article images for the reader over WiFi. Raw bytes are cached on disk
//! (via the shared [`imagecache`] crate — the identical keying `vaultsync` pre-warms at sync time)
//! then decoded/scaled/dithered by [`reader::prepare`]. Any failure — network, HTTP error,
//! unsupported format, decode error — is simply dropped from the map, and the reading view falls
//! back to its labelled placeholder box. After a sync pre-warm the cache is already populated, so
//! this runs offline with no network.

use image::RgbImage;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Fetch (or load from cache) and prepare every URL, returning the ones that decoded to an image
/// scaled to fit `(max_w, max_h)`. Fetches run on a small worker pool so an image-heavy clipping
/// doesn't stall serially; missing entries are silently absent (→ placeholder in the view).
pub fn load(cache_dir: &Path, urls: &[String], max_w: u32, max_h: u32) -> HashMap<String, RgbImage> {
    if urls.is_empty() {
        return HashMap::new();
    }
    let client = imagecache::client();
    let next = AtomicUsize::new(0);
    let out: Mutex<HashMap<String, RgbImage>> = Mutex::new(HashMap::new());
    std::thread::scope(|s| {
        for _ in 0..8usize.min(urls.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(url) = urls.get(i) else { break };
                let Some(bytes) = imagecache::ensure_bytes(&client, cache_dir, url) else { continue };
                if let Some(img) = reader::prepare(&bytes, max_w, max_h) {
                    out.lock().unwrap().insert(url.clone(), img);
                }
            });
        }
    });
    out.into_inner().unwrap()
}
