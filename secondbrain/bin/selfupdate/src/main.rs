//! `selfupdate` — pull new tool binaries over WiFi from a GitHub Release and install them.
//!
//! Flow: read the installed version → GET the latest (or pinned) release via the GitHub API →
//! if newer, download the `manifest.json` asset (`{version, binaries:[{name, sha256}],
//! icons:[{app, sha256}]}`) and the named binary assets → **verify each sha256** → atomically
//! swap into `bin/` keeping a `.bak` → `--version` smoke-test each → on failure roll back; on
//! success write `VERSION`, restart the systemd timers, and best-effort install the launcher
//! icons (release assets `icon-<app>.png`) into the AppLoad dirs — cosmetic, so an icon failure
//! never fails or rolls back a binary update. Offline-safe: a failed fetch just exits non-zero
//! and the next timer retries.
//!
//! This runs as root and installs code that runs as root, so downloads are checksum-verified and
//! the release metadata is fetched over HTTPS from GitHub. (A signature step can be layered on.)

use anyhow::{bail, Context, Result};
use sb_core::creds::Config;
use std::path::{Path, PathBuf};

const UA: &str = concat!("secondbrain-selfupdate/", env!("CARGO_PKG_VERSION"));
const BINARIES_DEFAULT: &str = "/home/root/secondbrain/bin";
const VERSION_DEFAULT: &str = "/home/root/secondbrain/VERSION";
/// Where AppLoad reads each app's launcher entry (icon.png lives per-app under here). Icons only
/// take visual effect after the next xovi re-injection (AppLoad caches them at injection time).
const APPLOAD_DEFAULT: &str = "/home/root/xovi/exthome/appload";

struct Args {
    config: PathBuf,
    force: bool,
    dry_run: bool,
}

#[derive(serde::Deserialize)]
struct Manifest {
    version: String,
    #[serde(default)]
    binaries: Vec<BinEntry>,
    /// Launcher icons (release assets `icon-<app>.png`), installed into the AppLoad dirs.
    /// Optional — old manifests don't carry it, and old selfupdates ignore it (serde skips
    /// unknown fields), so the format change is compatible both ways.
    #[serde(default)]
    icons: Vec<IconEntry>,
}
#[derive(serde::Deserialize)]
struct BinEntry {
    name: String,
    sha256: String,
}
#[derive(serde::Deserialize)]
struct IconEntry {
    app: String,
    sha256: String,
}

struct Asset {
    name: String,
    url: String,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

    let cfg = Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    let repo = cfg
        .update
        .repo
        .clone()
        .context("[update] repo not set (e.g. \"keithwithAI/remarkable-move-apps\")")?;
    let token = cfg.update_token()?;
    let bin_dir = cfg.update.bin_dir.clone().unwrap_or_else(|| PathBuf::from(BINARIES_DEFAULT));
    let version_file = cfg.update.version_file.clone().unwrap_or_else(|| PathBuf::from(VERSION_DEFAULT));

    // One selfupdate at a time: the update timer and both apps' on-launch checks can overlap, and
    // two concurrent swap loops would clobber each other's `.bak`s. flock releases on process exit,
    // so a crashed run never leaves a stale lock.
    let _lock = match acquire_lock(&bin_dir)? {
        Some(f) => f,
        None => {
            println!("another selfupdate is already running — exiting");
            return Ok(());
        }
    };
    sweep_orphans(&bin_dir);

    let local_version = std::fs::read_to_string(&version_file)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let (tag, assets) = fetch_release(&client, &repo, token.as_deref(), &cfg.update.channel)?;
    log::info!("installed {local_version}, latest release {tag}");

    if !args.force && !is_newer(&tag, &local_version) {
        println!("up to date ({local_version})");
        return Ok(());
    }

    // Manifest (sha256 of each binary) is published as a release asset.
    let manifest_asset = assets
        .iter()
        .find(|a| a.name == "manifest.json")
        .context("release has no manifest.json asset")?;
    let manifest: Manifest = serde_json::from_slice(&download(&client, &manifest_asset.url, token.as_deref())?)
        .context("parsing manifest.json")?;

