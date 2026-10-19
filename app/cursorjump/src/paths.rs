//! Per-user locations of the settings file and the device identity.

use std::path::PathBuf;

use anyhow::Context;

/// Windows: `%APPDATA%\CursorJump`. macOS: `~/Library/Application Support/CursorJump`.
/// Holds `config.toml` and the identity files. Not created here.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .context("APPDATA is not set")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
        })
        .context("HOME is not set")?;
    Ok(base.join("CursorJump"))
}

/// The settings file inside `config_dir`.
pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn paths_are_per_user() {
        let appdata = PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA is set"));
        let p = config_path().unwrap();
        assert!(p.ends_with(r"CursorJump\config.toml"), "{}", p.display());
        assert!(
            p.starts_with(&appdata),
            "{} not under {}",
            p.display(),
            appdata.display()
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn paths_are_per_user() {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
        let p = config_path().unwrap();
        assert_eq!(
            p,
            home.join("Library/Application Support/CursorJump/config.toml")
        );
    }
}
