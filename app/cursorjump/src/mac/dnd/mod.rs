//! File drag-and-drop on the Mac: helpers shared by its pieces.
//!
//! AppKit objects live on the main thread, which runs the app's
//! `NSApplication` run loop (the tray's event loop); that loop serves GCD's
//! main queue, so work for AppKit is sent there.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use cj_core::dnd::{DragId, DragItem, FailReason};
use dispatch2::DispatchQueue;
use objc2::runtime::AnyClass;
use objc2::{MainThreadMarker, msg_send};
use tokio::sync::oneshot;
use tracing::warn;

use crate::common::dnd_link::Source;
use crate::common::transfer::Dest;
use crate::mac::control::{Crossing, InjectMsg};

pub mod catcher;
#[cfg(debug_assertions)]
pub mod smoke_catcher;
#[cfg(debug_assertions)]
pub mod smoke_drag;
pub mod source;

/// What the drag source and the catcher tell the app (Task 17).
#[derive(Debug)]
pub enum MacDndEv {
    /// The session of `drag`'s `crossing` ended here, on a target that
    /// took it (`dropped`) or not. Exactly one per crossing, a failed
    /// `prepare` included (crossing 0 if it never got a token). A drag
    /// that went home and came back ends once per crossing: only its
    /// latest crossing's end counts for the drag. A drop whose promise
    /// writes still wait is dropped: the outcome comes from the fetches
    /// (`incoming_outcome`).
    Ended {
        drag: DragId,
        crossing: Crossing,
        dropped: bool,
    },
    /// Fetch item `item` of `drag` into `dest`, a staging folder
    /// (`Dest::InDir`), and reply with the received path or why it failed.
    /// One per item not already here, sent from the prefetch thread just
    /// after `prepare`. A reply dropped unanswered fails the item. The source drops its receiver when the drag goes home or is
    /// cancelled: stop the fetch when `reply.closed()` resolves.
    Fetch {
        drag: DragId,
        item: u32,
        dest: Dest,
        reply: oneshot::Sender<Result<PathBuf, FailReason>>,
    },
    /// The user let go while the drag rode here, at `at` (global points,
    /// origin top left of the main display): R49, the pill starts now.
    #[cfg_attr(
        debug_assertions,
        expect(dead_code, reason = "Task 17 starts the pill from it")
    )]
    Released { drag: DragId, at: (f64, f64) },
    /// Every item of `drag`'s `crossing` is in place.
    Ready { drag: DragId, crossing: Crossing },
    /// A session started just as its drag was cancelled: cj-inject posts
    /// Esc to end it undropped.
    NeedsEscape { drag: DragId, crossing: Crossing },
    /// Item `item` of `drag`'s `crossing` failed; cj-inject cancels the
    /// drag.
    #[cfg_attr(
        all(debug_assertions, not(test)),
        expect(dead_code, reason = "Task 17 tells the pill why")
    )]
    Failed {
        drag: DragId,
        crossing: Crossing,
        item: u32,
        reason: FailReason,
    },
    /// Catcher: a drag from a Mac app is on the strip: its items, and
    /// where each is read from. A promised file's source is pending until
    /// the drop receives it; if no drop comes, it fails (the sender is
    /// dropped). The latest offer wins; it travels with the `Leave` that
    /// carries `drag`.
    Offer {
        drag: DragId,
        items: Vec<DragItem>,
        sources: Vec<Source>,
    },
    /// Catcher: `drag` left the strip, or its drop there was refused,
    /// without a `Leave` carrying it: forget its offer.
    OfferGone { drag: DragId },
    /// Catcher: `drag` dropped on the strip while a push-out waited for a
    /// late drag (R48): cj-inject sends the `Leave` with it now.
    LateDrop { drag: DragId },
}

impl MacDndEv {
    /// What cj-inject must be told of this event: Task 17's events
    /// callback sends it on the injection thread's channel.
    pub fn inject_msg(&self) -> Option<InjectMsg> {
        match *self {
            MacDndEv::Ended { drag, crossing, .. } => Some(InjectMsg::DragEnded { drag, crossing }),
            MacDndEv::Ready { drag, crossing } => Some(InjectMsg::DragReady { drag, crossing }),
            MacDndEv::Failed { drag, crossing, .. } => {
                Some(InjectMsg::DragFailed { drag, crossing })
            }
            MacDndEv::NeedsEscape { drag, crossing } => {
                Some(InjectMsg::EscapeSession { drag, crossing })
            }
            MacDndEv::LateDrop { drag } => Some(InjectMsg::LateDrop { drag }),
            MacDndEv::Fetch { .. }
            | MacDndEv::Released { .. }
            | MacDndEv::Offer { .. }
            | MacDndEv::OfferGone { .. } => None,
        }
    }
}

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

/// `~/Library/Caches/CursorJump/<name>` (the temp folder without a home).
fn cache_dir(name: &str) -> PathBuf {
    let caches = std::env::var_os("HOME").map_or_else(std::env::temp_dir, |home| {
        PathBuf::from(home).join("Library").join("Caches")
    });
    caches.join("CursorJump").join(name)
}

/// `[CATransaction flush]`: commits the layer tree (AppKit's windows are
/// layer-backed) without waiting for the run loop to turn.
fn flush_core_animation() {
    if let Some(cls) = AnyClass::get(c"CATransaction") {
        // SAFETY: `+[CATransaction flush]` takes and returns nothing.
        let _: () = unsafe { msg_send![cls, flush] };
    }
}

/// Run `f`, catching and logging a panic (then giving `fallback`): for
/// code AppKit calls, which must not be unwound into. `who` names the
/// piece, `what` the callback.
fn guarded<T>(who: &str, what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        warn!("{who}: {what} panicked");
        fallback
    })
}
