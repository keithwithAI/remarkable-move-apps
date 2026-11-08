//! Shared on-disk cache for article/book images, used by both the reader (fetch at read time) and
//! `vaultsync` (pre-warm at sync time). Keeping the keying + fetch here is what makes the two agree
//! byte-for-byte: the reader writes `<cache_dir>/images/<sha256(url)>` and vaultsync fills the same
//! files ahead of time, so an offline open finds every image already local.
//!
//! Only raw bytes are cached — decoding/scaling/dithering stays in the reader (`reader::prepare`),
//! so this crate needs no image codec.

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const UA: &str = concat!("secondbrain-imagecache/", env!("CARGO_PKG_VERSION"));
const FETCH_TIMEOUT: Duration = Duration::from_secs(12);
const WORKERS: usize = 8;
/// How long a failed URL is skipped by [`prewarm`] before it's retried — so dead/slow links aren't
/// re-hammered (12s each) on every sync. Only the pre-warm honours this; the reader's read-time
/// fetch always retries, so a transient failure still loads the moment you open the doc online.
const FAIL_COOLDOWN: Duration = Duration::from_secs(7 * 24 * 3600);

/// The reader's cache directory for a given content directory: a `cache/` sibling of it. Mirrors
/// the reader's own derivation (`<--dir>.parent()/cache`), so a pre-warm targets the exact dir the
/// reader reads from.
pub fn cache_root(content_dir: &Path) -> PathBuf {
    content_dir.parent().unwrap_or(content_dir).join("cache")
}

/// The on-disk cache path for `url`'s raw bytes: `<cache_dir>/images/<sha256(url)>`.
pub fn cache_path(cache_dir: &Path, url: &str) -> PathBuf {
    let hex: String = Sha256::digest(url.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    cache_dir.join("images").join(hex)
}

/// Whether `url`'s bytes are already cached (non-empty file present).
pub fn is_cached(cache_dir: &Path, url: &str) -> bool {
    std::fs::metadata(cache_path(cache_dir, url)).map(|m| m.len() > 0).unwrap_or(false)
}

/// The skip-marker path for a URL that recently failed to fetch: `<cache_dir>/images/.fail/<hash>`.
fn fail_marker(cache_dir: &Path, url: &str) -> PathBuf {
    let hex: String = Sha256::digest(url.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    cache_dir.join("images").join(".fail").join(hex)
}

/// True when `url` has a skip-marker younger than [`FAIL_COOLDOWN`] — [`prewarm`] should not retry
/// it yet. A missing marker, or one older than the cooldown (or a clock that moved backwards), is
/// treated as "retry".
fn fail_fresh(cache_dir: &Path, url: &str) -> bool {
    std::fs::metadata(fail_marker(cache_dir, url))
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|age| age < FAIL_COOLDOWN).unwrap_or(false))
        .unwrap_or(false)
}

/// Write/refresh a URL's skip-marker (mtime = now).
fn mark_failed(cache_dir: &Path, url: &str) {
    let path = fail_marker(cache_dir, url);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, []);
}

/// Clear a URL's skip-marker (after a successful fetch).
fn clear_failed(cache_dir: &Path, url: &str) {
    let _ = std::fs::remove_file(fail_marker(cache_dir, url));
}

/// Cached bytes for `url`, else fetch over HTTP and cache them. `None` on any network/HTTP failure.
pub fn ensure_bytes(client: &reqwest::blocking::Client, cache_dir: &Path, url: &str) -> Option<Vec<u8>> {
    let path = cache_path(cache_dir, url);
    if let Ok(bytes) = std::fs::read(&path) {
        if !bytes.is_empty() {
            return Some(bytes);
        }
    }
    let resp = client.get(url).header(reqwest::header::USER_AGENT, UA).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().ok()?.to_vec();
    if bytes.is_empty() {
        return None;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &bytes);
    Some(bytes)
}

/// A blocking HTTP client tuned for image fetches (12s timeout). Falls back to the default client
/// if the builder fails.
pub fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Outcome of a [`prewarm`] run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Prewarmed {
    /// URLs freshly fetched and written to the cache this run.
    pub fetched: usize,
    /// URLs already present in the cache (skipped).
    pub cached: usize,
    /// URLs skipped because they failed recently (within [`FAIL_COOLDOWN`]).
    pub skipped: usize,
    /// URLs that failed this run (network/HTTP/empty) — marked to skip until the cooldown lapses.
    pub failed: usize,
}

