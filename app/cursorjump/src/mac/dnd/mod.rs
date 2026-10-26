//! File drag-and-drop on the Mac: helpers shared by its pieces.
//!
//! AppKit objects live on the main thread, which runs the app's
//! `NSApplication` run loop (the tray's event loop); that loop serves GCD's
//! main queue, so work for AppKit is sent there.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use tracing::warn;

#[cfg(debug_assertions)]
pub mod smoke_drag;
pub mod source;

/// Run `f` on the main thread, later: queued on GCD's main queue, even when
/// called from the main thread. A panic in `f` is caught and logged.
pub fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        // SAFETY: the main queue runs its blocks on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        if catch_unwind(AssertUnwindSafe(|| f(mtm))).is_err() {
            warn!("a main-thread task panicked");
        }
    });
}

/// Run `f` on the main thread and wait up to `timeout` for its result.
///
/// - Called on the main thread, `f` runs inline, at once, whatever the
///   timeout: queueing it and waiting there would deadlock.
/// - On timeout this returns `None`, but `f` is still queued and runs later;
///   its result is then dropped. That is why `f` and `R` are `'static`.
/// - A panic in `f` is caught and logged, and gives `None`.
pub fn on_main_sync<R: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Option<R> {
    if let Some(mtm) = MainThreadMarker::new() {
        return catch_unwind(AssertUnwindSafe(|| f(mtm)))
            .inspect_err(|_| warn!("a main-thread task panicked"))
            .ok();
    }
    let (tx, rx) = mpsc::sync_channel(1);
    on_main(move |mtm| {
        // Fails once the caller has stopped waiting.
        let _ = tx.send(f(mtm));
    });
    rx.recv_timeout(timeout).ok()
}