    // Download + verify every binary BEFORE touching anything on disk.
    let mut staged: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in &manifest.binaries {
        let asset = assets
            .iter()
            .find(|a| a.name == entry.name)
            .with_context(|| format!("manifest lists {:?} but the release has no such asset", entry.name))?;
        let data = download(&client, &asset.url, token.as_deref())?;
        let got = sha256_hex(&data);
        if !got.eq_ignore_ascii_case(entry.sha256.trim()) {
            bail!("sha256 mismatch for {}: manifest {} != download {}", entry.name, entry.sha256, got);
        }
        log::info!("verified {} ({} bytes)", entry.name, data.len());
        staged.push((entry.name.clone(), data));
    }
    if staged.is_empty() {
        bail!("manifest lists no binaries");
    }

    if args.dry_run {
        println!("dry-run: would update {} → {} ({} binaries)", local_version, manifest.version, staged.len());
        return Ok(());
    }

    // Install atomically (temp → rename), keeping a .bak of each; a mid-loop failure rolls back
    // the already-swapped subset so bin/ is never left mixed-version.
    let installed = install_all(&bin_dir, &staged)?;

    // Smoke-test each new binary; roll all back on any failure.
    for inst in &installed {
        let dest = bin_dir.join(&inst.name);
        if let Err(e) = smoke_test(&dest) {
            log::error!("smoke test failed for {}: {e}; rolling back", inst.name);
            rollback(&bin_dir, &installed);
            bail!("update rolled back after smoke-test failure on {}", inst.name);
        }
    }

    if let Err(e) = std::fs::write(&version_file, format!("{}\n", manifest.version)) {
        // Not fatal (the binaries ARE installed), but without VERSION advancing, every future
        // trigger re-downloads and re-swaps this same release — make that visible.
        log::error!("VERSION write failed ({e}) — updates will re-install until it succeeds");
    }
    restart_timers();

    // Launcher icons: cosmetic, so best-effort AFTER the binary swap is home safe — an icon
    // failure must never fail (or roll back) a binary update. Installed atomically per icon;
    // they show after the next xovi re-injection.
    let icons_done = install_icons(&client, &assets, &manifest.icons, token.as_deref());
    println!(
        "updated {} → {} ({} binaries, {} icons)",
        local_version,
        manifest.version,
        staged.len(),
        icons_done
    );
    Ok(())
}

/// An icon's `app` field becomes a path component under the appload root — accept only plain
/// ascii-alphanumeric/dash/underscore names so `../` (or anything else creative) can't escape.
fn safe_app_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Download, verify, and atomically install each manifest icon into the AppLoad app dirs.
/// Best-effort: any failure logs a warning and moves on. Returns the number installed. Skips
/// apps whose AppLoad dir doesn't exist (AppLoad not installed / app not deployed) and rejects
/// path-escaping app names — the manifest is trusted-ish (sha-verified release) but this runs
/// as root, so a `../` can never leave the appload tree.
fn install_icons(
    client: &reqwest::blocking::Client,
    assets: &[Asset],
    icons: &[IconEntry],
    token: Option<&str>,
) -> usize {
    let root = Path::new(APPLOAD_DEFAULT);
    let mut done = 0usize;
    for entry in icons {
        if !safe_app_name(&entry.app) {
            log::warn!("icon entry has an unsafe app name {:?} — skipped", entry.app);
            continue;
        }
        let dir = root.join(&entry.app);
        if !dir.is_dir() {
            log::info!("icon {}: no AppLoad dir — skipped", entry.app);
            continue;
        }
        let asset_name = format!("icon-{}.png", entry.app);
        let Some(asset) = assets.iter().find(|a| a.name == asset_name) else {
            log::warn!("manifest lists icon {:?} but the release has no {asset_name}", entry.app);
            continue;
        };
        let data = match download(client, &asset.url, token) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("icon {} download failed: {e}", entry.app);
                continue;
            }
        };
        let got = sha256_hex(&data);
        if !got.eq_ignore_ascii_case(entry.sha256.trim()) {
            log::warn!("icon {} sha256 mismatch (manifest {} != {got}) — skipped", entry.app, entry.sha256);
            continue;
        }
        let dest = dir.join("icon.png");
        let tmp = dir.join(".icon.png.new");
        let ok = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?; // flush before rename — power loss must not publish a torn icon
            std::fs::rename(&tmp, &dest)
        })();
        match ok {
            Ok(()) => {
                log::info!("installed icon {} ({} bytes)", entry.app, data.len());
                done += 1;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                log::warn!("icon {} install failed: {e}", entry.app);
            }
        }
    }
    done
}

