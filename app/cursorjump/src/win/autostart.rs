//! Start at login through the per-user Run key:
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `CursorJump`.

use std::os::windows::ffi::OsStrExt;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW,
};
use windows::core::{PCWSTR, w};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("CursorJump");

/// An open registry key, closed on drop.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by RegCreateKeyExW and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn open_run_key() -> Result<Key> {
    let mut key = HKEY::default();
    // SAFETY: `key` outlives the call; the strings are static and NUL-terminated.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    status.ok().context("opening the Run key")?;
    Ok(Key(key))
}

/// Registers (or removes) this executable, quoted, to start at login.
pub fn set_autostart(enabled: bool) -> Result<()> {
    let key = open_run_key()?;
    if enabled {
        let exe = std::env::current_exe().context("locating cursorjump.exe")?;
        let mut command: Vec<u16> = Vec::new();
        command.push(u16::from(b'"'));
        command.extend(exe.as_os_str().encode_wide());
        command.push(u16::from(b'"'));
        command.push(0);
        let bytes: Vec<u8> = command.iter().flat_map(|c| c.to_le_bytes()).collect();
        // SAFETY: the key is open for KEY_SET_VALUE; `bytes` is a NUL-terminated UTF-16 string.
        unsafe { RegSetValueExW(key.0, VALUE_NAME, None, REG_SZ, Some(&bytes)) }
            .ok()
            .context("writing the Run value")?;
        tracing::info!(exe = %exe.display(), "autostart enabled");
    } else {
        // SAFETY: the key is open for KEY_SET_VALUE.
        let status = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            bail!(
                "removing the Run value: {:?}",
                status.to_hresult().message()
            );
        }
        tracing::info!("autostart disabled");
    }
    Ok(())
}

/// True if the Run value exists (whatever executable it names).
pub fn autostart_enabled() -> bool {
    // SAFETY: only queries existence; no output buffers are passed.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    };
    status == ERROR_SUCCESS
}
