//! User settings file.
use crate::proto::Side;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub mac_side: Side,
    pub edge_offset: f32,
    pub pointer_speed: f32,
    pub scroll_speed: f32,
    pub reverse_scroll: bool,
    pub swap_ctrl_cmd: bool,
    pub paused: bool,
    pub autostart: bool,
    /// Mac only.
    pub reduce_wifi_hiccups: bool,
    pub clipboard_max_bytes: u64,
    /// Hex fingerprint of the paired peer.
    pub paired_peer: Option<String>,
    /// Hidden last resort, e.g. "192.168.1.20:47800".
    pub manual_peer_addr: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mac_side: Side::Right,
            edge_offset: 0.0,
            pointer_speed: 1.0,
            scroll_speed: 1.0,
            reverse_scroll: false,
            swap_ctrl_cmd: true,
            paused: false,
            autostart: true,
            reduce_wifi_hiccups: false,
            clipboard_max_bytes: 33_554_432,
            paired_peer: None,
            manual_peer_addr: None,
        }
    }
}

fn clamp_or(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        default
    }
}

impl Config {
    /// Never panics: a missing or corrupt file yields the defaults.
    pub fn load(path: &Path) -> Config {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<Config>(&s).ok())
            .unwrap_or_default()
            .clamped()
    }

    /// Atomic write: temp file in the same directory, then rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    pub fn clamped(mut self) -> Config {
        self.edge_offset = clamp_or(self.edge_offset, -0.5, 0.5, 0.0);
        self.pointer_speed = clamp_or(self.pointer_speed, 0.25, 4.0, 1.0);
        self.scroll_speed = clamp_or(self.scroll_speed, 0.25, 4.0, 1.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_on_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert_eq!(Config::load(&p), Config::default());
        std::fs::write(&p, "not toml {{").unwrap();
        assert_eq!(Config::load(&p), Config::default());
    }

    #[test]
    fn config_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("config.toml");
        let c = Config {
            mac_side: Side::Left,
            edge_offset: 0.25,
            pointer_speed: 2.0,
            reverse_scroll: true,
            paired_peer: Some("ab12cd".into()),
            manual_peer_addr: Some("192.168.1.20:47800".into()),
            ..Config::default()
        };
        c.save(&p).unwrap();
        c.save(&p).unwrap(); // overwrite works
        assert_eq!(Config::load(&p), c);
    }

    #[test]
    fn config_partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "paused = true\n").unwrap();
        let c = Config::load(&p);
        assert!(c.paused);
        assert_eq!(c.pointer_speed, 1.0);
    }

    #[test]
    fn config_clamps() {
        let c = Config {
            pointer_speed: 10.0,
            scroll_speed: 0.0,
            edge_offset: -3.0,
            ..Config::default()
        }
        .clamped();
        assert_eq!(c.pointer_speed, 4.0);
        assert_eq!(c.scroll_speed, 0.25);
        assert_eq!(c.edge_offset, -0.5);
    }
}
