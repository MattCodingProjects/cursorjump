//! The PC's audio devices, through WASAPI shared mode on the eConsole
//! defaults only: the Mac's sound played on the default output
//! ([`render`]), and which devices exist and whether Windows lets desktop
//! apps use the microphone ([`devices`]).
pub mod capture;
pub mod devices;
pub mod render;

use std::marker::PhantomData;

use anyhow::{Context, Result};
use cj_core::audio::{AudioStream, RATE};
use tracing::warn;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, EDataFlow, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_FRONT_CENTER, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
};
use windows::core::w;

/// COM on the current thread for the guard's life. Joins the MTA, or uses
/// the apartment the thread already has; uninitializes only what it added.
struct Com {
    added: bool,
    /// Stays on the thread that initialized it.
    _not_send: PhantomData<*const ()>,
}

impl Com {
    fn init() -> Com {
        // SAFETY: plain FFI call; balanced by Drop when it succeeds.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            tracing::warn!("CoInitializeEx: {:?}", hr.message());
        }
        Com {
            added: hr.is_ok(),
            _not_send: PhantomData,
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.added {
            // SAFETY: balances this guard's successful CoInitializeEx, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// 48 kHz 32-bit float in `stream`'s channel count (stereo or mono).
pub(super) fn stream_format(stream: AudioStream) -> WAVEFORMATEXTENSIBLE {
    let channels = stream.channels() as u16;
    let bits = 32u16;
    let block = channels * bits / 8;
    let mask = if channels == 1 {
        SPEAKER_FRONT_CENTER
    } else {
        SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: bits,
        },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

pub(super) fn invalidated(e: &anyhow::Error) -> bool {
    e.downcast_ref::<windows::core::Error>()
        .is_some_and(|e| e.code() == AUDCLNT_E_DEVICE_INVALIDATED)
}

/// The eConsole default endpoint for `flow`.
pub(super) unsafe fn default_endpoint(flow: EDataFlow) -> Result<IMMDevice> {
    // SAFETY: plain COM calls; COM is initialized on this thread.
    unsafe {
        let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("audio device enumerator")?;
        en.GetDefaultAudioEndpoint(flow, eConsole)
            .context("no default audio endpoint")
    }
}

/// The endpoint's id string, without its NUL.
pub(super) unsafe fn endpoint_id(device: &IMMDevice) -> Result<Vec<u16>> {
    // SAFETY: GetId returns a NUL-terminated CoTaskMem string, copied then freed.
    unsafe {
        let p = device.GetId().context("endpoint id")?;
        let id = p.as_wide().to_vec();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(id)
    }
}

/// An auto-reset event for the stream's buffer events, closed on drop.
pub(super) struct Event(pub(super) HANDLE);

impl Event {
    pub(super) fn new() -> Result<Event> {
        // SAFETY: plain FFI call; the handle is closed by Drop.
        let h = unsafe { CreateEventW(None, false, false, None) }.context("CreateEventW")?;
        Ok(Event(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The thread's MMCSS "Pro Audio" membership, left on drop.
pub(super) struct Mmcss(Option<HANDLE>);

impl Mmcss {
    pub(super) fn join() -> Mmcss {
        let mut index = 0u32;
        // SAFETY: plain FFI call; `index` outlives it.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) } {
            Ok(h) => Mmcss(Some(h)),
            Err(e) => {
                warn!("MMCSS Pro Audio: {e}; running at normal priority");
                Mmcss(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            // SAFETY: the handle AvSetMmThreadCharacteristicsW returned, on the same thread.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_format_is_48k_float_front_center() {
        let f = stream_format(AudioStream::PcMic);
        let base = f.Format;
        assert_eq!({ base.nChannels }, 1);
        assert_eq!({ base.nSamplesPerSec }, 48_000);
        assert_eq!({ base.wBitsPerSample }, 32);
        assert_eq!({ base.nBlockAlign }, 4);
        assert_eq!({ base.nAvgBytesPerSec }, 48_000 * 4);
        assert_eq!({ f.dwChannelMask }, SPEAKER_FRONT_CENTER);
        assert_eq!({ f.SubFormat }, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
    }
}