/// Fetch every not-yet-cached URL into `<cache_dir>/images/` on a small worker pool. Best-effort:
/// failures are counted, never fatal. Already-cached URLs are skipped without a request, and URLs
/// that failed within [`FAIL_COOLDOWN`] are skipped too — so dead links aren't retried every sync.
pub fn prewarm(cache_dir: &Path, urls: &[String]) -> Prewarmed {
    if urls.is_empty() {
        return Prewarmed::default();
    }
    let client = client();
    let next = AtomicUsize::new(0);
    let fetched = AtomicUsize::new(0);
    let cached = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..WORKERS.min(urls.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(url) = urls.get(i) else { break };
                if is_cached(cache_dir, url) {
                    cached.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if fail_fresh(cache_dir, url) {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if ensure_bytes(&client, cache_dir, url).is_some() {
                    fetched.fetch_add(1, Ordering::Relaxed);
                    clear_failed(cache_dir, url);
                } else {
                    failed.fetch_add(1, Ordering::Relaxed);
                    mark_failed(cache_dir, url);
                }
            });
        }
    });
    Prewarmed {
        fetched: fetched.into_inner(),
        cached: cached.into_inner(),
        skipped: skipped.into_inner(),
        failed: failed.into_inner(),
    }
}

/// Distinct `http(s)` image URLs referenced by a Markdown document, in first-seen order. Uses a
/// CommonMark pass so it selects exactly what the reader's parser renders as an image — inline
/// `![alt](url)` and reference images — and skips `data:`/relative/local URLs, matching the reader's
/// `starts_with("http")` filter.
pub fn image_urls_in_markdown(src: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Parser, Tag};
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for event in Parser::new(src) {
        if let Event::Start(Tag::Image { dest_url, .. }) = event {
            let url = dest_url.into_string();
            if url.starts_with("http") && seen.insert(url.clone()) {
                out.push(url);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_root_is_a_cache_sibling_of_the_content_dir() {
        assert_eq!(
            cache_root(Path::new("/home/root/secondbrain/vault")),
            Path::new("/home/root/secondbrain/cache")
        );
        // A subfolder --dir keys off ITS parent (documented alternative setup).
        assert_eq!(
            cache_root(Path::new("/home/root/secondbrain/vault/Clippings")),
            Path::new("/home/root/secondbrain/vault/cache")
        );
    }

    #[test]
    fn cache_path_is_sha256_hex_under_images() {
        // sha256("https://example.com/a.png") — stable, shared with the reader's keying.
        let p = cache_path(Path::new("/c"), "https://example.com/a.png");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(p.parent().unwrap(), Path::new("/c/images"));
        assert_eq!(name.len(), 64);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        // Identical URL → identical path (the whole point).
        assert_eq!(p, cache_path(Path::new("/c"), "https://example.com/a.png"));
    }

    #[test]
    fn extracts_http_images_deduped_skipping_non_http() {
        let md = "\
# Title

![one](https://ex.com/1.png) and ![two](http://ex.com/2.jpg)

![dup](https://ex.com/1.png)

![local](images/x.png) ![data](data:image/png;base64,AAAA) ![rel](../y.gif)

`![incode](https://ex.com/z.png)`
";
        let urls = image_urls_in_markdown(md);
        assert_eq!(urls, vec!["https://ex.com/1.png".to_string(), "http://ex.com/2.jpg".to_string()]);
    }

    #[test]
    fn empty_and_no_image_docs_yield_nothing() {
        assert!(image_urls_in_markdown("").is_empty());
        assert!(image_urls_in_markdown("just prose, no pictures").is_empty());
        assert_eq!(prewarm(Path::new("/nonexistent"), &[]), Prewarmed::default());
    }

    #[test]
    fn fail_marker_marks_skips_and_clears() {
        // Unique per-test dir so parallel tests don't collide.
        let dir = std::env::temp_dir().join("imagecache-test-failmarker");
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://dead.example.com/gone.png";

        // No marker yet → not fresh (would retry).
        assert!(!fail_fresh(&dir, url));
        // After marking, it's fresh → prewarm would skip it.
        mark_failed(&dir, url);
        assert!(fail_fresh(&dir, url));
        // A different URL is unaffected.
        assert!(!fail_fresh(&dir, "https://ok.example.com/a.png"));
        // Clearing (on a later success) makes it retryable again.
        clear_failed(&dir, url);
        assert!(!fail_fresh(&dir, url));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
