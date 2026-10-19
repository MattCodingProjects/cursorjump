//! OS-neutral pieces of the app loop, shared by the PC and Mac apps.

pub mod clipboard_bridge;
pub mod config_file;
pub mod settings_window;

use cj_core::config::Config;
use cj_net::Fingerprint;
use tracing::warn;

/// The paired computer's fingerprint; None if unpaired or not a fingerprint.
pub fn paired_fingerprint(cfg: &Config) -> Option<Fingerprint> {
    let hex = cfg.paired_peer.as_deref()?;
    let fp = Fingerprint::from_hex(hex);
    if fp.is_none() {
        warn!("paired_peer in the config is not a fingerprint; treating as unpaired");
    }
    fp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_fingerprint_parses_hex_only() {
        let fp = Fingerprint([0xab; 32]);
        let cfg = Config {
            paired_peer: Some(fp.to_hex()),
            ..Config::default()
        };
        assert_eq!(paired_fingerprint(&cfg), Some(fp));
        let bad = Config {
            paired_peer: Some("not hex".into()),
            ..Config::default()
        };
        assert_eq!(paired_fingerprint(&bad), None);
        assert_eq!(paired_fingerprint(&Config::default()), None);
    }
}
