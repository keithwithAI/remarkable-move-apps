//! `vaultsync` — Phase 2. One Git sync cycle for the Obsidian vault: commit local highlight
//! edits, pull new articles (Web Clipper), push highlights back. Run from a systemd timer and
//! on network-up. Offline-safe: with no network it just commits locally (the offline queue).
//!
//! With a `[books]` section configured, the same cycle then pulls the Kindle-highlights books
//! repo (read-only on device — the device never edits books, so this is pull-only in practice;
//! the shared `sync_vault` would push only if the clone were somehow ahead).

use anyhow::{Context, Result};
use sb_core::creds::{Config, VaultCfg};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let (config_path, message) = parse_args();
    ensure_ssh_home();

    let cfg = Config::load(&config_path).with_context(|| format!("loading {}", config_path.display()))?;
    cfg.vault.repo_url.as_deref().context("[vault] repo_url is not set in config.toml")?;

    let vault = sync_section(&cfg, &cfg.vault, "vault", &message);
    // Books are optional and best-effort: a books failure must not mask a good vault sync in the
    // exit code the reader's ↺ watches — but it must not be silent either, so it still fails the
    // run when the vault itself was fine.
    let books = if cfg.books.repo_url.is_some() { Some(sync_section(&cfg, &cfg.books, "books", &message)) } else { None };

    // After a successful pull (⇒ we were online), pre-warm article/book images into the reader's
    // on-disk cache so it opens fully offline. Best-effort — never touches the exit code the
    // reader's ↺ watches. Skipped when the vault sync failed (offline: the fetches would just fail).
    if vault.is_ok() {
        prewarm_images(&cfg);
    }

    match (vault, books) {
        (Err(e), _) => {
            // A sync failure (offline, conflict) is expected sometimes — local commits are safe.
            log::warn!("vault sync incomplete: {e:#}");
            Err(e)
        }
        (Ok(()), Some(Err(e))) => {
            log::warn!("books sync incomplete: {e:#}");
            Err(e)
        }
        _ => Ok(()),
    }
}

/// Where root's SSH material actually lives on the device. Not `$HOME` — see [`ensure_ssh_home`].
const DEVICE_HOME: &str = "/home/root";

/// Point `$HOME` at a directory that actually holds `.ssh/known_hosts`, when it doesn't already.
///
/// libgit2's SSH transport resolves `known_hosts` from `$HOME`, and systemd only derives `HOME`
/// from `/etc/passwd` when the unit sets `User=`. `vaultsync.service` doesn't (it is already root),
/// so it inherited PID 1's `HOME=/`, `//.ssh/known_hosts` didn't exist, and **every** timer run
/// died with `error loading known_hosts: ; class=Ssh (23)` — 136 runs, 0 successes, unnoticed for
/// weeks because the only path anyone watched was `reader-app` spawning us, which passes a real
/// `HOME=/home/root`.
///
/// The unit now sets `Environment=HOME=/home/root`, which is the proper fix. This is the belt to
/// its braces: we are also spawned by the reader and run by hand, and any launcher with a thin
/// environment would resurrect the same silent failure.
fn ensure_ssh_home() {
    let current = std::env::var("HOME").ok();
    if let Some(home) = usable_home(current.as_deref(), |p| p.is_file()) {
        log::info!("HOME={:?} has no .ssh/known_hosts — using {} for git SSH", current, home.display());
        // Safety: single-threaded here (called first thing in main, before any git or thread work).
        std::env::set_var("HOME", &home);
    }
}

/// Decide whether `$HOME` needs redirecting, and to what. `Some(path)` = caller should set `HOME`
/// to it; `None` = leave `HOME` alone. Pure, with the filesystem behind `probe`, so it is testable.
///
/// Only rewrites when the current `HOME` is *unusable for SSH* — absent, empty, `/`, or simply
/// missing `.ssh/known_hosts` — and only when [`DEVICE_HOME`] actually has the file. It never
/// overrides a `HOME` that already works, so a Mac dev run or a differently-provisioned device is
/// left untouched.
fn usable_home(current: Option<&str>, probe: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let known_hosts = |dir: &str| Path::new(dir).join(".ssh").join("known_hosts");
    if let Some(h) = current.filter(|h| !h.is_empty()) {
        if probe(&known_hosts(h)) {
            return None; // the environment we were given is fine
        }
    }
    let fallback = known_hosts(DEVICE_HOME);
    probe(&fallback).then(|| PathBuf::from(DEVICE_HOME))
}

/// One sync cycle for a `[vault]`-shaped config section; `default_local` names the clone dir
/// (relative paths resolve against the config file's directory, matching `[vault]`'s behavior).
fn sync_section(cfg: &Config, section: &VaultCfg, default_local: &str, message: &str) -> Result<()> {
    let url = section.repo_url.clone().with_context(|| format!("[{default_local}] repo_url is not set"))?;
    let local = cfg.resolve(section.local.clone().unwrap_or_else(|| PathBuf::from(default_local)).as_path());
    let branch = section.branch.clone();
    let ssh_key = section.ssh_key.clone().map(|k| cfg.resolve(&k));

    let report = syncqueue::sync_vault(&url, &local, branch.as_deref(), ssh_key, message)
        .map_err(explain_known_hosts)?;
    println!(
        "{default_local} sync: committed={}, pull={:?}, pushed={} → {}",
        report.committed.map(|o| o.to_string()).unwrap_or_else(|| "—".into()),
        report.outcome,
        report.pushed,
        local.display(),
    );
    Ok(())
}

