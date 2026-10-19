//! Start at login through a per-user LaunchAgent:
//! `~/Library/LaunchAgents/com.cursorjump.agent.plist`.
//!
//! Only an executable inside an app bundle is registered, so a development
//! build under `target/` never ends up starting at login.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The LaunchAgent's label, also its plist's file name.
pub const AGENT_LABEL: &str = "com.cursorjump.agent";

/// `ESRCH` ("No such process"): launchctl's answer for an unloaded service.
const LAUNCHCTL_NO_SUCH_PROCESS: i32 = 3;
/// "Could not find specified service", from newer launchctl versions.
const LAUNCHCTL_NO_SUCH_SERVICE: i32 = 113;

/// Registers (or removes) the LaunchAgent that starts this app at login.
///
/// Enabling also starts the app now (the agent has `RunAtLoad`); disabling
/// stops a copy the agent started.
pub fn set_autostart(enabled: bool) -> Result<()> {
    let plist = agent_plist_path()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let service = format!("{domain}/{AGENT_LABEL}");
    if enabled {
        let exe = std::env::current_exe().context("locating the cursorjump executable")?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        require_app_bundle(&exe)?;
        write_atomically(&plist, &launch_agent_plist(&exe))?;
        bootout(&service)?;
        let plist_arg = plist
            .to_str()
            .context("the LaunchAgents path is not UTF-8")?;
        launchctl(&["bootstrap", &domain, plist_arg])?;
        tracing::info!(exe = %exe.display(), plist = %plist.display(), "autostart enabled");
    } else {
        bootout(&service)?;
        match std::fs::remove_file(&plist) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", plist.display())),
        }
        tracing::info!("autostart disabled");
    }
    Ok(())
}

/// `~/Library/LaunchAgents/com.cursorjump.agent.plist`.
fn agent_plist_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{AGENT_LABEL}.plist")))
}

/// The LaunchAgent property list that runs `executable` at login.
///
/// `KeepAlive` restarts the app after a crash but not after Quit, and
/// `ProcessType Interactive` keeps App Nap from throttling it.
fn launch_agent_plist(executable: &Path) -> String {
    let exe = xml_escape(&executable.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{AGENT_LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>LimitLoadToSessionType</key>
	<string>Aqua</string>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Errs unless `exe` is `…/<Name>.app/Contents/MacOS/cursorjump`.
fn require_app_bundle(exe: &Path) -> Result<()> {
    let names: Vec<&str> = exe
        .iter()
        .rev()
        .take(4)
        .map(|part| part.to_str().unwrap_or(""))
        .collect();
    let bundled = matches!(
        names.as_slice(),
        ["cursorjump", "MacOS", "Contents", app] if app.len() > ".app".len() && app.ends_with(".app")
    );
    if !bundled {
        bail!(
            "start at login needs the installed app (…/CursorJump.app/Contents/MacOS/cursorjump), \
             but this is {}. Build and install it with scripts/mac/install.sh.",
            exe.display()
        );
    }
    Ok(())
}

/// Writes `contents` to `path` (mode 0644) through a temporary file and a
/// rename, so launchd never sees a half-written plist.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("the plist path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{AGENT_LABEL}.plist.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        // The mode above is masked by the umask; set it exactly.
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// Unloads `service`; an agent that is not loaded is not an error.
fn bootout(service: &str) -> Result<()> {
    let out = Command::new("launchctl")
        .args(["bootout", service])
        .output()
        .context("running launchctl")?;
    let code = out.status.code().unwrap_or(-1);
    if out.status.success() || not_loaded(code, &String::from_utf8_lossy(&out.stderr)) {
        return Ok(());
    }
    bail!(
        "launchctl bootout {service} failed ({code}): {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )
}

/// True if a failed `launchctl bootout` only means the agent was not loaded.
fn not_loaded(code: i32, stderr: &str) -> bool {
    code == LAUNCHCTL_NO_SUCH_PROCESS
        || code == LAUNCHCTL_NO_SUCH_SERVICE
        || stderr.contains("No such process")
        || stderr.contains("Could not find specified service")
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = Command::new("launchctl")
        .args(args)
        .output()
        .context("running launchctl")?;
    if !out.status.success() {
        bail!(
            "launchctl {} failed ({}): {}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLED: &str = "/Applications/CursorJump.app/Contents/MacOS/cursorjump";

    /// Writes `contents` to a temporary file and returns plutil's view of `key`.
    fn plutil_extract(contents: &str, key: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(&path, contents).unwrap();
        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        assert!(lint.status.success(), "{lint:?}\n{contents}");
        let out = Command::new("plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout)
            .unwrap()
            .trim_end()
            .to_string()
    }

    #[test]
    fn launch_agent_plist_contents() {
        let plist = launch_agent_plist(Path::new(BUNDLED));
        assert!(
            plist.contains(&format!("<string>{BUNDLED}</string>")),
            "{plist}"
        );
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"), "{plist}");
        assert!(
            plist.contains(
                "<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>"
            ),
            "{plist}"
        );
        assert_eq!(plutil_extract(&plist, "Label"), AGENT_LABEL);
        assert_eq!(plutil_extract(&plist, "ProgramArguments.0"), BUNDLED);
        assert_eq!(plutil_extract(&plist, "RunAtLoad"), "true");
        assert_eq!(plutil_extract(&plist, "KeepAlive.SuccessfulExit"), "false");
        assert_eq!(plutil_extract(&plist, "LimitLoadToSessionType"), "Aqua");
        assert_eq!(plutil_extract(&plist, "ProcessType"), "Interactive");
    }

    #[test]
    fn launch_agent_plist_escapes_the_path() {
        let exe = "/Users/a&b/<Apps>/C\"J'.app/Contents/MacOS/cursorjump";
        let plist = launch_agent_plist(Path::new(exe));
        assert!(
            plist.contains("/Users/a&amp;b/&lt;Apps&gt;/C&quot;J&apos;.app/"),
            "{plist}"
        );
        assert_eq!(plutil_extract(&plist, "ProgramArguments.0"), exe);
    }

    #[test]
    fn bundled_executable_is_accepted() {
        assert!(require_app_bundle(Path::new(BUNDLED)).is_ok());
        assert!(
            require_app_bundle(Path::new(
                "/Users/me/cursorjump/target/CursorJump.app/Contents/MacOS/cursorjump"
            ))
            .is_ok()
        );
    }

    #[test]
    fn executable_outside_a_bundle_is_refused() {
        for exe in [
            "/Users/me/cursorjump/target/debug/cursorjump",
            "/Users/me/cursorjump/target/release/cursorjump",
            "/Applications/CursorJump/Contents/MacOS/cursorjump",
            "/Applications/CursorJump.app/Contents/Resources/cursorjump",
            "/Applications/CursorJump.app/MacOS/cursorjump",
            "/Applications/CursorJump.app/Contents/MacOS/other",
            "cursorjump",
        ] {
            let err = require_app_bundle(&PathBuf::from(exe)).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains(".app"), "{exe}: {text}");
            assert!(text.contains(exe), "{exe}: {text}");
        }
    }

    #[test]
    fn bootout_of_an_unloaded_agent_is_ignored() {
        assert!(not_loaded(3, ""));
        assert!(not_loaded(113, ""));
        assert!(not_loaded(1, "Boot-out failed: 3: No such process"));
        assert!(!not_loaded(5, "Boot-out failed: 5: Input/output error"));
        assert!(!not_loaded(1, "Operation not permitted"));
    }
}
