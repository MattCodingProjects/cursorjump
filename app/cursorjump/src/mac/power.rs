//! Waking the display, keeping the Mac awake while it is in use, and
//! hearing that it is about to sleep.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::Mutex;

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{NSWorkspace, NSWorkspaceWillSleepNotification};
use objc2_core_foundation::CFString;
use objc2_foundation::{
    NSActivityOptions, NSNotification, NSOperationQueue, NSProcessInfo, NSString,
};
use objc2_io_kit::{
    IOPMAssertionCreateWithName, IOPMAssertionDeclareUserActivity, IOPMAssertionID,
    IOPMAssertionRelease, IOPMUserActiveType, kIOPMAssertionLevelOn,
};
use tracing::warn;

/// `kIOReturnSuccess`.
const IO_RETURN_SUCCESS: i32 = 0;
/// `kIOPMAssertionTypePreventUserIdleSystemSleep` (a CFSTR macro in IOKit).
const PREVENT_USER_IDLE_SYSTEM_SLEEP: &str = "PreventUserIdleSystemSleep";
const ACTIVITY_NAME: &str = "CursorJump input from the PC";
const STAY_AWAKE_NAME: &str = "CursorJump is connected to the PC";
const ACTIVITY_REASON: &str = "CursorJump is connected";

/// The user-activity assertion; reused on each call so repeated wakes
/// refresh one assertion instead of creating new ones.
static USER_ACTIVITY: Mutex<IOPMAssertionID> = Mutex::new(0);

/// Tells the system a local user is active: wakes the display and resets
/// the idle timer.
pub fn wake_display() {
    let mut id = USER_ACTIVITY.lock().unwrap_or_else(|e| e.into_inner());
    let name = CFString::from_static_str(ACTIVITY_NAME);
    // SAFETY: `name` is a valid CFString and `id` points to a live u32.
    let ret = unsafe {
        IOPMAssertionDeclareUserActivity(Some(&name), IOPMUserActiveType::Local, &mut *id)
    };
    if ret != IO_RETURN_SUCCESS {
        warn!("IOPMAssertionDeclareUserActivity failed: {ret:#x}");
    }
}

/// Prevents idle system sleep while it exists.
#[derive(Debug)]
pub struct StayAwake(IOPMAssertionID);

impl StayAwake {
    pub fn new() -> anyhow::Result<StayAwake> {
        let kind = CFString::from_static_str(PREVENT_USER_IDLE_SYSTEM_SLEEP);
        let name = CFString::from_static_str(STAY_AWAKE_NAME);
        let mut id: IOPMAssertionID = 0;
        // SAFETY: both strings are valid CFStrings and `id` is a live u32.
        let ret = unsafe {
            IOPMAssertionCreateWithName(Some(&kind), kIOPMAssertionLevelOn, Some(&name), &mut id)
        };
        if ret != IO_RETURN_SUCCESS {
            anyhow::bail!("IOPMAssertionCreateWithName failed: {ret:#x}");
        }
        Ok(StayAwake(id))
    }
}

impl Drop for StayAwake {
    fn drop(&mut self) {
        let ret = IOPMAssertionRelease(self.0);
        if ret != IO_RETURN_SUCCESS {
            warn!("IOPMAssertionRelease failed: {ret:#x}");
        }
    }
}

/// Keeps this Mac responsive while the PC is connected: no idle system
/// sleep (`StayAwake`), and a latency-critical, user-initiated activity so
/// App Nap never throttles the agent. Both end when it is dropped.
pub struct KeepAwake {
    _stay: StayAwake,
    activity: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl KeepAwake {
    pub fn start() -> anyhow::Result<KeepAwake> {
        let stay = StayAwake::new()?;
        let options = NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical;
        let activity = NSProcessInfo::processInfo()
            .beginActivityWithOptions_reason(options, &NSString::from_str(ACTIVITY_REASON));
        Ok(KeepAwake {
            _stay: stay,
            activity,
        })
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: `activity` is the token beginActivityWithOptions returned,
        // ended exactly once.
        unsafe { NSProcessInfo::processInfo().endActivity(&self.activity) };
    }
}

/// Runs `f` on the main thread each time the Mac is about to sleep
/// (`NSWorkspaceWillSleepNotification`). The returned observer is never
/// removed: keep it for the life of the process. `f` must not block.
pub fn on_will_sleep(
    _mtm: MainThreadMarker,
    f: impl Fn() + 'static,
) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        if catch_unwind(AssertUnwindSafe(&f)).is_err() {
            warn!("the will-sleep handler panicked");
        }
    });
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    // SAFETY: a static AppKit name; the center copies the block and runs it
    // on the main queue.
    unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceWillSleepNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_awake_begins_and_ends() {
        let k = KeepAwake::start().expect("keep awake");
        drop(k);
    }

    #[test]
    fn stay_awake_creates_and_releases_an_assertion() {
        let a = StayAwake::new().expect("power assertion");
        assert_ne!(a.0, 0);
        drop(a);
    }
}