/// Fetch the latest (or tagged) release: returns `(tag, assets)`.
fn fetch_release(
    client: &reqwest::blocking::Client,
    repo: &str,
    token: Option<&str>,
    channel: &str,
) -> Result<(String, Vec<Asset>)> {
    let url = if channel == "latest" {
        format!("https://api.github.com/repos/{repo}/releases/latest")
    } else {
        format!("https://api.github.com/repos/{repo}/releases/tags/{channel}")
    };
    let mut req = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, UA)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().context("GitHub releases API")?;
    let status = resp.status();
    // Fine-grained PATs return their expiry in this header; warn as it approaches so WiFi updates
    // never silently stop when the token lapses. Read it before the body is consumed.
    if let Some(exp) = resp.headers().get("github-authentication-token-expiration").and_then(|v| v.to_str().ok()) {
        warn_token_expiry(exp);
    }
    let body: serde_json::Value = resp.json()?;
    if !status.is_success() {
        bail!("releases API {}: {}", status, body);
    }
    let tag = body["tag_name"].as_str().context("release has no tag_name")?.to_string();
    let assets = parse_assets(&body);
    Ok((tag, assets))
}

/// Log a heads-up as the update token nears expiry, so WiFi updates never silently stop. GitHub
/// sends a fine-grained PAT's expiry as `GitHub-Authentication-Token-Expiration`, e.g.
/// `2027-07-03 00:00:00 UTC`. Warns within the last 30 days; info otherwise; silent if unparsable.
fn warn_token_expiry(header: &str) {
    let day = header.get(..10).unwrap_or(header);
    match token_expiry_days(header, chrono::Utc::now().date_naive()) {
        Some(d) if d <= 0 => log::warn!("update token has expired ({day}) — regenerate it to resume WiFi updates"),
        Some(d) if d <= 30 => {
            log::warn!("update token expires in {d} day(s) ({day}) — regenerate it soon to keep WiFi updates working")
        }
        Some(d) => log::info!("update token valid for {d} more day(s) (expires {day})"),
        None => {}
    }
}

/// Days from `today` until the token-expiry `header`, parsing its leading `YYYY-MM-DD`.
fn token_expiry_days(header: &str, today: chrono::NaiveDate) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(header.get(..10)?, "%Y-%m-%d").ok()?;
    Some((date - today).num_days())
}

