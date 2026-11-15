//! Which PC audio devices exist, the microphone privacy switch, and a watch
//! on device changes.
//!
//! Nothing here opens a device: existence comes from the endpoint
//! enumerator and the privacy switch from the registry, so Windows' "mic in
//! use" indicator never shows for a status check.
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use cj_core::audio::{AudioStatus, DeviceState};
use tracing::warn;
use windows::Win32::Foundation::{ERROR_SUCCESS, PROPERTYKEY};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{PCWSTR, implement, w};

use super::Com;

/// The microphone's consent store: the "Microphone access" switch.
const MIC_CONSENT: PCWSTR = w!(
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone"
);
/// "Let desktop apps access your microphone".
const MIC_CONSENT_DESKTOP: PCWSTR = w!(
    r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged"
);

/// Whether a consent store `Value` denies access.
fn consent_denied(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("Deny"))
}

/// The string `Value` of the HKCU key `subkey`; None if it is missing.
fn consent_value(subkey: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `buf` and `bytes` outlive the call and `bytes` is buf's size;
    // the strings are static and NUL-terminated.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey,
            w!("Value"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        // Missing, or longer than any consent value: not "Deny" either way.
        return None;
    }
    let chars = (bytes as usize / 2).min(buf.len());
    let s = &buf[..chars];
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    Some(String::from_utf16_lossy(&s[..end]))
}

/// True if Windows' privacy settings keep desktop apps from the microphone:
/// the microphone switch, or the desktop apps switch under it, is off.
pub fn mic_blocked_by_privacy() -> bool {
    consent_denied(consent_value(MIC_CONSENT).as_deref())
        || consent_denied(consent_value(MIC_CONSENT_DESKTOP).as_deref())
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: plain COM activation; COM is initialized on this thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// The state of the PC's default output and microphone.
pub fn pc_status() -> AudioStatus {
    let _com = Com::init();
    let enumerator = enumerator()
        .map_err(|e| warn!("audio device enumerator: {e}"))
        .ok();
    let has = |flow: EDataFlow| {
        // SAFETY: plain COM call on a live enumerator; it only looks the endpoint up.
        let found = enumerator
            .as_ref()
            .is_some_and(|en| unsafe { en.GetDefaultAudioEndpoint(flow, eConsole) }.is_ok());
        if found {
            DeviceState::Ok
        } else {
            DeviceState::Missing
        }
    };
    AudioStatus {
        output: has(eRender),
        mic: if mic_blocked_by_privacy() {
            DeviceState::Blocked
        } else {
            has(eCapture)
        },
    }
}

/// Calls `on_change` whenever the default devices or the set of devices
/// change. Runs its own thread, `cj-audio-watch`; drop unregisters it.
pub struct DeviceWatch {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceWatch {
    pub fn start(on_change: impl Fn() + Send + Sync + 'static) -> Result<DeviceWatch> {
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(on_change);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(1);
        let thread = std::thread::Builder::new()
            .name("cj-audio-watch".into())
            .spawn(move || watch(on_change, &ready_tx, &stop_rx))
            .context("spawn cj-audio-watch")?;
        let ready = ready_rx
            .recv()
            .map_err(|_| anyhow!("cj-audio-watch exited at start"))
            .and_then(|r| r);
        let watch = DeviceWatch {
            stop: Some(stop_tx),
            thread: Some(thread),
        };
        ready.map(|()| watch)
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// cj-audio-watch: registers the client, waits for the stop, unregisters.
fn watch(
    on_change: Arc<dyn Fn() + Send + Sync>,
    ready: &mpsc::SyncSender<Result<()>>,
    stop: &mpsc::Receiver<()>,
) {
    let _com = Com::init();
    let registered = enumerator().and_then(|en| {
        let client: IMMNotificationClient = Notify { on_change }.into();
        // SAFETY: plain COM call; the client stays alive until it is unregistered below.
        unsafe { en.RegisterEndpointNotificationCallback(&client) }.map(|()| (en, client))
    });
    let (en, client) = match registered {
        Ok(r) => r,
        Err(e) => {
            let _ = ready.send(Err(anyhow!(e).context("watching audio devices")));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    // Until the DeviceWatch drops its sender.
    let _ = stop.recv();
    // SAFETY: the same enumerator and client that registered.
    if let Err(e) = unsafe { en.UnregisterEndpointNotificationCallback(&client) } {
        warn!("unwatching audio devices: {e}");
    }
}

/// The notification client. Windows calls it on its own threads; each
/// callback only calls `on_change`, which must return at once.
#[implement(IMMNotificationClient)]
struct Notify {
    on_change: Arc<dyn Fn() + Send + Sync>,
}

impl IMMNotificationClient_Impl for Notify_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        (self.on_change)();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if role == eConsole {
            (self.on_change)();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_values_are_read_as_blocked_only_for_deny() {
        assert!(consent_denied(Some("Deny")));
        assert!(!consent_denied(Some("Allow")));
        assert!(!consent_denied(Some("")));
        assert!(!consent_denied(Some("Prompt")));
        assert!(!consent_denied(None));
    }

    #[test]
    fn pc_status_does_not_panic_without_devices() {
        // CI may have no audio endpoints: then both read Missing (or the mic
        // Blocked), never a panic.
        let s = pc_status();
        assert!(matches!(s.output, DeviceState::Ok | DeviceState::Missing));
        let _ = s.mic;
    }

    #[test]
    fn device_watch_starts_and_stops_cleanly() {
        // CI may have no audio service, so failing to start is fine; a
        // started watch must unregister and end its thread on drop.
        if let Ok(w) = DeviceWatch::start(|| {}) {
            drop(w);
        }
    }
}
