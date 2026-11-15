//! The PC's audio devices, through WASAPI shared mode on the eConsole
//! defaults only: the Mac's sound played on the default output
//! ([`render`]), and which devices exist and whether Windows lets desktop
//! apps use the microphone ([`devices`]).
pub mod devices;
pub mod render;

use std::marker::PhantomData;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

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