fn parse_assets(release: &serde_json::Value) -> Vec<Asset> {
    release["assets"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a["name"].as_str()?.to_string(),
                        url: a["url"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Download a release asset's bytes (the API asset URL 302-redirects to storage; reqwest follows
/// it and strips the Authorization header on the cross-host hop, which is what we want).
fn download(client: &reqwest::blocking::Client, asset_url: &str, token: Option<&str>) -> Result<Vec<u8>> {
    let mut req = client
        .get(asset_url)
        .header(reqwest::header::USER_AGENT, UA)
        .header(reqwest::header::ACCEPT, "application/octet-stream");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send()?;
    let status = resp.status();
    if !status.is_success() {
        bail!("download {} → {}", asset_url, status);
    }
    Ok(resp.bytes()?.to_vec())
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Semver-ish comparison; falls back to "any difference = update" if either side is unparsable.
fn is_newer(remote: &str, local: &str) -> bool {
    fn parse(s: &str) -> Option<(u64, u64, u64)> {
        let s = s.trim().trim_start_matches('v');
        let mut it = s.split(['.', '-', '+']);
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().ok()?;
        let patch = it.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch))
    }
    match (parse(remote), parse(local)) {
        (Some(r), Some(l)) => r > l,
        _ => remote.trim_start_matches('v') != local.trim_start_matches('v'),
    }
}

fn write_executable(path: &Path, data: &[u8]) -> Result<()> {
    // fsync before the caller's rename: rename alone is only atomic for metadata — on power
    // loss, unflushed data blocks can publish a truncated ROOT-RUN binary. The .bak would
    // recover it, but never knowingly leave that window open.
    {
        use std::io::Write;
        let mut f = std::fs::File::create(path).with_context(|| format!("writing {}", path.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn smoke_test(bin: &Path) -> Result<()> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", bin.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        bail!("{} --version exited {}", bin.display(), out.status);
    }
}

/// One binary swapped into place, and whether it replaced an existing file (`.bak` kept) or is
/// brand-new to this release (nothing to restore — rollback must *remove* it).
struct Installed {
    name: String,
    had_bak: bool,
}

/// Hold an exclusive advisory lock on `<bin_dir>/.selfupdate.lock` for the whole run. Returns
/// `None` (without blocking) when another selfupdate already holds it. The lock file itself is
/// harmless residue — flock state lives on the open descriptor, not the file's existence.
fn acquire_lock(bin_dir: &Path) -> Result<Option<std::fs::File>> {
    std::fs::create_dir_all(bin_dir).ok();
    let path = bin_dir.join(".selfupdate.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok(if rc == 0 { Some(f) } else { None })
}

/// Remove `.{name}.new` temp files orphaned by a previous run that died between staging and the
/// rename. Runs under the lock, so a live run's temps are never swept.
fn sweep_orphans(bin_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(bin_dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n.starts_with('.') && n.ends_with(".new") {
            log::info!("removing orphaned temp {n}");
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Swap every staged binary into place (temp → rename), keeping a `.bak` of anything replaced.
/// A failure mid-loop rolls back the already-swapped subset before returning the error, so a
/// partial filesystem failure can't leave `bin/` half-old/half-new.
fn install_all(bin_dir: &Path, staged: &[(String, Vec<u8>)]) -> Result<Vec<Installed>> {
    std::fs::create_dir_all(bin_dir).ok();
    let mut done: Vec<Installed> = Vec::new();
    for (name, data) in staged {
        if let Err(e) = install_one(bin_dir, name, data, &mut done) {
            log::error!("installing {name} failed: {e}; rolling back the {} already swapped", done.len());
            rollback(bin_dir, &done);
            return Err(e);
        }
    }
    Ok(done)
}

fn install_one(bin_dir: &Path, name: &str, data: &[u8], done: &mut Vec<Installed>) -> Result<()> {
    let dest = bin_dir.join(name);
    let tmp = bin_dir.join(format!(".{name}.new"));
    write_executable(&tmp, data)?;
    let had_bak = dest.exists();
    if had_bak {
        std::fs::rename(&dest, bin_dir.join(format!("{name}.bak"))).with_context(|| format!("backing up {name}"))?;
    }
    // Recorded before the final rename: if that rename fails after the backup already moved the old
    // binary aside, rollback must know to restore it.
    done.push(Installed { name: name.to_string(), had_bak });
    std::fs::rename(&tmp, &dest).with_context(|| format!("installing {name}"))?;
    Ok(())
}

fn rollback(bin_dir: &Path, installed: &[Installed]) {
    for inst in installed {
        let dest = bin_dir.join(&inst.name);
        if inst.had_bak {
            let _ = std::fs::rename(bin_dir.join(format!("{}.bak", inst.name)), &dest);
        } else {
            // Brand-new binary this release — no predecessor to restore; remove the bad copy
            // rather than leave unvetted root-run code in bin/.
            let _ = std::fs::remove_file(&dest);
        }
    }
}

fn restart_timers() {
    let _ = std::process::Command::new("systemctl").arg("daemon-reload").status();
    for t in ["agenda.timer", "vaultsync.timer", "secondbrain-update.timer"] {
        // Only restart units this device actually has — the deployment may install just a subset
        // (e.g. only agenda.timer). `systemctl cat` exits non-zero for unknown units, so we skip
        // them instead of letting systemctl print "Unit not found" noise to the update log.
        let known = std::process::Command::new("systemctl")
            .args(["cat", t])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if known {
            let _ = std::process::Command::new("systemctl").args(["restart", t]).status();
        }
    }
}

fn parse_args() -> Args {
    let mut a = Args {
        config: PathBuf::from("/home/root/secondbrain/config/config.toml"),
        force: false,
        dry_run: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(p) = it.next() {
                    a.config = p.into();
                }
            }
            "--force" => a.force = true,
            "--dry-run" => a.dry_run = true,
            "-V" | "--version" => {
                println!(env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-h" | "--help" => {
                eprintln!("selfupdate [--config <config.toml>] [--force] [--dry-run]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // unparsable tag → update only if the string differs
        assert!(is_newer("nightly-2", "nightly-1"));
        assert!(!is_newer("same", "same"));
    }

    #[test]
    fn token_expiry_days_parses_header() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
        assert_eq!(token_expiry_days("2027-07-03 00:00:00 UTC", today), Some(365));
        assert_eq!(token_expiry_days("2026-07-13 12:00:00 +0000", today), Some(10));
        assert_eq!(token_expiry_days("2026-07-03 00:00:00 UTC", today), Some(0), "expires today");
        assert_eq!(token_expiry_days("2026-06-03", today), Some(-30), "already expired");
        assert_eq!(token_expiry_days("garbage", today), None);
        assert_eq!(token_expiry_days("", today), None);
    }

    #[test]
    fn app_names_cannot_escape_the_appload_tree() {
        assert!(safe_app_name("browser"));
        assert!(safe_app_name("one_thing-2"));
        assert!(!safe_app_name(""));
        assert!(!safe_app_name("../evil"));
        assert!(!safe_app_name("a/b"));
        assert!(!safe_app_name("."));
        assert!(!safe_app_name("name with spaces"));
    }

    #[test]
    fn sha256_matches_known_vector() {
        // echo -n "abc" | sha256sum
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn tmp_bin_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sbupd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_is_exclusive_and_nonblocking() {
        let dir = tmp_bin_dir("lock");
        let first = acquire_lock(&dir).unwrap();
        assert!(first.is_some(), "first acquire succeeds");
        // flock state lives on the open file description, so a second open in the same process
        // still contends — this models the timer + an app-launch check overlapping.
        let second = acquire_lock(&dir).unwrap();
        assert!(second.is_none(), "second acquire is refused without blocking");
        drop(first);
        let third = acquire_lock(&dir).unwrap();
        assert!(third.is_some(), "released lock can be re-acquired");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_restores_baks_and_removes_new_binaries() {
        let dir = tmp_bin_dir("rb");
        // "agenda" was replaced (has a .bak); "newtool" is brand-new this release (no .bak).
        std::fs::write(dir.join("agenda"), b"new-broken").unwrap();
        std::fs::write(dir.join("agenda.bak"), b"old-good").unwrap();
        std::fs::write(dir.join("newtool"), b"new-broken").unwrap();
        let installed = vec![
            Installed { name: "agenda".into(), had_bak: true },
            Installed { name: "newtool".into(), had_bak: false },
        ];
        rollback(&dir, &installed);
        assert_eq!(std::fs::read(dir.join("agenda")).unwrap(), b"old-good", "bak restored");
        assert!(!dir.join("agenda.bak").exists(), "bak consumed by the restore");
        assert!(!dir.join("newtool").exists(), "no-predecessor binary removed, not left in place");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_failure_mid_loop_rolls_back_earlier_swaps() {
        let dir = tmp_bin_dir("mid");
        std::fs::write(dir.join("a"), b"old-a").unwrap();
        // Second entry's name routes the temp file into a missing subdirectory → write fails
        // mid-loop, after "a" was already swapped.
        let staged = vec![
            ("a".to_string(), b"new-a".to_vec()),
            ("missing-dir/b".to_string(), b"new-b".to_vec()),
        ];
        let err = install_all(&dir, &staged);
        assert!(err.is_err(), "second install fails");
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"old-a", "first swap rolled back");
        assert!(!dir.join("a.bak").exists(), "rollback consumed the bak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_removes_only_orphaned_new_temps() {
        let dir = tmp_bin_dir("sweep");
        std::fs::write(dir.join(".agenda.new"), b"orphan").unwrap();
        std::fs::write(dir.join("agenda"), b"live").unwrap();
        std::fs::write(dir.join("agenda.bak"), b"bak").unwrap();
        sweep_orphans(&dir);
        assert!(!dir.join(".agenda.new").exists(), "orphaned temp removed");
        assert!(dir.join("agenda").exists() && dir.join("agenda.bak").exists(), "live files untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_manifest_and_assets() {
        // Old-format manifest (no icons) still parses — the field defaults empty.
        let manifest: Manifest = serde_json::from_str(
            r#"{"version":"0.2.0","binaries":[{"name":"agenda","sha256":"AA"},{"name":"vaultsync","sha256":"bb"}]}"#,
        )
        .unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.binaries.len(), 2);
        assert!(manifest.icons.is_empty(), "icons default empty on old manifests");

        // New-format manifest with icons.
        let with_icons: Manifest = serde_json::from_str(
            r#"{"version":"0.21.0","binaries":[{"name":"agenda","sha256":"AA"}],"icons":[{"app":"browser","sha256":"CC"}]}"#,
        )
        .unwrap();
        assert_eq!(with_icons.icons.len(), 1);
        assert_eq!(with_icons.icons[0].app, "browser");

        let release = serde_json::json!({
            "tag_name": "v0.2.0",
            "assets": [
                {"name": "agenda", "url": "https://api.github.com/.../assets/1"},
                {"name": "manifest.json", "url": "https://api.github.com/.../assets/2"}
            ]
        });
        let assets = parse_assets(&release);
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|a| a.name == "manifest.json"));
    }
}