/// Turn libgit2's `error loading known_hosts: ; class=Ssh (23)` into something that names its own
/// cause. The reason field is *empty*, so the raw error says nothing about `$HOME` — which is why
/// 136 consecutive failures read as noise for weeks rather than as one obvious misconfiguration.
fn explain_known_hosts(e: anyhow::Error) -> anyhow::Error {
    if !format!("{e:#}").contains("known_hosts") {
        return e;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "<unset>".into());
    e.context(format!(
        "SSH host-key check could not read known_hosts. libgit2 looks for $HOME/.ssh/known_hosts, \
         and $HOME is {home:?} (probed {home}/.ssh/known_hosts). If this is the systemd unit, it \
         needs Environment=HOME={DEVICE_HOME}; otherwise seed {DEVICE_HOME}/.ssh/known_hosts with \
         GitHub's host keys."
    ))
}

/// Best-effort: after a pull, fetch every article/book image into the reader's on-disk cache so it
/// opens fully offline. Never fails the sync — image fetches are advisory, and the reader still
/// falls back to a read-time fetch when online. The first run after this ships backfills all
/// existing docs; later runs only fetch newly-added images (cached ones are skipped fast).
fn prewarm_images(cfg: &Config) {
    // Key off the SAME dir the reader derives its cache from: `<content dir>.parent()/cache`, where
    // the content dir is the resolved [vault].local (== the reader's --dir on device).
    let vault_dir =
        cfg.resolve(cfg.vault.local.clone().unwrap_or_else(|| PathBuf::from("vault")).as_path());
    let cache_dir = imagecache::cache_root(&vault_dir);

    let mut content_dirs = vec![vault_dir];
    if cfg.books.repo_url.is_some() {
        content_dirs.push(
            cfg.resolve(cfg.books.local.clone().unwrap_or_else(|| PathBuf::from("books")).as_path()),
        );
    }

    let mut urls: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in &content_dirs {
        for md in markdown_files(dir) {
            let Ok(src) = std::fs::read_to_string(&md) else { continue };
            for u in imagecache::image_urls_in_markdown(&src) {
                if seen.insert(u.clone()) {
                    urls.push(u);
                }
            }
        }
    }
    if urls.is_empty() {
        return;
    }
    let r = imagecache::prewarm(&cache_dir, &urls);
    log::info!(
        "image prewarm: {} fetched, {} cached, {} skipped (recent fail), {} failed ({} urls) → {}",
        r.fetched,
        r.cached,
        r.skipped,
        r.failed,
        urls.len(),
        cache_dir.join("images").display(),
    );
}

/// Recursively list `*.md` files under `dir` (best-effort; unreadable dirs skipped; dot-dirs like
/// `.git` are not descended).
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let dot = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
                if !dot {
                    stack.push(p);
                }
            } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
                out.push(p);
            }
        }
    }
    out
}

fn parse_args() -> (PathBuf, String) {
    let mut config = PathBuf::from("/home/root/secondbrain/config/config.toml");
    let mut message = "secondbrain: sync highlights".to_string();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => {
                if let Some(p) = it.next() {
                    config = p.into();
                }
            }
            "-m" | "--message" => {
                if let Some(m) = it.next() {
                    message = m;
                }
            }
            "-V" | "--version" => {
                println!(env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                eprintln!("vaultsync [--config <config.toml>] [-m <commit message>]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    (config, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `probe` stands in for the filesystem: only the listed paths "exist".
    fn probe_for(existing: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p: &Path| existing.contains(&p.to_str().unwrap_or_default())
    }
    const DEVICE_KH: &str = "/home/root/.ssh/known_hosts";

    /// The regression this whole change exists for: systemd hands a root service PID 1's
    /// environment, `HOME=/`, and `//.ssh/known_hosts` does not exist. Every fetch died there.
    #[test]
    fn systemd_home_slash_is_redirected_to_the_device_home() {
        let got = usable_home(Some("/"), probe_for(&[DEVICE_KH]));
        assert_eq!(got.as_deref(), Some(Path::new("/home/root")));
    }

    #[test]
    fn unset_or_empty_home_is_redirected_too() {
        for current in [None, Some("")] {
            let got = usable_home(current, probe_for(&[DEVICE_KH]));
            assert_eq!(got.as_deref(), Some(Path::new("/home/root")), "current={current:?}");
        }
    }

    /// A HOME that already resolves known_hosts must be left alone — this is how `reader-app`
    /// spawns us (HOME=/home/root), and how a Mac dev run works.
    #[test]
    fn a_working_home_is_never_overridden() {
        assert_eq!(usable_home(Some("/home/root"), probe_for(&[DEVICE_KH])), None);
        // A different, equally valid home stays put rather than being dragged to /home/root.
        assert_eq!(
            usable_home(Some("/Users/someone"), probe_for(&["/Users/someone/.ssh/known_hosts", DEVICE_KH])),
            None
        );
    }

    /// Nothing to fall back to → leave HOME alone and let libgit2 report it (now with the
    /// `explain_known_hosts` context attached), rather than silently pointing somewhere useless.
    #[test]
    fn no_known_hosts_anywhere_leaves_home_untouched() {
        assert_eq!(usable_home(Some("/"), probe_for(&[])), None);
        assert_eq!(usable_home(None, probe_for(&[])), None);
    }
}
