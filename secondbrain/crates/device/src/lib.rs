//! Suspend-screen installation — the one device-specific crate, behind [`SuspendScreen`].
//!
//! Three implementations:
//! * [`MockSuspendScreen`] — writes the PNG to a file (Mac preview / tests).
//! * [`ConfigSuspend`] — **preferred on device**: writes the PNG under `/home/root` and points
//!   `xochitl.conf`'s `SleepScreenPath` at it. Update-safe (image + config both in `/home/root`).
//! * [`SysfsSuspend`] — fallback: overwrite `/usr/share/remarkable/suspended.png` (needs the
//!   rootfs remounted rw; wiped by OS updates). Used only where `SleepScreenPath` is unsupported.

use anyhow::{Context, Result};
use image::RgbImage;
use std::path::PathBuf;

/// Install a rendered agenda as the lock/suspend screen.
pub trait SuspendScreen {
    fn install(&self, img: &RgbImage) -> Result<()>;
    /// Human-readable description of where/how it installs (for logs).
    fn describe(&self) -> String;
}

/// Mac/dev: just write the PNG so you can open and review it.
pub struct MockSuspendScreen {
    pub out: PathBuf,
}
impl SuspendScreen for MockSuspendScreen {
    fn install(&self, img: &RgbImage) -> Result<()> {
        if let Some(parent) = self.out.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        img.save(&self.out).with_context(|| format!("writing preview {}", self.out.display()))?;
        Ok(())
    }
    fn describe(&self) -> String {
        format!("preview → {}", self.out.display())
    }
}

/// Device (preferred): PNG in `/home/root`, referenced by `SleepScreenPath` in `xochitl.conf`.
pub struct ConfigSuspend {
    /// e.g. `/home/root/secondbrain/agenda.png`
    pub image_path: PathBuf,
    /// e.g. `/home/root/.config/remarkable/xochitl.conf`
    pub xochitl_conf: PathBuf,
}
impl SuspendScreen for ConfigSuspend {
    fn install(&self, img: &RgbImage) -> Result<()> {
        if let Some(parent) = self.image_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        img.save(&self.image_path)
            .with_context(|| format!("writing {}", self.image_path.display()))?;
        let existing = std::fs::read_to_string(&self.xochitl_conf).unwrap_or_default();
        let updated = set_ini_key(&existing, "General", "SleepScreenPath", &self.image_path.to_string_lossy());
        if let Some(parent) = self.xochitl_conf.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.xochitl_conf, updated)
            .with_context(|| format!("updating {}", self.xochitl_conf.display()))?;
        Ok(())
    }
    fn describe(&self) -> String {
        format!("SleepScreenPath {} in {}", self.image_path.display(), self.xochitl_conf.display())
    }
}

/// Device (fallback): overwrite the system suspend image. The caller must remount `/` rw first
/// (documented in `install.sh`); this is wiped by OS updates.
pub struct SysfsSuspend {
    /// e.g. `/usr/share/remarkable/suspended.png`
    pub target: PathBuf,
}
impl SuspendScreen for SysfsSuspend {
    fn install(&self, img: &RgbImage) -> Result<()> {
        img.save(&self.target)
            .with_context(|| format!("writing {} (is the rootfs remounted rw?)", self.target.display()))?;
        Ok(())
    }
    fn describe(&self) -> String {
        format!("system suspend image → {} (rootfs must be rw)", self.target.display())
    }
}

/// Set `[section] key=value` in INI text (xochitl.conf), preserving everything else.
/// Replaces the key if present in the section, inserts into the section if the key is absent,
/// or appends the section if absent. Exposed for testing.
pub fn set_ini_key(input: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let mut lines: Vec<String> = input.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut section_header_idx: Option<usize> = None;
    let mut key_idx: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            if in_section {
                break; // reached the next section without finding the key
            }
            if t == header {
                in_section = true;
                section_header_idx = Some(i);
            }
        } else if in_section {
            if let Some((k, _)) = t.split_once('=') {
                if k.trim() == key {
                    key_idx = Some(i);
                    break;
                }
            }
        }
    }

    let new_line = format!("{key}={value}");
    match (key_idx, section_header_idx) {
        (Some(i), _) => lines[i] = new_line,
        (None, Some(h)) => lines.insert(h + 1, new_line),
        (None, None) => {
            if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(new_line);
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_key_in_section() {
        let input = "[General]\nFoo=1\nSleepScreenPath=/old.png\nBar=2\n";
        let out = set_ini_key(input, "General", "SleepScreenPath", "/home/root/a.png");
        assert!(out.contains("SleepScreenPath=/home/root/a.png"));
        assert!(!out.contains("/old.png"));
        assert!(out.contains("Foo=1") && out.contains("Bar=2"));
    }

    #[test]
    fn inserts_key_into_existing_section() {
        let input = "[General]\nFoo=1\n[Other]\nBaz=3\n";
        let out = set_ini_key(input, "General", "SleepScreenPath", "/a.png");
        assert!(out.contains("[General]\nSleepScreenPath=/a.png"));
        // didn't leak into [Other]
        assert!(out.contains("[Other]\nBaz=3"));
    }

    #[test]
    fn appends_section_when_absent() {
        let out = set_ini_key("", "General", "SleepScreenPath", "/a.png");
        assert_eq!(out, "[General]\nSleepScreenPath=/a.png\n");
        let out2 = set_ini_key("[Other]\nX=1\n", "General", "SleepScreenPath", "/a.png");
        assert!(out2.contains("[Other]\nX=1"));
        assert!(out2.trim_end().ends_with("[General]\nSleepScreenPath=/a.png"));
    }

    #[test]
    fn mock_install_writes_png() {
        let dir = std::env::temp_dir().join(format!("sbdev-{}", std::process::id()));
        let out = dir.join("preview.png");
        let img = RgbImage::from_pixel(8, 8, image::Rgb([255, 255, 255]));
        MockSuspendScreen { out: out.clone() }.install(&img).unwrap();
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
