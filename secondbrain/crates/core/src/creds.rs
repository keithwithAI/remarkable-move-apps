//! Configuration + credential loading from `config/config.toml`.
//!
//! Secrets (service-account JSON, Todoist token, Git key) live as files referenced by the
//! config, `chmod 600`, never inlined into source. The Todoist token may be given inline or,
//! preferably, via `token_file`.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CredsError {
    #[error("reading {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("parsing {0}: {1}")]
    Parse(PathBuf, #[source] toml::de::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub google: GoogleCfg,
    #[serde(default)]
    pub todoist: TodoistCfg,
    #[serde(default)]
    pub vault: VaultCfg,
    /// Optional second Git-synced source: the Kindle-highlights books repo (read-only on device —
    /// vaultsync pulls it after the vault; the reader scans its clone as the Books collection).
    #[serde(default)]
    pub books: VaultCfg,
    #[serde(default)]
    pub update: UpdateCfg,
    #[serde(default)]
    pub planner: PlannerCfg,
    /// Directory of the config file, filled in by [`Config::load`] for resolving relative paths.
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoogleCfg {
    /// Path to the Google service-account JSON key (calendar.readonly scope).
    pub service_account: PathBuf,
    /// Calendar IDs to merge (e.g. "primary", "you@work.com"). Empty = ["primary"].
    #[serde(default)]
    pub calendars: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TodoistCfg {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Todoist filter for today's view (API v1 `filter` query).
    #[serde(default = "default_filter")]
    pub filter: String,
}

impl Default for TodoistCfg {
    fn default() -> Self {
        TodoistCfg { token: None, token_file: None, filter: default_filter() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VaultCfg {
    #[serde(default)]
    pub repo_url: Option<String>,
    #[serde(default)]
    pub local: Option<PathBuf>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Path to an SSH private key for Git, if using SSH transport.
    #[serde(default)]
    pub ssh_key: Option<PathBuf>,
}

/// Over-the-WiFi self-update via GitHub Releases (the `selfupdate` binary).
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateCfg {
    /// GitHub repo hosting the releases, e.g. "keithwithAI/remarkable-move-apps".
    #[serde(default)]
    pub repo: Option<String>,
    /// File holding a token with read access to the (private) repo's releases.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// Inline token (discouraged; prefer token_file).
    #[serde(default)]
    pub token: Option<String>,
    /// "latest" or a specific release tag.
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Where the installed binaries live (default /home/root/secondbrain/bin).
    #[serde(default)]
    pub bin_dir: Option<PathBuf>,
    /// File tracking the installed version (default /home/root/secondbrain/VERSION).
    #[serde(default)]
    pub version_file: Option<PathBuf>,
    /// When true (default), the planner checks for updates in the background on launch and the
    /// sleep-screen service checks before each render. Set false to rely only on the timer.
    #[serde(default = "default_true")]
    pub auto: bool,
}

impl Default for UpdateCfg {
    fn default() -> Self {
        UpdateCfg {
            repo: None,
            token_file: None,
            token: None,
            channel: default_channel(),
            bin_dir: None,
            version_file: None,
            auto: true,
        }
    }
}

/// The interactive planner app (day paging window + its Todoist fetch filter).
#[derive(Debug, Clone, Deserialize)]
pub struct PlannerCfg {
    /// Days after today included in the fetch/paging window.
    #[serde(default = "default_days_ahead")]
    pub days_ahead: u32,
    /// Days before today included (yesterday helps when reviewing after midnight).
    #[serde(default = "default_days_back")]
    pub days_back: u32,
    /// Todoist filter for the planner window. Defaults to
    /// `"overdue | due before: +{days_ahead+1} days"`.
    #[serde(default)]
    pub todoist_filter: Option<String>,
}

impl Default for PlannerCfg {
    fn default() -> Self {
        PlannerCfg { days_ahead: default_days_ahead(), days_back: default_days_back(), todoist_filter: None }
    }
}

impl PlannerCfg {
    pub fn filter(&self) -> String {
        self.todoist_filter
            .clone()
            .unwrap_or_else(|| format!("overdue | due before: +{} days", self.days_ahead + 1))
    }
}

fn default_days_ahead() -> u32 {
    7
}
fn default_days_back() -> u32 {
    1
}

fn default_channel() -> String {
    "latest".to_string()
}

fn default_true() -> bool {
    true
}

fn default_filter() -> String {
    "today | overdue".to_string()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, CredsError> {
        let raw = std::fs::read_to_string(path).map_err(|e| CredsError::Io(path.into(), e))?;
        let mut cfg: Config = toml::from_str(&raw).map_err(|e| CredsError::Parse(path.into(), e))?;
        cfg.base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(cfg)
    }

    /// Resolve a possibly-relative path against the config file's directory.
    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() { p.to_path_buf() } else { self.base_dir.join(p) }
    }

    /// Where caches (agenda.json, planner.json, actions.json) live.
    pub fn cache_dir(&self) -> PathBuf {
        self.base_dir.join("cache")
    }

    pub fn service_account_path(&self) -> PathBuf {
        self.resolve(&self.google.service_account)
    }

    /// Calendars to query (defaults to `["primary"]` when none configured).
    pub fn calendars(&self) -> Vec<String> {
        if self.google.calendars.is_empty() {
            vec!["primary".to_string()]
        } else {
            self.google.calendars.clone()
        }
    }

    /// Resolve the Todoist token from the inline value or `token_file`.
    pub fn todoist_token(&self) -> Result<Option<String>, CredsError> {
        if let Some(t) = self.todoist.token.as_deref() {
            if !t.is_empty() {
                return Ok(Some(t.to_string()));
            }
        }
        if let Some(f) = &self.todoist.token_file {
            let p = self.resolve(f);
            let t = std::fs::read_to_string(&p).map_err(|e| CredsError::Io(p.clone(), e))?;
            return Ok(Some(t.trim().to_string()));
        }
        Ok(None)
    }

    /// Resolve the GitHub token for self-update, from the inline value or `token_file`.
    pub fn update_token(&self) -> Result<Option<String>, CredsError> {
        if let Some(t) = self.update.token.as_deref() {
            if !t.is_empty() {
                return Ok(Some(t.to_string()));
            }
        }
        if let Some(f) = &self.update.token_file {
            let p = self.resolve(f);
            let t = std::fs::read_to_string(&p).map_err(|e| CredsError::Io(p.clone(), e))?;
            return Ok(Some(t.trim().to_string()));
        }
        Ok(None)
    }
}

/// Where `selfupdate` records the installed version (mirrors its own default).
const VERSION_FILE_DEFAULT: &str = "/home/root/secondbrain/VERSION";

/// A no-update check is a single API round-trip (a few seconds); a child still running past this
/// is downloading binaries — worth a different status line.
const IN_PROGRESS_AFTER: std::time::Duration = std::time::Duration::from_secs(6);

/// Lifecycle of a background self-update, for a status line in the launching app's UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Still running, and young enough to be the version check itself.
    Checking,
    /// Still running past [`IN_PROGRESS_AFTER`] — binaries are downloading/installing.
    InProgress,
    /// Finished; already on the latest version (the installed one).
    UpToDate(String),
    /// Finished; binaries were swapped to this version (applies on the app's next launch).
    Updated(String),
    /// The check exited non-zero (offline, auth, …) — the next trigger retries.
    Failed,
}

/// Handle to a spawned background self-update. [`poll`](UpdateCheck::poll) is non-blocking, so the
/// app's run loop can surface the outcome without ever waiting on the child. The outcome is derived
/// from the exit status + whether the `VERSION` file changed (selfupdate writes it only after a
/// successful, smoke-tested swap).
pub struct UpdateCheck {
    child: std::process::Child,
    version_file: PathBuf,
    before: Option<String>,
    started: std::time::Instant,
}

impl UpdateCheck {
    pub fn poll(&mut self) -> UpdateStatus {
        match self.child.try_wait() {
            Ok(None) if self.started.elapsed() > IN_PROGRESS_AFTER => UpdateStatus::InProgress,
            Ok(None) => UpdateStatus::Checking,
            Ok(Some(status)) => {
                let after = read_version(&self.version_file);
                match &after {
                    Some(a) if self.before.as_deref() != Some(a.as_str()) => UpdateStatus::Updated(a.clone()),
                    _ if status.success() => UpdateStatus::UpToDate(
                        after.or_else(|| self.before.clone()).unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
                    ),
                    _ => UpdateStatus::Failed,
                }
            }
            Err(_) => UpdateStatus::Failed,
        }
    }
}

fn read_version(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Fire-and-forget WiFi self-update: spawn the sibling `selfupdate` binary detached so it checks
/// GitHub Releases in the background while an app runs. New binaries land for the *next* launch, so
/// this never blocks the UI. Returns a pollable [`UpdateCheck`] so the app can surface the outcome;
/// `None` (a no-op) when `[update] auto=false`, no repo is configured, or the `selfupdate` binary
/// isn't beside the current exe (e.g. on the Mac). Shared by the planner and the reader so opening
/// *either* app pulls the latest release; output (incl. token-expiry warnings) is captured to
/// `selfupdate.log` beside the install tree.
pub fn spawn_self_update(cfg: &Config, config_path: &Path) -> Option<UpdateCheck> {
    if !cfg.update.auto || cfg.update.repo.is_none() {
        return None;
    }
    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))?;
    let selfupdate = dir.join("selfupdate");
    if !selfupdate.exists() {
        log::warn!("self-update: {} not found; skipping", selfupdate.display());
        return None;
    }
    // Capture selfupdate's output (including token-expiry warnings) to a log rather than discard it.
    // Append — both apps and repeated launches share this file, and truncating (File::create) would
    // clobber a concurrent run's output — with a crude size cap so it can't grow unbounded.
    let root = dir.parent().unwrap_or(dir.as_path());
    let log_path = root.join("selfupdate.log");
    if std::fs::metadata(&log_path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&log_path);
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();
    let stdio = |f: &Option<std::fs::File>| {
        f.as_ref()
            .and_then(|h| h.try_clone().ok())
            .map(std::process::Stdio::from)
            .unwrap_or_else(std::process::Stdio::null)
    };
    let mut cmd = std::process::Command::new(&selfupdate);
    cmd.arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(stdio(&log))
        .stderr(stdio(&log));
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // Detach into a new session so the check completes even if the app is closed right after.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        Ok(child) => {
            log::info!("self-update: background check started");
            let version_file =
                cfg.update.version_file.clone().unwrap_or_else(|| PathBuf::from(VERSION_FILE_DEFAULT));
            let before = read_version(&version_file);
            Some(UpdateCheck { child, version_file, before, started: std::time::Instant::now() })
        }
        Err(e) => {
            log::warn!("self-update: spawn failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_and_defaults() {
        let dir = std::env::temp_dir().join(format!("sbcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            r#"
                [google]
                service_account = "service-account.json"
            "#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.calendars(), vec!["primary".to_string()]);
        assert_eq!(cfg.todoist.filter, "today | overdue");
        assert!(cfg.todoist_token().unwrap().is_none());
        // relative service-account path resolves against the config dir
        assert_eq!(cfg.service_account_path(), dir.join("service-account.json"));
        // planner defaults
        assert_eq!(cfg.planner.days_ahead, 7);
        assert_eq!(cfg.planner.days_back, 1);
        assert_eq!(cfg.planner.filter(), "overdue | due before: +8 days");
        assert_eq!(cfg.cache_dir(), dir.join("cache"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn finished(mut check: UpdateCheck) -> UpdateStatus {
        for _ in 0..500 {
            match check.poll() {
                UpdateStatus::Checking | UpdateStatus::InProgress => std::thread::sleep(std::time::Duration::from_millis(10)),
                done => return done,
            }
        }
        panic!("child never exited");
    }

    #[test]
    fn update_check_reports_outcomes_from_exit_and_version_file() {
        let dir = std::env::temp_dir().join(format!("sbupdchk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vf = dir.join("VERSION");
        std::fs::write(&vf, "1.0.0\n").unwrap();
        let spawn = |bin: &str| std::process::Command::new(bin).spawn().unwrap();
        let mk = |child| UpdateCheck { child, version_file: vf.clone(), before: read_version(&vf), started: std::time::Instant::now() };

        // Exit 0, VERSION unchanged → up to date at the installed version.
        assert_eq!(finished(mk(spawn("true"))), UpdateStatus::UpToDate("1.0.0".into()));

        // VERSION advanced while the child ran → updated (regardless of exit code).
        let check = mk(spawn("true"));
        std::fs::write(&vf, "1.1.0\n").unwrap();
        assert_eq!(finished(check), UpdateStatus::Updated("1.1.0".into()));

        // Non-zero exit, VERSION unchanged → failed.
        assert_eq!(finished(mk(spawn("false"))), UpdateStatus::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planner_section_overrides() {
        let dir = std::env::temp_dir().join(format!("sbcfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            r#"
                [google]
                service_account = "sa.json"
                [planner]
                days_ahead = 3
                todoist_filter = "overdue | 4 days"
            "#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.planner.days_ahead, 3);
        assert_eq!(cfg.planner.filter(), "overdue | 4 days");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod stale_section_tests {
    use super::*;

    /// A device config written before Focustask was retired still carries a `[focustask]`
    /// section. Serde ignores unknown fields by default, but that is exactly the kind of
    /// assumption worth pinning: if anyone ever adds `deny_unknown_fields`, every deployed
    /// device would fail to load its config and the app would fall back to demo mode.
    #[test]
    fn a_leftover_focustask_section_does_not_break_config_loading() {
        let toml = r#"
            [google]
            service_account = "service-account.json"
            calendars = ["calendar@example.com"]

            [todoist]
            token_file = "todoist.token"

            [focustask]
            base_url = "http://10.0.0.2:8080"
        "#;
        let cfg: Config = toml::from_str(toml).expect("stale [focustask] section must be ignored");
        assert_eq!(cfg.google.calendars, vec!["calendar@example.com".to_string()]);
    }
}
