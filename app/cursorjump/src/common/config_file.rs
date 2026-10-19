//! The settings file as the app's loop sees it. A file that cannot be parsed
//! is never applied and never written over.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use cj_core::config::Config;
use tracing::{info, warn};

pub struct ConfigFile {
    path: PathBuf,
    stamp: Option<SystemTime>,
    /// Tells the user about a settings file that cannot be read.
    notify: fn(String),
}

impl ConfigFile {
    pub fn new(path: PathBuf, notify: fn(String)) -> Self {
        ConfigFile {
            stamp: modified(&path),
            path,
            notify,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The config to start with. An unreadable file is reported and the
    /// defaults are used in memory only.
    pub fn load_initial(&mut self) -> Config {
        match Config::try_load(&self.path) {
            Ok(cfg) => cfg.unwrap_or_default(),
            Err(e) => {
                warn!("{e:#}; using default settings until the file is fixed");
                (self.notify)(format!(
                    "CursorJump's settings file can't be read:\n\n{e:#}\n\n\
                     It is using default settings and won't change the file until \
                     it is fixed or deleted."
                ));
                Config::default()
            }
        }
    }

    /// The new config if the file changed and parses. None keeps the current one.
    pub fn poll(&mut self) -> Option<Config> {
        let stamp = modified(&self.path);
        if stamp == self.stamp {
            return None;
        }
        self.stamp = stamp;
        match Config::try_load(&self.path) {
            Ok(Some(cfg)) => Some(cfg),
            Ok(None) => {
                info!("settings file removed; keeping the current settings");
                None
            }
            Err(e) => {
                warn!("{e:#}; keeping the current settings");
                None
            }
        }
    }

    /// Applies `change` to the file as it is on disk (the settings window
    /// may have written it) and saves it. A missing file starts from
    /// `current`. Errs, leaving the file untouched, if it cannot be parsed.
    pub fn update(&mut self, current: &Config, change: impl FnOnce(&mut Config)) -> Result<Config> {
        let mut cfg = Config::try_load(&self.path)?.unwrap_or_else(|| current.clone());
        change(&mut cfg);
        let cfg = cfg.clamped();
        cfg.save(&self.path)
            .with_context(|| format!("saving {}", self.path.display()))?;
        self.stamp = modified(&self.path);
        Ok(cfg)
    }

    /// The config to apply after `change`: saved through `update`, or, if
    /// the file cannot be parsed, `current` with the change in memory only
    /// (the file is left alone and the user is told).
    pub fn change(&mut self, current: &Config, change: impl FnOnce(&mut Config) + Clone) -> Config {
        match self.update(current, change.clone()) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("not saving the settings file: {e:#}");
                (self.notify)(format!(
                    "CursorJump could not save this change, because its settings file \
                     can't be read:\n\n{e:#}\n\nThe change applies until CursorJump \
                     restarts. Fix or delete the file to save settings again."
                ));
                let mut cfg = current.clone();
                change(&mut cfg);
                cfg.clamped()
            }
        }
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ignore(_: String) {}

    fn file_with(dir: &tempfile::TempDir, text: &str, notify: fn(String)) -> ConfigFile {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        ConfigFile {
            path,
            stamp: None,
            notify,
        }
    }

    #[test]
    fn config_poll_applies_a_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "paused = true\n", ignore);
        assert!(f.poll().is_some_and(|c| c.paused));
        assert!(f.poll().is_none(), "unchanged file is not re-applied");
    }

    #[test]
    fn config_poll_keeps_current_on_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "paired_peer = \"ab\npaused = ", ignore);
        assert!(f.poll().is_none());
    }

    #[test]
    fn config_update_never_overwrites_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "paired_peer = \"ab\npaused = ";
        let mut f = file_with(&dir, broken, ignore);
        let current = Config {
            paired_peer: Some("ab".into()),
            ..Config::default()
        };
        assert!(f.update(&current, |c| c.paused = true).is_err());
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), broken);
    }

    #[test]
    fn config_update_merges_onto_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "mac_side = \"Left\"\n", ignore);
        let cfg = f.update(&Config::default(), |c| c.paused = true).unwrap();
        assert!(cfg.paused);
        assert_eq!(cfg.mac_side, cj_core::proto::Side::Left);
        assert_eq!(Config::try_load(&f.path).unwrap(), Some(cfg));
    }

    #[test]
    fn config_update_without_a_file_starts_from_current() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = ConfigFile::new(path, ignore);
        let current = Config {
            pointer_speed: 2.0,
            ..Config::default()
        };
        let cfg = f.update(&current, |c| c.paused = true).unwrap();
        assert_eq!(cfg.pointer_speed, 2.0);
        assert!(cfg.paused);
    }

    static NOTIFIED: AtomicUsize = AtomicUsize::new(0);

    fn count(_: String) {
        NOTIFIED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn config_change_on_a_corrupt_file_applies_in_memory_and_tells_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let broken = "paired_peer = \"ab\npaused = ";
        let mut f = file_with(&dir, broken, count);
        let before = NOTIFIED.load(Ordering::SeqCst);
        let cfg = f.change(&Config::default(), |c| c.paused = true);
        assert!(cfg.paused);
        assert_eq!(NOTIFIED.load(Ordering::SeqCst), before + 1);
        assert_eq!(std::fs::read_to_string(&f.path).unwrap(), broken);
    }

    #[test]
    fn config_change_saves_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = file_with(&dir, "", ignore);
        let cfg = f.change(&Config::default(), |c| c.paired_peer = Some("ab".into()));
        assert_eq!(Config::try_load(&f.path).unwrap(), Some(cfg));
    }
}
