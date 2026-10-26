//! THROWAWAY spike for file drag-and-drop on the Mac (plan Task 1b). It proves the AppKit
//! mechanics before the real build and is deleted or ignored afterwards.
//!
//! Every drag is driven by **scripted posted CGEvents**, made the way the Mac injector
//! (`src/mac/inject.rs`) makes them: a `CGEventSource(HIDSystemState)` with local-events
//! suppression 0, `CGEvent::post(HIDEventTap)`, delta fields, button number 0 and click
//! state 1 on down/dragged/up, and a mouse-moved event after every warp. So **running it
//! moves your real cursor and presses the (virtual) left button**. Do not touch the mouse
//! or trackpad while a scripted run is going.
//!
//! The app is an Accessory app (no Dock icon) and never activates itself: some other app
//! stays frontmost, as in the real situation. The main thread runs the AppKit run loop;
//! the scripted posting runs on a thread named `bg`.
//!
//! Every line is logged as `[+seconds.millis] [thread] text` to stdout and to a log file
//! under `$TMPDIR/cursorjump-spike/` (the path is printed at start). At start it logs
//! `CGPreflightPostEventAccess()` and `AXIsProcessTrusted()`; when they are false, give the
//! terminal app Accessibility (System Settings > Privacy & Security > Accessibility) or the
//! posted events go nowhere.
//!
//! # Common flags
//!
//! - `--countdown SECS` (default 5): a countdown, then the cursor position is read. Point
//!   with the trackpad during the countdown, then let go of it.
//! - `--quit-after SECS`: how long to stay after the scripted sequence ends (default 20;
//!   120 for `catcher --drive manual`). The quit also waits for any provider, promise
//!   writer or promise reader still running, so a 60 s wait extends it automatically.
//! - `--move-ms MS` (default 800): how long a scripted drag takes, posting a dragged event
//!   about every 8 ms; the first three steps are 2.5 pt each to cross the drag threshold.
//!
//! `CJ_AT=x,y` (environment, global CG points): no countdown; the cursor is warped to that
//! point, which is used as T (source) or P (catcher), so runs can be unattended.
//!
//! On every exit path (normal quit, Ctrl+C / SIGTERM, a panic) a LeftMouseUp is posted if
//! a posted LeftMouseDown is still outstanding.
//!
//! # Source (A1, A2, A3): a drag that starts on our tiny panel
//!
//! ```text
//! cargo run -p cursorjump --example dnd_spike_mac -- source [--kind promise|url|both]
//!     [--size-mb 5] [--from PATH] [--wait-secs 60] [--wait-when drop|first]
//!     [--end up|esc|esc-then-up|mask-none|hold]
//!     [--start left-edge|here] [--hover-ms 1500] [--jiggle] [--panel-alpha 0.01]
//!     [--hold-write rename|inplace] [--start-mode down|late-real|late-synth|late-timer]
//!     [--late-ms 240]
//!     [--countdown 5] [--move-ms 800] [--quit-after 20]
//! ```
//! 1. Payload: `--from PATH` serves a copy of that file under its name; otherwise
//!    `spike-<N>MB.bin` (`--size-mb`, default 5) is generated in the spike directory.
//! 2. Countdown: point at the drop target T (a WhatsApp chat, a Gmail compose window, a
//!    Discord channel, a Finder window, Downloads, the desktop).
//! 3. Start point S: `--start left-edge` (default) is 2 pt inside the main display's left
//!    edge at T's height, like a PC cursor entering; `--start here` is 12 pt left of T
//!    (right of T near the left edge), so the panel is not under the drop point. An 8x8 pt
//!    borderless non-activating panel (status level, all Spaces, alpha `--panel-alpha`) is
//!    shown centred on S. The script warps the cursor to S, posts a moved event, then a
//!    LeftMouseDown. The panel's view starts an `NSDraggingSession` in `mouseDown:`.
//!    Logged: whether `mouseDown:` arrived at all, `willBeginAtPoint`, every 25th
//!    `movedToPoint`, the first `sourceOperationMask` call and a count.
//! 4. `--kind promise` (default, A1): an `NSFilePromiseProvider` (file type = the UTType of
//!    the payload's extension). Logged: `fileNameForType` and `writePromiseToURL` with
//!    thread and time relative to the release; the writer waits `--wait-secs` (default 60)
//!    on its own `NSOperationQueue`, then copies the payload to the URL and completes.
//!    `--kind url` (A2): an `NSPasteboardItem` whose data provider serves
//!    `public.file-url`. Every provider call is logged (thread, time, before/after the
//!    release, call number). `--wait-when drop` (default): before the release it answers at
//!    once with a placeholder `$TMPDIR/cursorjump-spike/incoming/<name>` (an empty file);
//!    after the release it blocks `--wait-secs` (on the main thread, where AppKit calls
//!    it), then writes the real bytes there and answers the same URL. `--wait-when first`:
//!    the first call blocks, whenever it comes. The summary says whether the provider was
//!    asked again after the release or the pasteboard served its cached answer.
//!    `--start-mode` (finding F1: Chrome starts its session late, from a synthesized event):
//!    `down` (default) starts in `mouseDown:` with the real posted event; `late-real` starts
//!    on the first `mouseDragged:` >= `--late-ms` (default 240) after the down, passing that
//!    real event; `late-synth` starts at the same moment with a synthesized
//!    LeftMouseDragged built like Chromium's (`mouseEventWithType:` with NSApp.currentEvent's
//!    window location and timestamp, our window number, event number 0, click count 1,
//!    pressure 1); `late-timer` starts from a `dispatch_after(--late-ms)` on the main queue
//!    with the synthesized event (closest to Chrome's IPC-driven start). Every
//!    `mouseDragged:` the view gets, before and after the start, is logged with the drag
//!    pasteboard's changeCount.
//! 5. The script drags S to T over `--move-ms`, then hovers `--hover-ms` (default 1500;
//!    `--jiggle` adds a +-1 pt dragged event every 50 ms).
//!    `--kind both`: one item whose writer (an `NSFilePromiseProvider` subclass) offers the
//!    promise AND `public.file-url` pointing at the placeholder (answered at once, so it
//!    is empty unless `--end hold` filled it). The summary says which one the target
//!    used: `writePromiseToURL` called means the promise; a drop without it means the URL.
//! 6. `--end`:
//!    - `up` (default): RELEASE = LeftMouseUp at T.
//!    - `hold`: after the hover the button stays down while the script writes the payload
//!      into the placeholder over `--wait-secs` (a +-1 pt dragged jiggle every 500 ms keeps
//!      the session alive), then RELEASE. `--hold-write rename` (default) writes a temp
//!      file in the same directory and renames it over the placeholder; `inplace`
//!      truncates and writes the placeholder's own inode. In this mode the promise writer
//!      and the URL provider do not wait again after the release.
//!    - `esc`: Escape down/up (keycode 0x35) from the same source, wait 1 s, log whether
//!      the session ended. The button stays (virtually) down until the quit posts the up.
//!    - `esc-then-up`: Escape down/up, 300 ms, LeftMouseUp at T (the real app's
//!      cancel-then-release-all order).
//!    - `mask-none`: the source's operation mask turns to None, two more dragged events
//!      (T+1 pt, T) make AppKit ask again, then LeftMouseUp at T.
//!
//!    `endedAtPoint:operation:` logs the operation and the time since the release or Esc.
//!
//! # Catcher (A7): an edge drop target
//!
//! ```text
//! cargo run -p cursorjump --example dnd_spike_mac -- catcher [--drive posted|manual] [--alpha 0.01]
//!     [--reject] [--types-all] [--hover-ms 800] [--countdown 5] [--move-ms 800] [--quit-after SECS]
//! ```
//! A 2 pt strip on the main display's left edge, full height: borderless non-activating
//! panel, status level, all Spaces + full-screen auxiliary + stationary, alpha `--alpha`
//! (default 0.01; e.g. 0.5 to see it). Registered for `public.file-url` plus
//! `NSFilePromiseReceiver.readableDraggedTypes`; `--types-all` adds `public.url`,
//! `public.image`, `public.png`, `public.jpeg`, `public.tiff`, `public.utf8-plain-text` and
//! `public.html`.
//! - `draggingEntered:` logs the source's operation mask, every pasteboard item's types,
//!   the file URLs with their sizes on disk and the file promises (types, and whether the
//!   names are known before the drop). It answers Copy, or None with `--reject`.
//! - `performDragOperation:` logs the time since the posted up, the file URLs, and receives
//!   each promise into a fresh `$TMPDIR/cursorjump-spike/recv-<n>/` (reader: URL, error,
//!   size, time). `draggingUpdated:` is counted; exits, prepare/conclude/ended are logged.
//! - `--drive posted` (default): countdown (point at the file or image to drag), then
//!   LeftMouseDown there, 200 ms, a drag to the strip (x = left edge + 1 pt) over
//!   `--move-ms`, a `--hover-ms` hover, RELEASE = LeftMouseUp on the strip.
//! - `--drive manual`: nothing is posted; drag onto the strip with the trackpad yourself.
//!
//! # Self-check (no posting, no drag)
//!
//! `-- selfcheck` registers every class (debug builds check the overridden method
//! signatures), calls the promise delegate and the URL provider directly (with a 0 s wait)
//! and builds a dragging item with its icon, then prints PASS or FAIL. It posts nothing.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use block2::{DynBlock, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2::{define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSDragOperation,
        NSDraggingContext, NSDraggingDestination, NSDraggingInfo, NSDraggingItem,
        NSDraggingSession, NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType,
        NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSFilePromiseReceiver, NSPanel,
        NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardTypeFileURL,
        NSPasteboardURLReadingFileURLsOnlyKey, NSPasteboardWritingOptions, NSResponder,
        NSStatusWindowLevel, NSView, NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
    };
    use objc2_core_foundation::{CFRetained, CGPoint};
    use objc2_core_graphics::{
        CGAssociateMouseAndMouseCursorPosition, CGDisplayBounds, CGError, CGEvent, CGEventField,
        CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventType,
        CGMainDisplayID, CGMouseButton, CGPreflightPostEventAccess, CGWarpMouseCursorPosition,
    };
    use objc2_foundation::{
        NSArray, NSDictionary, NSError, NSNumber, NSOperationQueue, NSPoint, NSProcessInfo, NSRect,
        NSSize, NSString, NSURL, ns_string,
    };
    use objc2_uniform_type_identifiers::UTType;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        /// Returns a C `Boolean` (unsigned char).
        fn AXIsProcessTrusted() -> u8;
    }

    const KC_ESCAPE: u16 = 0x35;
    const STEP_MS: u64 = 8;
    const NOT_YET: u64 = u64::MAX;

    // ---------------------------------------------------------------- logging

    static START: OnceLock<Instant> = OnceLock::new();
    static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

    macro_rules! log {
        ($($a:tt)*) => { log_line(&format!($($a)*)) };
    }

    fn log_line(s: &str) {
        let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
        let th = std::thread::current();
        let name = th
            .name()
            .map_or_else(|| format!("{:?}", th.id()), str::to_string);
        let line = format!("[+{t:9.3}] [{name}] {s}");
        println!("{line}");
        if let Some(f) = LOG_FILE.get()
            && let Ok(mut f) = f.lock()
        {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }

    fn spike_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cursorjump-spike");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn open_log(mode: &str) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = spike_dir().join(format!("{mode}-{secs}.log"));
        match File::create(&path) {
            Ok(f) => {
                let _ = LOG_FILE.set(Mutex::new(f));
                log!("log file: {}", path.display());
            }
            Err(e) => log!("could not create log file {}: {e}", path.display()),
        }
    }

    fn now_us() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
    }

    fn sleep_ms(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }

    // ---------------------------------------------------------------- shared state

    /// When the scripted LeftMouseUp was posted (µs since start), or NOT_YET.
    static RELEASE_AT: AtomicU64 = AtomicU64::new(NOT_YET);
    /// When the scripted Escape was posted (µs since start), or NOT_YET.
    static ESC_AT: AtomicU64 = AtomicU64::new(NOT_YET);
    /// A posted LeftMouseDown has no matching up yet.
    static LAST_DRAG_CC: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
    static DOWN_EVNUM: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    static DOWN_OUTSTANDING: AtomicBool = AtomicBool::new(false);
    /// Provider, promise writer or promise reader work still running.
    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    /// `--end mask-none`: the source answers NSDragOperationNone from now on.
    static CANCEL: AtomicBool = AtomicBool::new(false);

    static MOUSE_DOWN_SEEN: AtomicBool = AtomicBool::new(false);
    /// When `mouseDown:` reached the panel view (µs since start), or NOT_YET.
    static DOWN_AT: AtomicU64 = AtomicU64::new(NOT_YET);
    /// `beginDraggingSession` has been called (once per run).
    static DRAG_STARTED: AtomicBool = AtomicBool::new(false);
    /// What the session was started from, for the summary.
    static STARTED_HOW: Mutex<String> = Mutex::new(String::new());
    static VIEW_DRAGGED_AFTER: AtomicU32 = AtomicU32::new(0);
    static VIEW_DRAGGED: AtomicU32 = AtomicU32::new(0);
    static SESSION_BEGAN: AtomicBool = AtomicBool::new(false);
    static SESSION_ENDED: AtomicBool = AtomicBool::new(false);
    static ENDED_OP: AtomicU64 = AtomicU64::new(NOT_YET);
    static MASK_CALLS: AtomicU32 = AtomicU32::new(0);
    static MOVED_CALLS: AtomicU32 = AtomicU32::new(0);
    static PROVIDER_BEFORE: AtomicU32 = AtomicU32::new(0);
    static PROVIDER_AFTER: AtomicU32 = AtomicU32::new(0);
    static PROMISE_NAME_CALLS: AtomicU32 = AtomicU32::new(0);
    static PROMISE_WRITE_CALLS: AtomicU32 = AtomicU32::new(0);
    static BOTH_URL_CALLS: AtomicU32 = AtomicU32::new(0);

    static UPDATED_CALLS: AtomicU32 = AtomicU32::new(0);
    static PERFORMED: AtomicBool = AtomicBool::new(false);
    static RECV_DIRS: AtomicU32 = AtomicU32::new(0);

    fn released() -> bool {
        RELEASE_AT.load(Ordering::SeqCst) != NOT_YET
    }

    /// "before release", or "+N ms after release" (and after Esc, when there was one).
    fn rel() -> String {
        let now = now_us();
        let since = |at: u64| (now.saturating_sub(at)) as f64 / 1000.0;
        let r = RELEASE_AT.load(Ordering::SeqCst);
        let e = ESC_AT.load(Ordering::SeqCst);
        let mut s = if r == NOT_YET {
            "before release".to_string()
        } else {
            format!("+{:.1} ms after release", since(r))
        };
        if e != NOT_YET {
            s.push_str(&format!(", +{:.1} ms after Esc", since(e)));
        }
        s
    }

    /// Increments IN_FLIGHT for its lifetime.
    struct InFlight;

    impl InFlight {
        fn new() -> InFlight {
            IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
            InFlight
        }
    }

    impl Drop for InFlight {
        fn drop(&mut self) {
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }

    // ---------------------------------------------------------------- arguments

    struct Args {
        mode: String,
        vals: HashMap<String, String>,
        flags: Vec<String>,
    }

    const COMMON_VALS: &[&str] = &["--countdown", "--quit-after", "--move-ms"];
    const SOURCE_VALS: &[&str] = &[
        "--kind",
        "--size-mb",
        "--from",
        "--wait-secs",
        "--wait-when",
        "--end",
        "--start",
        "--hover-ms",
        "--panel-alpha",
        "--hold-write",
        "--start-mode",
        "--late-ms",
    ];
    const SOURCE_FLAGS: &[&str] = &["--jiggle"];
    const CATCHER_VALS: &[&str] = &["--drive", "--alpha", "--hover-ms"];
    const CATCHER_FLAGS: &[&str] = &["--reject", "--types-all"];

    fn usage() -> ! {
        eprintln!(
            "usage: dnd_spike_mac source|catcher [flags]  (see the module doc of \
             app/cursorjump/examples/dnd_spike_mac.rs)"
        );
        std::process::exit(2);
    }

    impl Args {
        fn parse() -> Args {
            let mut it = std::env::args().skip(1);
            let mode = it.next().unwrap_or_else(|| usage());
            let (vals_ok, flags_ok): (&[&str], &[&str]) = match mode.as_str() {
                "source" => (SOURCE_VALS, SOURCE_FLAGS),
                "catcher" => (CATCHER_VALS, CATCHER_FLAGS),
                "selfcheck" => (&[], &[]),
                _ => usage(),
            };
            let mut vals = HashMap::new();
            let mut flags = Vec::new();
            while let Some(a) = it.next() {
                if COMMON_VALS.contains(&a.as_str()) || vals_ok.contains(&a.as_str()) {
                    let Some(v) = it.next() else {
                        eprintln!("{a} needs a value");
                        usage();
                    };
                    vals.insert(a, v);
                } else if flags_ok.contains(&a.as_str()) {
                    flags.push(a);
                } else {
                    eprintln!("unknown argument for {mode}: {a}");
                    usage();
                }
            }
            Args { mode, vals, flags }
        }

        fn num<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
            match self.vals.get(key) {
                None => default,
                Some(v) => v.parse().unwrap_or_else(|_| {
                    eprintln!("bad value for {key}: {v}");
                    usage()
                }),
            }
        }

        fn choice(&self, key: &str, allowed: &[&str]) -> String {
            match self.vals.get(key) {
                None => allowed[0].to_string(),
                Some(v) if allowed.contains(&v.as_str()) => v.clone(),
                Some(v) => {
                    eprintln!("{key} must be one of {allowed:?}, not {v}");
                    usage()
                }
            }
        }

        fn flag(&self, key: &str) -> bool {
            self.flags.iter().any(|f| f == key)
        }
    }

    // ---------------------------------------------------------------- posting

    /// Posts mouse and key events the way `src/mac/inject.rs` does.
    struct Poster {
        source: CFRetained<CGEventSource>,
    }

    impl Poster {
        fn new() -> Option<Poster> {
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)?;
            CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
            Some(Poster { source })
        }

        /// A left-button mouse event (or a moved event) at `at`, in global CG points.
        fn mouse(&self, ty: CGEventType, at: (f64, f64), delta: (i64, i64)) {
            let point = CGPoint::new(at.0, at.1);
            let button = CGMouseButton::Left;
            let Some(ev) = CGEvent::new_mouse_event(Some(&self.source), ty, point, button) else {
                log!("CGEventCreateMouseEvent failed for {ty:?}");
                return;
            };
            let set = |field, value| CGEvent::set_integer_value_field(Some(&ev), field, value);
            set(CGEventField::MouseEventDeltaX, delta.0);
            set(CGEventField::MouseEventDeltaY, delta.1);
            set(CGEventField::MouseEventButtonNumber, i64::from(button.0));
            if ty != CGEventType::MouseMoved {
                set(CGEventField::MouseEventClickState, 1);
            }
            // CJ_PRESSURE=1: pressure 1.0 on down/dragged (0 on up), like a real click.
            if std::env::var_os("CJ_PRESSURE").is_some() {
                let pressed = matches!(
                    ty,
                    CGEventType::LeftMouseDown | CGEventType::LeftMouseDragged
                );
                CGEvent::set_double_value_field(
                    Some(&ev),
                    CGEventField::MouseEventPressure,
                    if pressed { 1.0 } else { 0.0 },
                );
            }
            // CJ_EVNUM=1: dragged/up events reuse the down's mouse event number,
            // as a real click sequence does.
            let num = CGEvent::integer_value_field(Some(&ev), CGEventField::MouseEventNumber);
            // Deskflow's recipe: each press gets a fresh non-zero number (the
            // system's down counters + 1), kept on its dragged and up events.
            if std::env::var_os("CJ_EVNUM").is_some() {
                if ty == CGEventType::LeftMouseDown {
                    let st = CGEventSourceStateID::HIDSystemState;
                    let downs: u32 = [
                        CGEventType::LeftMouseDown,
                        CGEventType::RightMouseDown,
                        CGEventType::OtherMouseDown,
                    ]
                    .into_iter()
                    .map(|t| CGEventSource::counter_for_event_type(st, t))
                    .sum();
                    DOWN_EVNUM.store(i64::from(downs) + 1, Ordering::SeqCst);
                }
                if matches!(
                    ty,
                    CGEventType::LeftMouseDown
                        | CGEventType::LeftMouseDragged
                        | CGEventType::LeftMouseUp
                ) {
                    set(
                        CGEventField::MouseEventNumber,
                        DOWN_EVNUM.load(Ordering::SeqCst),
                    );
                }
            }
            if ty != CGEventType::LeftMouseDragged {
                log!(
                    "posted {ty:?}: created event number {num}, sent {}",
                    CGEvent::integer_value_field(Some(&ev), CGEventField::MouseEventNumber)
                );
            }
            CGEvent::set_flags(Some(&ev), CGEventFlags(0));
            match ty {
                CGEventType::LeftMouseDown => DOWN_OUTSTANDING.store(true, Ordering::SeqCst),
                CGEventType::LeftMouseUp => DOWN_OUTSTANDING.store(false, Ordering::SeqCst),
                _ => {}
            }
            // CJ_TAP=session posts at the session tap (Chrome Remote Desktop's choice).
            let tap = if std::env::var("CJ_TAP").is_ok_and(|v| v == "session") {
                CGEventTapLocation::SessionEventTap
            } else {
                CGEventTapLocation::HIDEventTap
            };
            CGEvent::post(tap, Some(&ev));
        }

        fn key(&self, kc: u16, down: bool) {
            let Some(ev) = CGEvent::new_keyboard_event(Some(&self.source), kc, down) else {
                log!("CGEventCreateKeyboardEvent failed for {kc:#x}");
                return;
            };
            CGEvent::set_flags(Some(&ev), CGEventFlags(0));
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&ev));
        }

        /// Warps the cursor like `Injector::warp_to`, then posts a moved event there.
        fn warp(&self, at: (f64, f64)) {
            let err = CGWarpMouseCursorPosition(CGPoint::new(at.0, at.1));
            if err != CGError::Success {
                log!("CGWarpMouseCursorPosition failed: {err:?}");
            }
            let err = CGAssociateMouseAndMouseCursorPosition(true);
            if err != CGError::Success {
                log!("CGAssociateMouseAndMouseCursorPosition failed: {err:?}");
            }
            self.mouse(CGEventType::MouseMoved, at, (0, 0));
        }

        /// Posts LeftMouseDragged from `from` to `to` over `move_ms`, every ~8 ms.
        fn drag(&self, from: (f64, f64), to: (f64, f64), move_ms: u64) {
            let path = drag_path(from, to, move_ms);
            log!(
                "dragging ({:.0}, {:.0}) -> ({:.0}, {:.0}) in {} steps",
                from.0,
                from.1,
                to.0,
                to.1,
                path.len()
            );
            let mut prev = from;
            for (i, p) in path.into_iter().enumerate() {
                let d = ((p.0 - prev.0).round() as i64, (p.1 - prev.1).round() as i64);
                self.mouse(CGEventType::LeftMouseDragged, p, d);
                prev = p;
                sleep_ms(STEP_MS);
                // The drag pasteboard changes when a drag session is set up.
                let drag_pb = unsafe {
                    objc2_app_kit::NSPasteboard::pasteboardWithName(
                        objc2_app_kit::NSPasteboardNameDrag,
                    )
                };
                let cc = drag_pb.changeCount();
                if cc != LAST_DRAG_CC.swap(cc as i64, Ordering::SeqCst) as isize {
                    log!("drag step {i}: drag pasteboard changeCount -> {cc}");
                }
                if i % 40 == 0 {
                    log!(
                        "drag step {i}: pressedMouseButtons {:#x}, HID left {}, combined left {}",
                        objc2_app_kit::NSEvent::pressedMouseButtons(),
                        CGEventSource::button_state(
                            CGEventSourceStateID::HIDSystemState,
                            CGMouseButton::Left
                        ),
                        CGEventSource::button_state(
                            CGEventSourceStateID::CombinedSessionState,
                            CGMouseButton::Left
                        )
                    );
                }
            }
        }
    }

    /// The points of a drag: three 2.5 pt steps towards `to` (to cross the drag
    /// threshold), then even steps to `to`. A short drag goes out 7.5 pt and back.
    fn drag_path(from: (f64, f64), to: (f64, f64), move_ms: u64) -> Vec<(f64, f64)> {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let dist = dx.hypot(dy);
        let mut pts = Vec::new();
        if dist < 10.0 {
            for i in 1..=3 {
                pts.push((from.0 + 2.5 * f64::from(i), from.1));
            }
            for i in (0..3).rev() {
                pts.push((to.0 + 2.5 * f64::from(i), to.1));
            }
            return pts;
        }
        let (ux, uy) = (dx / dist, dy / dist);
        for i in 1..=3 {
            let k = 2.5 * f64::from(i);
            pts.push((from.0 + ux * k, from.1 + uy * k));
        }
        let first = pts[2];
        let n = (move_ms / STEP_MS).max(1);
        for j in 1..=n {
            let f = j as f64 / n as f64;
            pts.push((
                (first.0 + (to.0 - first.0) * f).round(),
                (first.1 + (to.1 - first.1) * f).round(),
            ));
        }
        pts
    }

    /// Where the system cursor is now, in global CG points.
    fn cursor_location() -> (f64, f64) {
        CGEvent::new(None).map_or((0.0, 0.0), |ev| {
            let p = CGEvent::location(Some(&ev));
            (p.x, p.y)
        })
    }

    /// Main display bounds (x, y, w, h) in global CG points.
    fn main_bounds() -> (f64, f64, f64, f64) {
        let r = CGDisplayBounds(CGMainDisplayID());
        (r.origin.x, r.origin.y, r.size.width, r.size.height)
    }

    /// Global CG point (origin top-left, y down) to Cocoa screen point (origin
    /// bottom-left of the main display, y up).
    fn cg_to_cocoa(p: (f64, f64)) -> NSPoint {
        NSPoint::new(p.0, main_bounds().3 - p.1)
    }

    fn cocoa_to_cg(p: NSPoint) -> (f64, f64) {
        (p.x, main_bounds().3 - p.y)
    }

    /// Posts LeftMouseUp at the cursor if a posted down is outstanding.
    fn release_if_down(why: &str) {
        if DOWN_OUTSTANDING.load(Ordering::SeqCst) {
            let at = cursor_location();
            log!("{why}: a posted LeftMouseDown is outstanding; posting LeftMouseUp at {at:?}");
            if let Some(p) = Poster::new() {
                p.mouse(CGEventType::LeftMouseUp, at, (0, 0));
            }
        }
    }

    /// Ctrl+C / SIGTERM: post the outstanding up, then exit. The signals are
    /// blocked in every thread spawned after this (they inherit the mask) and
    /// taken by `sigwait` on a thread of their own.
    fn install_exit_guards() {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            release_if_down("panic");
            default_hook(info);
        }));
        // SAFETY: plain libc signal-mask calls on a local, zero-initialised set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            std::thread::Builder::new()
                .name("signals".into())
                .spawn(move || {
                    let mut sig: libc::c_int = 0;
                    libc::sigwait(&set, &mut sig);
                    log!("signal {sig}: quitting");
                    release_if_down("signal");
                    std::process::exit(130);
                })
                .expect("spawn signal thread");
        }
    }

    /// After the scripted sequence: stay `quit_after` s, then wait for in-flight
    /// provider/promise work, log the summary and exit.
    fn finish(quit_after: f64, summary: fn()) -> ! {
        log!(
            "sequence done; quitting in {quit_after} s (later if provider/promise work is still running)"
        );
        std::thread::sleep(Duration::from_secs_f64(quit_after.max(0.0)));
        let mut last = Instant::now();
        while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
            if last.elapsed() >= Duration::from_secs(5) {
                log!(
                    "waiting for {} in-flight provider/promise call(s)",
                    IN_FLIGHT.load(Ordering::SeqCst)
                );
                last = Instant::now();
            }
            sleep_ms(200);
        }
        summary();
        release_if_down("quit");
        log!("quit");
        std::process::exit(0);
    }

    /// The point to use: `CJ_AT=x,y` (global CG points; the cursor is warped
    /// there, no countdown), else where the user points after the countdown.
    fn pick_point(secs: u64, what: &str) -> (f64, f64) {
        let Ok(v) = std::env::var("CJ_AT") else {
            return countdown(secs, what);
        };
        let parsed = v
            .split_once(',')
            .and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)));
        let Some(at) = parsed else {
            log!("CJ_AT={v} is not x,y; exiting");
            std::process::exit(2);
        };
        log!(
            "CJ_AT={v}: no countdown; warping the cursor to ({:.1}, {:.1})",
            at.0,
            at.1
        );
        if let Some(p) = Poster::new() {
            p.warp(at);
        }
        sleep_ms(150);
        let now = cursor_location();
        log!("cursor now at ({:.1}, {:.1})", now.0, now.1);
        at
    }

    fn countdown(secs: u64, what: &str) -> (f64, f64) {
        log!("countdown {secs} s: {what}");
        for i in (1..=secs).rev() {
            log!("  {i}...");
            sleep_ms(1000);
        }
        let at = cursor_location();
        log!("cursor at ({:.1}, {:.1}) (global CG points)", at.0, at.1);
        at
    }

    fn log_permissions() {
        let can_post = CGPreflightPostEventAccess();
        // SAFETY: a plain query with no arguments.
        let trusted = unsafe { AXIsProcessTrusted() } != 0;
        log!("CGPreflightPostEventAccess() = {can_post}, AXIsProcessTrusted() = {trusted}");
        if !can_post || !trusted {
            log!(
                "!! posted events may be dropped: give the terminal app Accessibility in System \
                 Settings > Privacy & Security > Accessibility, then quit and reopen the terminal"
            );
        }
        let (x, y, w, h) = main_bounds();
        log!("main display bounds: x={x} y={y} w={w} h={h} (points)");
    }

    fn log_frontmost(when: &str) {
        let ws = NSWorkspace::sharedWorkspace();
        match ws.frontmostApplication() {
            Some(app) => log!(
                "frontmost app {when}: {} ({})",
                app.localizedName()
                    .map_or("?".to_string(), |s| s.to_string()),
                app.bundleIdentifier()
                    .map_or("?".to_string(), |s| s.to_string())
            ),
            None => log!("frontmost app {when}: none"),
        }
    }

    /// Size on disk of a path, for logs.
    fn size_of(path: &Path) -> String {
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => "a directory".to_string(),
            Ok(m) => format!("{} bytes", m.len()),
            Err(e) => format!("not readable ({e})"),
        }
    }

    fn op_name(op: NSDragOperation) -> String {
        if op == NSDragOperation::None {
            return "None".to_string();
        }
        let names = [
            (NSDragOperation::Copy, "Copy"),
            (NSDragOperation::Link, "Link"),
            (NSDragOperation::Generic, "Generic"),
            (NSDragOperation::Private, "Private"),
            (NSDragOperation::Move, "Move"),
            (NSDragOperation::Delete, "Delete"),
        ];
        let parts: Vec<&str> = names
            .iter()
            .filter(|(f, _)| op.contains(*f))
            .map(|&(_, n)| n)
            .collect();
        format!("{} ({:#x})", parts.join("|"), op.0)
    }

    /// A borderless, non-activating, transparent-ish panel at the status level
    /// on every Space, with `view` as its content.
    fn make_panel(
        mtm: MainThreadMarker,
        frame: NSRect,
        alpha: f64,
        stationary: bool,
        view: &NSView,
    ) -> Retained<NSPanel> {
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: we keep our own reference; closing must not release it.
        unsafe { panel.setReleasedWhenClosed(false) };
        let mut behavior = NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary;
        if stationary {
            behavior |=
                NSWindowCollectionBehavior::Stationary | NSWindowCollectionBehavior::IgnoresCycle;
        }
        panel.setCollectionBehavior(behavior);
        panel.setLevel(NSStatusWindowLevel);
        panel.setHidesOnDeactivate(false);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setOpaque(false);
        panel.setHasShadow(false);
        panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.5, alpha)));
        panel.setIgnoresMouseEvents(false);
        panel.setContentView(Some(view));
        panel.orderFrontRegardless();
        log!(
            "panel shown: Cocoa frame ({:.1}, {:.1}, {:.1} x {:.1}), alpha {alpha}, window number {}",
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
            panel.windowNumber()
        );
        panel
    }

    fn new_app(mtm: MainThreadMarker) -> Retained<NSApplication> {
        let app = NSApplication::sharedApplication(mtm);
        let ok = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        log!("NSApplication activation policy Accessory: {ok}");
        app
    }

    // ---------------------------------------------------------------- source

    struct Payload {
        /// The file served (a copy of `--from`, or the generated one).
        path: PathBuf,
        name: String,
        /// UTType identifier for the payload's extension (`public.data` fallback).
        uti: String,
        /// `--kind url`: the placeholder path handed out before the real bytes.
        placeholder: PathBuf,
    }

    struct SourceCfg {
        kind: String,
        /// When the session starts: `down`, `late-real`, `late-synth`, `late-timer`.
        start_mode: String,
        late_ms: u64,
        /// `--end hold`: `rename` (temp file renamed over the placeholder) or
        /// `inplace` (truncate and write the placeholder's own inode).
        hold_write: String,
        wait_secs: u64,
        wait_when: String,
        end: String,
        hover_ms: u64,
        jiggle: bool,
        move_ms: u64,
        quit_after: f64,
        payload: Payload,
    }

    static SOURCE: OnceLock<SourceCfg> = OnceLock::new();

    fn cfg() -> &'static SourceCfg {
        SOURCE.get().expect("source config")
    }

    fn uti_for(name: &str) -> String {
        let ext = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        UTType::typeWithFilenameExtension(&NSString::from_str(ext))
            .map_or_else(|| "public.data".to_string(), |t| t.identifier().to_string())
    }

    fn prepare_payload(from: Option<&String>, size_mb: u64) -> Payload {
        let dir = spike_dir().join("payload");
        let _ = std::fs::create_dir_all(&dir);
        let path = match from {
            Some(src) => {
                let src = Path::new(src);
                let name = src.file_name().unwrap_or_else(|| {
                    eprintln!("--from needs a file path");
                    usage()
                });
                let dst = dir.join(name);
                if let Err(e) = std::fs::copy(src, &dst) {
                    eprintln!("copying {} failed: {e}", src.display());
                    std::process::exit(1);
                }
                dst
            }
            None => {
                let dst = dir.join(format!("spike-{size_mb}MB.bin"));
                let mut f = File::create(&dst).expect("create payload");
                let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
                let mut buf = vec![0u8; 1 << 20];
                for _ in 0..size_mb {
                    for chunk in buf.chunks_mut(8) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
                    }
                    f.write_all(&buf).expect("write payload");
                }
                dst
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let incoming = spike_dir().join("incoming");
        let _ = std::fs::create_dir_all(&incoming);
        let placeholder = incoming.join(&name);
        let _ = File::create(&placeholder);
        let uti = uti_for(&name);
        log!(
            "payload: {} ({}), type {uti}; placeholder {}",
            path.display(),
            size_of(&path),
            placeholder.display()
        );
        Payload {
            path,
            name,
            uti,
            placeholder,
        }
    }

    fn file_url_string(path: &Path) -> Retained<NSString> {
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        url.absoluteString()
            .unwrap_or_else(|| NSString::from_str(""))
    }

    /// Blocks `secs`, logging every 10 s.
    fn wait_logged(secs: u64, what: &str) {
        log!("{what}: waiting {secs} s");
        for i in 1..=secs {
            sleep_ms(1000);
            if i.is_multiple_of(10) && i != secs {
                log!("{what}: {i}/{secs} s");
            }
        }
    }

    #[derive(Default)]
    struct DragIvars {
        /// Providers and delegates that must outlive the drag (delegates are weak).
        keep: RefCell<Vec<Retained<NSObject>>>,
    }

    define_class!(
        // SAFETY: NSView has no subclassing requirements; DragView has no Drop.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "CJSpikeDragView"]
        #[ivars = DragIvars]
        struct DragView;

        unsafe impl NSObjectProtocol for DragView {}

        impl DragView {
            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
                log!("acceptsFirstMouse: -> YES");
                true
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, event: &NSEvent) {
                self.on_down(event);
            }

            #[unsafe(method(mouseDragged:))]
            fn mouse_dragged(&self, event: &NSEvent) {
                self.on_dragged(event);
            }

            #[unsafe(method(mouseUp:))]
            fn mouse_up(&self, _event: &NSEvent) {
                log!("mouseUp: reached the panel view (the drag session did not take the release)");
            }
        }

        unsafe impl NSDraggingSource for DragView {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn source_mask(
                &self,
                _session: &NSDraggingSession,
                context: NSDraggingContext,
            ) -> NSDragOperation {
                let n = MASK_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                let cancel = CANCEL.load(Ordering::SeqCst);
                let op = if cancel {
                    NSDragOperation::None
                } else {
                    NSDragOperation::Copy
                };
                if n == 1 || cancel || n.is_multiple_of(100) {
                    log!(
                        "sourceOperationMask call #{n}, context {} -> {} ({})",
                        context.0,
                        op_name(op),
                        rel()
                    );
                }
                op
            }

            #[unsafe(method(draggingSession:willBeginAtPoint:))]
            fn will_begin(&self, _session: &NSDraggingSession, p: NSPoint) {
                SESSION_BEGAN.store(true, Ordering::SeqCst);
                let g = cocoa_to_cg(p);
                log!("session willBeginAtPoint ({:.1}, {:.1}) [CG {:.1}, {:.1}]", p.x, p.y, g.0, g.1);
            }

            #[unsafe(method(draggingSession:movedToPoint:))]
            fn moved(&self, _session: &NSDraggingSession, p: NSPoint) {
                let n = MOVED_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 || n.is_multiple_of(25) {
                    let g = cocoa_to_cg(p);
                    log!("session movedToPoint #{n} [CG {:.1}, {:.1}] ({})", g.0, g.1, rel());
                }
            }

            #[unsafe(method(draggingSession:endedAtPoint:operation:))]
            fn ended(&self, _session: &NSDraggingSession, p: NSPoint, op: NSDragOperation) {
                SESSION_ENDED.store(true, Ordering::SeqCst);
                ENDED_OP.store(op.0 as u64, Ordering::SeqCst);
                let g = cocoa_to_cg(p);
                log!(
                    "SESSION ENDED at [CG {:.1}, {:.1}] operation {} ({}); moved {} times",
                    g.0,
                    g.1,
                    op_name(op),
                    rel(),
                    MOVED_CALLS.load(Ordering::SeqCst)
                );
            }
        }
    );

    impl DragView {
        fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<DragView> {
            let this = DragView::alloc(mtm).set_ivars(DragIvars::default());
            // SAFETY: NSView's designated initializer.
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        fn keep(&self, obj: Retained<NSObject>) {
            self.ivars().keep.borrow_mut().push(obj);
        }

        fn on_down(&self, event: &NSEvent) {
            MOUSE_DOWN_SEEN.store(true, Ordering::SeqCst);
            DOWN_AT.store(now_us(), Ordering::SeqCst);
            let c = cfg();
            let loc = event.locationInWindow();
            log!(
                "mouseDown: arrived at the panel view (window point {:.1}, {:.1}; clickCount {}; {}); kind {}, start mode {}",
                loc.x,
                loc.y,
                event.clickCount(),
                describe_event(event),
                c.kind,
                c.start_mode
            );
            match c.start_mode.as_str() {
                "down" => self.start_drag(event, "the real mouseDown: event"),
                "late-timer" => schedule_late_start(self, c.late_ms),
                _ => log!(
                    "not starting yet: waiting for a mouseDragged: >= {} ms after the down",
                    c.late_ms
                ),
            }
        }

        fn on_dragged(&self, event: &NSEvent) {
            let c = cfg();
            let started = DRAG_STARTED.load(Ordering::SeqCst);
            let n = VIEW_DRAGGED.fetch_add(1, Ordering::SeqCst) + 1;
            if started {
                VIEW_DRAGGED_AFTER.fetch_add(1, Ordering::SeqCst);
            }
            let ms = ms_since_down();
            let loc = event.locationInWindow();
            log!(
                "mouseDragged: #{n} at the view, +{ms:.1} ms after the down, window point ({:.1}, {:.1}), {}, session {}, drag pasteboard changeCount {}",
                loc.x,
                loc.y,
                describe_event(event),
                if started { "STARTED" } else { "not started" },
                drag_change_count()
            );
            if started || ms < c.late_ms as f64 {
                return;
            }
            match c.start_mode.as_str() {
                "late-real" => self.start_drag(event, "a real mouseDragged: event"),
                "late-synth" => match self.synth_dragged() {
                    Some(ev) => {
                        self.start_drag(&ev, "a synthesized LeftMouseDragged (in mouseDragged:)")
                    }
                    None => log!("synthesizing the dragged event failed"),
                },
                _ => {}
            }
        }

        /// A LeftMouseDragged NSEvent built like Chromium's startDragWithDropData:
        /// the current event's window location and timestamp, our window number,
        /// event number 0, click count 1, pressure 1.
        fn synth_dragged(&self) -> Option<Retained<NSEvent>> {
            let mtm = self.mtm();
            let current = NSApplication::sharedApplication(mtm).currentEvent();
            let window = self.window();
            let wn = window.as_ref().map_or(0, |w| w.windowNumber());
            let (loc, ts) = match &current {
                Some(e) => {
                    log!("synth: from NSApp.currentEvent ({})", describe_event(e));
                    (e.locationInWindow(), e.timestamp())
                }
                None => {
                    log!(
                        "synth: no NSApp.currentEvent; mouseLocationOutsideOfEventStream + systemUptime"
                    );
                    (
                        window.as_ref().map_or(NSPoint::new(0.0, 0.0), |w| {
                            w.mouseLocationOutsideOfEventStream()
                        }),
                        NSProcessInfo::processInfo().systemUptime(),
                    )
                }
            };
            // SAFETY: the class method's own selector and argument types; the
            // context argument is documented as unused (pass nil).
            let ev: Option<Retained<NSEvent>> = unsafe {
                msg_send![
                    NSEvent::class(),
                    mouseEventWithType: NSEventType::LeftMouseDragged,
                    location: loc,
                    modifierFlags: NSEventModifierFlags(0),
                    timestamp: ts,
                    windowNumber: wn,
                    context: None::<&AnyObject>,
                    eventNumber: 0isize,
                    clickCount: 1isize,
                    pressure: 1.0f32
                ]
            };
            ev
        }

        fn start_drag(&self, event: &NSEvent, how: &str) {
            if DRAG_STARTED.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Ok(mut h) = STARTED_HOW.lock() {
                *h = format!("{how}, +{:.1} ms after the down", ms_since_down());
            }
            let c = cfg();
            let loc = event.locationInWindow();
            log!(
                "STARTING the session from {how}, +{:.1} ms after the down: window point ({:.1}, {:.1}), {}, drag pasteboard changeCount {}",
                ms_since_down(),
                loc.x,
                loc.y,
                describe_event(event),
                drag_change_count()
            );
            let item = if c.kind == "both" {
                let delegate = PromiseDelegate::new();
                let provider = BothProvider::new(&c.payload.uti, &delegate);
                let writer: &NSFilePromiseProvider = &provider;
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(writer),
                );
                self.keep(delegate.into_super());
                self.keep(provider.into_super().into_super());
                item
            } else if c.kind == "promise" {
                let delegate = PromiseDelegate::new();
                let provider = NSFilePromiseProvider::initWithFileType_delegate(
                    NSFilePromiseProvider::alloc(),
                    &NSString::from_str(&c.payload.uti),
                    ProtocolObject::from_ref(&*delegate),
                );
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(&*provider),
                );
                self.keep(delegate.into_super());
                self.keep(provider.into_super());
                item
            } else {
                let provider = UrlProvider::new();
                let pb_item = NSPasteboardItem::new();
                // SAFETY: a static AppKit string.
                let file_url = unsafe { NSPasteboardTypeFileURL };
                let ok = pb_item.setDataProvider_forTypes(
                    ProtocolObject::from_ref(&*provider),
                    &NSArray::from_slice(&[file_url]),
                );
                log!("NSPasteboardItem setDataProvider(public.file-url) -> {ok}");
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(&*pb_item),
                );
                self.keep(provider.into_super());
                self.keep(pb_item.into_super());
                item
            };
            let ut = UTType::typeWithIdentifier(&NSString::from_str(&c.payload.uti))
                .or_else(|| UTType::typeWithIdentifier(ns_string!("public.data")));
            let p = self.convertPoint_fromView(loc, None);
            let frame = NSRect::new(
                NSPoint::new(p.x - 32.0, p.y - 32.0),
                NSSize::new(64.0, 64.0),
            );
            if let Some(ut) = ut {
                let icon = NSWorkspace::sharedWorkspace().iconForContentType(&ut);
                icon.setSize(NSSize::new(64.0, 64.0));
                let contents: &AnyObject = &icon;
                // SAFETY: an NSImage is a valid dragging-frame content.
                unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
            } else {
                item.setDraggingFrame(frame);
            }
            let items = NSArray::from_retained_slice(&[item]);
            let session = self.beginDraggingSessionWithItems_event_source(
                &items,
                event,
                ProtocolObject::from_ref(self),
            );
            session.setAnimatesToStartingPositionsOnCancelOrFail(false);
            log!(
                "beginDraggingSession returned (sequence number {}); drag pasteboard changeCount {}",
                session.draggingSequenceNumber(),
                drag_change_count()
            );
        }
    }

    fn ms_since_down() -> f64 {
        match DOWN_AT.load(Ordering::SeqCst) {
            NOT_YET => -1.0,
            at => now_us().saturating_sub(at) as f64 / 1000.0,
        }
    }

    fn describe_event(e: &NSEvent) -> String {
        format!(
            "type {}, eventNumber {}, timestamp {:.3}, window {}, pressure {:.2}",
            e.r#type().0,
            e.eventNumber(),
            e.timestamp(),
            e.windowNumber(),
            e.pressure()
        )
    }

    /// The drag pasteboard's changeCount (it changes when a session is set up).
    fn drag_change_count() -> isize {
        // SAFETY: a static AppKit string.
        let name = unsafe { objc2_app_kit::NSPasteboardNameDrag };
        NSPasteboard::pasteboardWithName(name).changeCount()
    }

    #[repr(C)]
    struct DispatchQueue {
        _opaque: [u8; 0],
    }

    unsafe extern "C" {
        /// The main queue (`dispatch_get_main_queue()` is an inline function).
        static _dispatch_main_q: DispatchQueue;
        fn dispatch_time(when: u64, delta_ns: i64) -> u64;
        fn dispatch_after_f(
            when: u64,
            queue: *const DispatchQueue,
            context: *mut std::ffi::c_void,
            work: extern "C" fn(*mut std::ffi::c_void),
        );
    }

    /// `--start-mode late-timer`: starts the session from a main-queue timer
    /// `late_ms` after the down, with a synthesized dragged event.
    fn schedule_late_start(view: &DragView, late_ms: u64) {
        log!("not starting yet: dispatch_after({late_ms} ms) on the main queue");
        let ctx = Retained::into_raw(objc2::Message::retain(view)).cast::<std::ffi::c_void>();
        // SAFETY: DISPATCH_TIME_NOW is 0; the main queue is a static; the
        // context is a retained DragView that the callback takes back.
        unsafe {
            let when = dispatch_time(0, (late_ms * 1_000_000) as i64);
            dispatch_after_f(when, &raw const _dispatch_main_q, ctx, late_timer_fired);
        }
    }

    static SELFCHECK_TIMER: AtomicBool = AtomicBool::new(false);

    extern "C" fn selfcheck_timer_fired(_ctx: *mut std::ffi::c_void) {
        SELFCHECK_TIMER.store(MainThreadMarker::new().is_some(), Ordering::SeqCst);
    }

    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const std::ffi::c_void;
        fn CFRunLoopRunInMode(mode: *const std::ffi::c_void, seconds: f64, once: u8) -> i32;
    }

    extern "C" fn late_timer_fired(ctx: *mut std::ffi::c_void) {
        // SAFETY: `ctx` is the pointer `schedule_late_start` leaked from a Retained.
        let view = unsafe { Retained::from_raw(ctx.cast::<DragView>()) }.expect("view");
        log!(
            "late timer fired +{:.1} ms after the down (main thread: {})",
            ms_since_down(),
            MainThreadMarker::new().is_some()
        );
        match view.synth_dragged() {
            Some(ev) => view.start_drag(&ev, "a synthesized LeftMouseDragged (main-queue timer)"),
            None => log!("synthesizing the dragged event failed"),
        }
    }

    struct PromiseIvars {
        queue: Retained<NSOperationQueue>,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop.
        #[unsafe(super(NSObject))]
        #[name = "CJSpikePromiseDelegate"]
        #[ivars = PromiseIvars]
        struct PromiseDelegate;

        unsafe impl NSObjectProtocol for PromiseDelegate {}

        unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
            #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
            fn file_name(
                &self,
                _provider: &NSFilePromiseProvider,
                file_type: &NSString,
            ) -> Retained<NSString> {
                let n = PROMISE_NAME_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                let name = &cfg().payload.name;
                log!(
                    "promise fileNameForType({file_type}) call #{n} -> {name} ({})",
                    rel()
                );
                NSString::from_str(name)
            }

            #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
            fn write_promise(
                &self,
                _provider: &NSFilePromiseProvider,
                url: &NSURL,
                completion: &DynBlock<dyn Fn(*mut NSError)>,
            ) {
                let _busy = InFlight::new();
                let n = PROMISE_WRITE_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                let path = url.path().map(|p| p.to_string()).unwrap_or_default();
                log!("promise writePromiseToURL call #{n}: {path} ({})", rel());
                let c = cfg();
                // `--end hold` already waited (and wrote) before the release.
                let wait = if c.end == "hold" { 0 } else { c.wait_secs };
                wait_logged(wait, "promise writer");
                let res = std::fs::copy(&c.payload.path, &path);
                match &res {
                    Ok(b) => log!("promise writer: wrote {b} bytes to {path} ({})", rel()),
                    Err(e) => log!("promise writer: writing {path} failed: {e}"),
                }
                let err = res
                    .err()
                    .map(|_| NSError::new(-1, ns_string!("CursorJumpSpike")));
                let err_ptr = err
                    .as_ref()
                    .map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e).cast_mut());
                completion.call((err_ptr,));
                log!("promise writer: completion handler called ({})", rel());
            }

            #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
            fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
                log!("promise operationQueueForFilePromiseProvider ({})", rel());
                self.ivars().queue.clone()
            }
        }
    );

    impl PromiseDelegate {
        fn new() -> Retained<PromiseDelegate> {
            let queue = NSOperationQueue::new();
            queue.setName(Some(ns_string!("cj-spike-promise")));
            let this = PromiseDelegate::alloc().set_ivars(PromiseIvars { queue });
            // SAFETY: NSObject's init.
            unsafe { msg_send![super(this), init] }
        }
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; no Drop.
        #[unsafe(super(NSObject))]
        #[name = "CJSpikeUrlProvider"]
        struct UrlProvider;

        unsafe impl NSObjectProtocol for UrlProvider {}

        unsafe impl NSPasteboardItemDataProvider for UrlProvider {
            #[unsafe(method(pasteboard:item:provideDataForType:))]
            fn provide(
                &self,
                _pasteboard: Option<&NSPasteboard>,
                item: &NSPasteboardItem,
                ty: &NSString,
            ) {
                let _busy = InFlight::new();
                let after = released();
                let n = if after {
                    PROVIDER_AFTER.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    PROVIDER_BEFORE.fetch_add(1, Ordering::SeqCst) + 1
                };
                let total =
                    PROVIDER_BEFORE.load(Ordering::SeqCst) + PROVIDER_AFTER.load(Ordering::SeqCst);
                log!(
                    "provideDataForType({ty}) call #{total} (#{n} {}) ({})",
                    if after {
                        "after release"
                    } else {
                        "before release"
                    },
                    rel()
                );
                let c = cfg();
                let block = if c.end == "hold" {
                    false // the hold writes the bytes before the release
                } else if c.wait_when == "first" {
                    total == 1
                } else {
                    after && n == 1
                };
                if block {
                    wait_logged(c.wait_secs, "url provider (blocking the main thread)");
                    match std::fs::copy(&c.payload.path, &c.payload.placeholder) {
                        Ok(b) => log!(
                            "url provider: wrote {b} bytes to the placeholder ({})",
                            rel()
                        ),
                        Err(e) => log!("url provider: writing the placeholder failed: {e}"),
                    }
                }
                let url = file_url_string(&c.payload.placeholder);
                let ok = item.setString_forType(&url, ty);
                log!("provideDataForType answered {url} -> {ok} ({})", rel());
            }

            #[unsafe(method(pasteboardFinishedWithDataProvider:))]
            fn finished(&self, _pasteboard: &NSPasteboard) {
                log!("pasteboardFinishedWithDataProvider ({})", rel());
            }
        }
    );

    impl UrlProvider {
        fn new() -> Retained<UrlProvider> {
            let this = UrlProvider::alloc().set_ivars(());
            // SAFETY: NSObject's init.
            unsafe { msg_send![super(this), init] }
        }
    }

    define_class!(
        // SAFETY: NSFilePromiseProvider may be subclassed (AppKit documents
        // overriding its NSPasteboardWriting methods to add types); no Drop.
        #[unsafe(super(NSFilePromiseProvider, NSObject))]
        #[name = "CJSpikeBothProvider"]
        struct BothProvider;

        unsafe impl NSObjectProtocol for BothProvider {}

        impl BothProvider {
            #[unsafe(method_id(writableTypesForPasteboard:))]
            fn writable_types(&self, pasteboard: &NSPasteboard) -> Retained<NSArray<NSString>> {
                // SAFETY: the superclass's own method with the same arguments.
                let base: Retained<NSArray<NSString>> =
                    unsafe { msg_send![super(self), writableTypesForPasteboard: pasteboard] };
                let mut types = base.to_vec();
                types.push(NSString::from_str(&file_url_type()));
                log!("both: writableTypesForPasteboard -> [{}] ({})", join(&NSArray::from_retained_slice(&types)), rel());
                NSArray::from_retained_slice(&types)
            }

            #[unsafe(method(writingOptionsForType:pasteboard:))]
            fn writing_options(
                &self,
                ty: &NSString,
                pasteboard: &NSPasteboard,
            ) -> NSPasteboardWritingOptions {
                if ty.to_string() == file_url_type() {
                    return NSPasteboardWritingOptions(0);
                }
                // SAFETY: the superclass's own method with the same arguments.
                unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pasteboard] }
            }

            #[unsafe(method_id(pasteboardPropertyListForType:))]
            fn property_list(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
                both_property_list(self, ty)
            }
        }
    );

    /// The file URL of the placeholder for `public.file-url`; the promise's
    /// own types from the superclass.
    fn both_property_list(this: &BothProvider, ty: &NSString) -> Option<Retained<AnyObject>> {
        if ty.to_string() == file_url_type() {
            let n = BOTH_URL_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
            let url = file_url_string(&cfg().payload.placeholder);
            log!(
                "both: pasteboardPropertyListForType(public.file-url) call #{n} -> {url}, placeholder {} ({})",
                size_of(&cfg().payload.placeholder),
                rel()
            );
            return Some(url.into_super().into_super());
        }
        // SAFETY: the superclass's own method with the same argument.
        let v: Option<Retained<AnyObject>> =
            unsafe { msg_send![super(this), pasteboardPropertyListForType: ty] };
        log!(
            "both: pasteboardPropertyListForType({ty}) -> super ({})",
            rel()
        );
        v
    }

    impl BothProvider {
        /// A file promise of type `uti` that also offers `public.file-url`
        /// (the placeholder), on one pasteboard item.
        fn new(uti: &str, delegate: &PromiseDelegate) -> Retained<BothProvider> {
            let this = BothProvider::alloc().set_ivars(());
            let uti = NSString::from_str(uti);
            let delegate = ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(delegate);
            // SAFETY: NSFilePromiseProvider's designated initializer.
            unsafe { msg_send![super(this), initWithFileType: &*uti, delegate: delegate] }
        }
    }

    fn file_url_type() -> String {
        // SAFETY: a static AppKit string.
        unsafe { NSPasteboardTypeFileURL }.to_string()
    }

    /// `--end hold`: keeps the button down while writing the payload into the
    /// placeholder over `--wait-secs`, with a +-1 pt dragged jiggle every 500 ms.
    fn hold_and_write(p: &Poster, t: (f64, f64)) {
        let c = cfg();
        let data = match std::fs::read(&c.payload.path) {
            Ok(d) => d,
            Err(e) => {
                log!("HOLD: reading the payload failed: {e}");
                return;
            }
        };
        let ticks = (c.wait_secs * 2).max(1) as usize;
        let chunk = data.len().div_ceil(ticks).max(1);
        let dest = &c.payload.placeholder;
        let tmp = dest.with_file_name(format!(".{}.part", c.payload.name));
        let target = if c.hold_write == "rename" { &tmp } else { dest };
        log!(
            "HOLD: button stays down; writing {} bytes into {} over {} s ({}), jiggle every 500 ms",
            data.len(),
            target.display(),
            c.wait_secs,
            c.hold_write
        );
        let mut file = match File::create(target) {
            Ok(f) => f,
            Err(e) => {
                log!("HOLD: creating {} failed: {e}", target.display());
                return;
            }
        };
        let mut jiggles = 0u32;
        let mut up = true;
        for part in data.chunks(chunk) {
            if let Err(e) = file.write_all(part) {
                log!("HOLD: write failed: {e}");
                return;
            }
            p.mouse(
                CGEventType::LeftMouseDragged,
                (t.0, t.1 + if up { 1.0 } else { 0.0 }),
                (0, if up { 1 } else { -1 }),
            );
            up = !up;
            jiggles += 1;
            if jiggles.is_multiple_of(10) {
                log!(
                    "HOLD: {jiggles} jiggles; session ended {}",
                    SESSION_ENDED.load(Ordering::SeqCst)
                );
            }
            sleep_ms(500);
        }
        if let Err(e) = file.sync_all() {
            log!("HOLD: sync failed: {e}");
        }
        drop(file);
        if c.hold_write == "rename"
            && let Err(e) = std::fs::rename(&tmp, dest)
        {
            log!("HOLD: renaming over the placeholder failed: {e}");
        }
        p.mouse(CGEventType::LeftMouseDragged, t, (0, 0));
        log!(
            "HOLD: payload complete in the placeholder ({}) after {jiggles} jiggles; session ended {}",
            size_of(dest),
            SESSION_ENDED.load(Ordering::SeqCst)
        );
    }

    fn source_summary() {
        let c = cfg();
        log!("---- summary (source, kind {}, end {}) ----", c.kind, c.end);
        log!(
            "mouseDown: reached the panel: {}",
            MOUSE_DOWN_SEEN.load(Ordering::SeqCst)
        );
        log!(
            "mouseDragged: reached the panel view {} times ({} after the session started)",
            VIEW_DRAGGED.load(Ordering::SeqCst),
            VIEW_DRAGGED_AFTER.load(Ordering::SeqCst)
        );
        log!(
            "session started from: {}",
            STARTED_HOW.lock().map(|h| h.clone()).unwrap_or_default()
        );
        log!(
            "session began: {}, ended: {}, operation: {}",
            SESSION_BEGAN.load(Ordering::SeqCst),
            SESSION_ENDED.load(Ordering::SeqCst),
            match ENDED_OP.load(Ordering::SeqCst) {
                NOT_YET => "-".to_string(),
                op => op_name(NSDragOperation(op as usize)),
            }
        );
        log!(
            "mask calls {}, moved calls {}",
            MASK_CALLS.load(Ordering::SeqCst),
            MOVED_CALLS.load(Ordering::SeqCst)
        );
        if c.kind != "url" {
            log!(
                "promise: fileNameForType calls {}, writePromiseToURL calls {}",
                PROMISE_NAME_CALLS.load(Ordering::SeqCst),
                PROMISE_WRITE_CALLS.load(Ordering::SeqCst)
            );
        }
        if c.kind == "both" {
            let (url_reads, writes) = (
                BOTH_URL_CALLS.load(Ordering::SeqCst),
                PROMISE_WRITE_CALLS.load(Ordering::SeqCst),
            );
            log!(
                "both: public.file-url property-list calls {url_reads} (asked when the drag starts, so they do not show who used it)"
            );
            let ended_ok =
                matches!(ENDED_OP.load(Ordering::SeqCst), op if op != NOT_YET && op != 0);
            let verdict = if writes > 0 {
                "the target used the PROMISE (writePromiseToURL was called)"
            } else if ended_ok {
                "the target took the drop WITHOUT asking for the promise: it used the file URL (placeholder)"
            } else {
                "no drop (operation None or no end)"
            };
            log!("=> both: {verdict}");
            log!(
                "placeholder now: {} ({})",
                c.payload.placeholder.display(),
                size_of(&c.payload.placeholder)
            );
        }
        if c.kind == "url" {
            let (b, a) = (
                PROVIDER_BEFORE.load(Ordering::SeqCst),
                PROVIDER_AFTER.load(Ordering::SeqCst),
            );
            log!("url provider calls: {b} before release, {a} after release");
            if b > 0 && a == 0 {
                log!(
                    "=> the provider was NOT asked again after the release: the pasteboard served \
                     its cached hover-time answer (placeholder-then-block will not work as is)"
                );
            } else if a > 0 {
                log!("=> the provider WAS asked after the release");
            }
            log!(
                "placeholder now: {} ({})",
                c.payload.placeholder.display(),
                size_of(&c.payload.placeholder)
            );
        }
    }

    fn source_script(s: (f64, f64), t: (f64, f64)) {
        let c = cfg();
        sleep_ms(500); // let the panel reach the window server
        // CJ_NOPOST=1: post nothing; another process drives the drag (F1:
        // does a session follow events posted by a different process?).
        if std::env::var_os("CJ_NOPOST").is_some() {
            log!(
                "CJ_NOPOST: panel at S ({:.1}, {:.1}); waiting for another process's events",
                s.0,
                s.1
            );
            for i in 0..40 {
                sleep_ms(250);
                if i % 4 == 0 {
                    log!(
                        "nopost: mouseDown seen {}, began {}, moved {}",
                        MOUSE_DOWN_SEEN.load(Ordering::SeqCst),
                        SESSION_BEGAN.load(Ordering::SeqCst),
                        MOVED_CALLS.load(Ordering::SeqCst)
                    );
                }
            }
            finish(c.quit_after, source_summary);
        }
        let Some(p) = Poster::new() else {
            log!("CGEventSourceCreate(HIDSystemState) failed; nothing posted");
            finish(c.quit_after, source_summary);
        };
        log!("warp to S ({:.1}, {:.1}) + moved event", s.0, s.1);
        p.warp(s);
        sleep_ms(50);
        log!("post LeftMouseDown at S");
        p.mouse(CGEventType::LeftMouseDown, s, (0, 0));
        sleep_ms(50);
        log!(
            "after 50 ms: mouseDown seen {}, session began {}",
            MOUSE_DOWN_SEEN.load(Ordering::SeqCst),
            SESSION_BEGAN.load(Ordering::SeqCst)
        );
        p.drag(s, t, c.move_ms);
        log!(
            "at T: session began {}, moved calls {}, mask calls {}; hovering {} ms{}",
            SESSION_BEGAN.load(Ordering::SeqCst),
            MOVED_CALLS.load(Ordering::SeqCst),
            MASK_CALLS.load(Ordering::SeqCst),
            c.hover_ms,
            if c.jiggle { " with jiggle" } else { "" }
        );
        if c.jiggle {
            let end = Instant::now() + Duration::from_millis(c.hover_ms);
            let mut up = true;
            while Instant::now() < end {
                let dy = if up { 1.0 } else { 0.0 };
                p.mouse(
                    CGEventType::LeftMouseDragged,
                    (t.0, t.1 + dy),
                    (0, if up { 1 } else { -1 }),
                );
                up = !up;
                sleep_ms(50);
            }
            p.mouse(CGEventType::LeftMouseDragged, t, (0, 0));
        } else {
            sleep_ms(c.hover_ms);
        }
        let release = |what: &str| {
            log!("RELEASE ({what}): LeftMouseUp at T");
            RELEASE_AT.store(now_us(), Ordering::SeqCst);
            p.mouse(CGEventType::LeftMouseUp, t, (0, 0));
        };
        let esc = || {
            log!("ESC: posting Escape down/up (keycode 0x35)");
            ESC_AT.store(now_us(), Ordering::SeqCst);
            p.key(KC_ESCAPE, true);
            p.key(KC_ESCAPE, false);
        };
        match c.end.as_str() {
            "esc" => {
                esc();
                sleep_ms(1000);
                log!(
                    "1 s after Esc: session ended {}; the left button is still DOWN (posted); the \
                     spike posts the up at quit",
                    SESSION_ENDED.load(Ordering::SeqCst)
                );
            }
            "esc-then-up" => {
                esc();
                sleep_ms(300);
                log!(
                    "300 ms after Esc: session ended {}",
                    SESSION_ENDED.load(Ordering::SeqCst)
                );
                release("esc-then-up");
            }
            "mask-none" => {
                log!("CANCEL: the source mask answers None from now on; two dragged events");
                CANCEL.store(true, Ordering::SeqCst);
                p.mouse(CGEventType::LeftMouseDragged, (t.0 + 1.0, t.1), (1, 0));
                sleep_ms(STEP_MS);
                p.mouse(CGEventType::LeftMouseDragged, t, (-1, 0));
                sleep_ms(50);
                release("mask-none");
            }
            "hold" => {
                hold_and_write(&p, t);
                release("hold");
            }
            _ => release("up"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !SESSION_ENDED.load(Ordering::SeqCst) && Instant::now() < deadline {
            sleep_ms(20);
        }
        if !SESSION_ENDED.load(Ordering::SeqCst) {
            log!("the session has not ended 5 s after the end step");
        }
        finish(c.quit_after, source_summary);
    }

    fn run_source(args: &Args) {
        let kind = args.choice("--kind", &["promise", "url", "both"]);
        let size_mb: u64 = args.num("--size-mb", 5);
        let payload = prepare_payload(args.vals.get("--from"), size_mb);
        let cfg = SourceCfg {
            kind,
            wait_secs: args.num("--wait-secs", 60),
            wait_when: args.choice("--wait-when", &["drop", "first"]),
            end: args.choice("--end", &["up", "esc", "esc-then-up", "mask-none", "hold"]),
            hold_write: args.choice("--hold-write", &["rename", "inplace"]),
            start_mode: args.choice(
                "--start-mode",
                &["down", "late-real", "late-synth", "late-timer"],
            ),
            late_ms: args.num("--late-ms", 240),
            hover_ms: args.num("--hover-ms", 1500),
            jiggle: args.flag("--jiggle"),
            move_ms: args.num("--move-ms", 800),
            quit_after: args.num("--quit-after", 20.0),
            payload,
        };
        let start = args.choice("--start", &["left-edge", "here"]);
        let alpha: f64 = args.num("--panel-alpha", 0.01);
        log!(
            "source: kind {}, wait {} s ({}), end {}, start {start}, hover {} ms, move {} ms, panel alpha {alpha}",
            cfg.kind,
            cfg.wait_secs,
            cfg.wait_when,
            cfg.end,
            cfg.hover_ms,
            cfg.move_ms
        );
        log!("start mode {}, late {} ms", cfg.start_mode, cfg.late_ms);
        let _ = SOURCE.set(cfg);

        let mtm = MainThreadMarker::new().expect("main thread");
        let app = new_app(mtm);
        log_frontmost("at start");
        let t = pick_point(
            args.num("--countdown", 5),
            "point at the drop target T, then leave the mouse alone",
        );
        log_frontmost("after the countdown");
        // `here` starts 12 pt beside T so the 8x8 panel is not under the drop point.
        let left = main_bounds().0;
        let s = match start.as_str() {
            "here" if t.0 - 12.0 >= left + 2.0 => (t.0 - 12.0, t.1),
            "here" => (t.0 + 12.0, t.1),
            _ => (left + 2.0, t.1),
        };
        log!("T = ({:.1}, {:.1}), S = ({:.1}, {:.1})", t.0, t.1, s.0, s.1);

        let view = DragView::new(
            mtm,
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0)),
        );
        let c = cg_to_cocoa(s);
        let frame = NSRect::new(NSPoint::new(c.x - 4.0, c.y - 4.0), NSSize::new(8.0, 8.0));
        let _panel = make_panel(mtm, frame, alpha, false, &view);

        std::thread::Builder::new()
            .name("bg".into())
            .spawn(move || source_script(s, t))
            .expect("spawn bg");
        app.run();
    }

    // ---------------------------------------------------------------- catcher

    struct CatcherIvars {
        reject: bool,
        queue: Retained<NSOperationQueue>,
        entered: Cell<u32>,
    }

    define_class!(
        // SAFETY: NSView has no subclassing requirements; no Drop.
        #[unsafe(super(NSView, NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "CJSpikeCatcherView"]
        #[ivars = CatcherIvars]
        struct CatcherView;

        unsafe impl NSObjectProtocol for CatcherView {}

        unsafe impl NSDraggingDestination for CatcherView {
            #[unsafe(method(draggingEntered:))]
            fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
                let n = self.ivars().entered.get() + 1;
                self.ivars().entered.set(n);
                UPDATED_CALLS.store(0, Ordering::SeqCst);
                log!("draggingEntered #{n} ({})", rel());
                dump_drag(sender, "entered");
                self.answer()
            }

            #[unsafe(method(draggingUpdated:))]
            fn updated(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
                let n = UPDATED_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 || n.is_multiple_of(25) {
                    log!("draggingUpdated #{n} ({})", rel());
                }
                self.answer()
            }

            #[unsafe(method(draggingExited:))]
            fn exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
                log!(
                    "draggingExited after {} updates ({})",
                    UPDATED_CALLS.load(Ordering::SeqCst),
                    rel()
                );
            }

            #[unsafe(method(prepareForDragOperation:))]
            fn prepare(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
                log!("prepareForDragOperation -> YES ({})", rel());
                true
            }

            #[unsafe(method(performDragOperation:))]
            fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
                PERFORMED.store(true, Ordering::SeqCst);
                log!("PERFORM performDragOperation ({})", rel());
                dump_drag(sender, "perform");
                self.receive_promises(sender);
                true
            }

            #[unsafe(method(concludeDragOperation:))]
            fn conclude(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
                log!("concludeDragOperation ({})", rel());
            }

            #[unsafe(method(draggingEnded:))]
            fn ended(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) {
                log!("draggingEnded ({})", rel());
            }
        }
    );

    impl CatcherView {
        fn new(mtm: MainThreadMarker, frame: NSRect, reject: bool) -> Retained<CatcherView> {
            let queue = NSOperationQueue::new();
            queue.setName(Some(ns_string!("cj-spike-receive")));
            let this = CatcherView::alloc(mtm).set_ivars(CatcherIvars {
                reject,
                queue,
                entered: Cell::new(0),
            });
            // SAFETY: NSView's designated initializer.
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        fn answer(&self) -> NSDragOperation {
            if self.ivars().reject {
                NSDragOperation::None
            } else {
                NSDragOperation::Copy
            }
        }

        fn receive_promises(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) {
            let pb = sender.draggingPasteboard();
            for recv in read_promises(&pb) {
                let n = RECV_DIRS.fetch_add(1, Ordering::SeqCst) + 1;
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let dir = spike_dir().join(format!("recv-{secs}-{n}"));
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    log!("creating {} failed: {e}", dir.display());
                    continue;
                }
                let files = file_names(&recv).map_or(0, |n| n.count()).max(1);
                IN_FLIGHT.fetch_add(files, Ordering::SeqCst);
                let left = AtomicUsize::new(files);
                let t0 = now_us();
                log!(
                    "receiving promise #{n} ({files} file(s)) into {}",
                    dir.display()
                );
                let reader = RcBlock::new(move |url: NonNull<NSURL>, err: *mut NSError| {
                    // SAFETY: AppKit passes a valid URL, and a valid error or null.
                    let (url, err) = unsafe { (url.as_ref(), err.as_ref()) };
                    let path = url.path().map(|p| p.to_string()).unwrap_or_default();
                    let ms = now_us().saturating_sub(t0) as f64 / 1000.0;
                    match err {
                        Some(e) => log!(
                            "promise reader #{n}: {path} ERROR {} after {ms:.1} ms ({})",
                            e.localizedDescription(),
                            rel()
                        ),
                        None => log!(
                            "promise reader #{n}: {path} ({}) after {ms:.1} ms ({})",
                            size_of(Path::new(&path)),
                            rel()
                        ),
                    }
                    // Readers for one receiver run one at a time on our queue.
                    if left.load(Ordering::SeqCst) > 0 {
                        left.fetch_sub(1, Ordering::SeqCst);
                        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                    }
                });
                let dest = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
                let options = NSDictionary::new();
                // SAFETY: an empty options dictionary, our own queue and a reader
                // block with the documented signature.
                unsafe {
                    recv.receivePromisedFilesAtDestination_options_operationQueue_reader(
                        &dest,
                        &options,
                        &self.ivars().queue,
                        &reader,
                    );
                }
            }
        }
    }

    fn read_promises(pb: &NSPasteboard) -> Vec<Retained<NSFilePromiseReceiver>> {
        let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
        // SAFETY: the class array holds a class that implements NSPasteboardReading.
        let objs = unsafe { pb.readObjectsForClasses_options(&classes, None) };
        objs.map(|a| {
            a.iter()
                .filter_map(|o| o.downcast::<NSFilePromiseReceiver>().ok())
                .collect()
        })
        .unwrap_or_default()
    }

    fn read_file_urls(pb: &NSPasteboard) -> Vec<String> {
        let classes = NSArray::from_slice(&[NSURL::class() as &AnyClass]);
        let yes = NSNumber::new_bool(true);
        let value: &AnyObject = &yes;
        // SAFETY: a static AppKit string.
        let key = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
        let options = NSDictionary::from_slices(&[key], &[value]);
        // SAFETY: NSURL implements NSPasteboardReading; the option takes an NSNumber.
        let objs = unsafe { pb.readObjectsForClasses_options(&classes, Some(&options)) };
        objs.map(|a| {
            a.iter()
                .filter_map(|o| o.downcast::<NSURL>().ok())
                .filter_map(|u| u.path().map(|p| p.to_string()))
                .collect()
        })
        .unwrap_or_default()
    }

    fn join(a: &NSArray<NSString>) -> String {
        a.iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn dump_drag(sender: &ProtocolObject<dyn NSDraggingInfo>, when: &str) {
        let mask = sender.draggingSourceOperationMask();
        let loc = sender.draggingLocation();
        log!(
            "[{when}] source mask {}, location (Cocoa {:.1}, {:.1}), sequence {}",
            op_name(mask),
            loc.x,
            loc.y,
            sender.draggingSequenceNumber()
        );
        let pb = sender.draggingPasteboard();
        let items = pb.pasteboardItems().map(|a| a.to_vec()).unwrap_or_default();
        log!("[{when}] {} pasteboard item(s)", items.len());
        for (i, item) in items.iter().enumerate() {
            log!("[{when}]   item {i}: {}", join(&item.types()));
        }
        for path in read_file_urls(&pb) {
            log!("[{when}] file URL {path} ({})", size_of(Path::new(&path)));
        }
        for (i, recv) in read_promises(&pb).iter().enumerate() {
            let names = file_names(recv);
            log!(
                "[{when}] file promise {i}: types [{}], names [{}] (names known: {})",
                join(&recv.fileTypes()),
                names.as_deref().map(join).unwrap_or_else(|| "nil".into()),
                names.is_some_and(|n| n.count() > 0)
            );
        }
    }

    /// `fileNames`, which Chrome leaves nil before the drop (the generated
    /// binding assumes non-null and panics).
    fn file_names(recv: &NSFilePromiseReceiver) -> Option<Retained<NSArray<NSString>>> {
        // SAFETY: `fileNames` takes no arguments and returns an NSArray or nil.
        unsafe { msg_send![recv, fileNames] }
    }

    fn catcher_summary() {
        log!("---- summary (catcher) ----");
        log!(
            "performDragOperation ran: {}; promise receives started: {}",
            PERFORMED.load(Ordering::SeqCst),
            RECV_DIRS.load(Ordering::SeqCst)
        );
    }

    fn catcher_script(
        drive: &str,
        countdown_secs: u64,
        move_ms: u64,
        hover_ms: u64,
        quit_after: f64,
    ) {
        if drive == "manual" {
            log!(
                "manual: drag a file onto the strip at the main display's left edge with the trackpad"
            );
            finish(quit_after, catcher_summary);
        }
        let at = pick_point(
            countdown_secs,
            "point at the file or image to drag, then leave the mouse alone",
        );
        let Some(p) = Poster::new() else {
            log!("CGEventSourceCreate(HIDSystemState) failed; nothing posted");
            finish(quit_after, catcher_summary);
        };
        let strip = (main_bounds().0 + 1.0, at.1);
        log!("post LeftMouseDown at P ({:.1}, {:.1})", at.0, at.1);
        p.mouse(CGEventType::LeftMouseDown, at, (0, 0));
        // CJ_STILL_MS: how long to hold still after the press (default 200).
        let still = std::env::var("CJ_STILL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200);
        log!("holding still {still} ms after the press");
        sleep_ms(still);
        p.drag(at, strip, move_ms);
        log!("on the strip; hovering {hover_ms} ms");
        sleep_ms(hover_ms);
        log!(
            "RELEASE: LeftMouseUp on the strip at ({:.1}, {:.1})",
            strip.0,
            strip.1
        );
        RELEASE_AT.store(now_us(), Ordering::SeqCst);
        p.mouse(CGEventType::LeftMouseUp, strip, (0, 0));
        finish(quit_after, catcher_summary);
    }

    fn run_catcher(args: &Args) {
        let drive = args.choice("--drive", &["posted", "manual"]);
        let alpha: f64 = args.num("--alpha", 0.01);
        let reject = args.flag("--reject");
        let types_all = args.flag("--types-all");
        let hover_ms: u64 = args.num("--hover-ms", 800);
        let move_ms: u64 = args.num("--move-ms", 800);
        let countdown_secs: u64 = args.num("--countdown", 5);
        let quit_after: f64 =
            args.num("--quit-after", if drive == "manual" { 120.0 } else { 20.0 });
        log!(
            "catcher: drive {drive}, alpha {alpha}, reject {reject}, types-all {types_all}, hover {hover_ms} ms, move {move_ms} ms"
        );

        let mtm = MainThreadMarker::new().expect("main thread");
        let app = new_app(mtm);
        log_frontmost("at start");
        let h = main_bounds().3;
        // CJ_STRIP_W=n: a wider strip, to see when posted drags reach it.
        let w: f64 = std::env::var("CJ_STRIP_W")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2.0);
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let view = CatcherView::new(mtm, NSRect::new(NSPoint::new(0.0, 0.0), frame.size), reject);
        // SAFETY: a static AppKit string.
        let file_url = unsafe { NSPasteboardTypeFileURL }.to_string();
        let mut types: Vec<Retained<NSString>> = vec![NSString::from_str(&file_url)];
        types.extend(NSFilePromiseReceiver::readableDraggedTypes().to_vec());
        if types_all {
            for t in [
                "public.url",
                "public.image",
                "public.png",
                "public.jpeg",
                "public.tiff",
                "public.utf8-plain-text",
                "public.html",
            ] {
                types.push(NSString::from_str(t));
            }
        }
        log!(
            "registered dragged types: {}",
            types
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        view.registerForDraggedTypes(&NSArray::from_retained_slice(&types));
        let _panel = make_panel(mtm, frame, alpha, true, &view);

        std::thread::Builder::new()
            .name("bg".into())
            .spawn(move || catcher_script(&drive, countdown_secs, move_ms, hover_ms, quit_after))
            .expect("spawn bg");
        app.run();
    }

    // ---------------------------------------------------------------- self-check

    fn run_selfcheck() {
        let payload = prepare_payload(None, 1);
        let name = payload.name.clone();
        let size = std::fs::metadata(&payload.path).map_or(0, |m| m.len());
        let placeholder = payload.placeholder.clone();
        let _ = SOURCE.set(SourceCfg {
            kind: "promise".into(),
            hold_write: "rename".into(),
            start_mode: "down".into(),
            late_ms: 240,
            wait_secs: 0,
            wait_when: "drop".into(),
            end: "up".into(),
            hover_ms: 0,
            jiggle: false,
            move_ms: 0,
            quit_after: 0.0,
            payload,
        });
        let mtm = MainThreadMarker::new().expect("main thread");
        let _app = new_app(mtm);
        let mut ok = true;
        let mut check = |what: &str, pass: bool| {
            log!("{} {what}", if pass { "ok  " } else { "FAIL" });
            ok &= pass;
        };
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(8.0, 8.0));
        let drag_view = DragView::new(mtm, rect);
        let synth = drag_view.synth_dragged();
        check(
            &format!(
                "synthesized dragged event: {:?}",
                synth.as_deref().map(describe_event)
            ),
            synth.is_some_and(|e| {
                e.r#type() == NSEventType::LeftMouseDragged
                    && e.eventNumber() == 0
                    && e.clickCount() == 1
            }),
        );
        // SAFETY: DISPATCH_TIME_NOW is 0; the main queue is a static; the
        // callback ignores its context. The run loop drains the main queue.
        unsafe {
            dispatch_after_f(
                dispatch_time(0, 50_000_000),
                &raw const _dispatch_main_q,
                std::ptr::null_mut(),
                selfcheck_timer_fired,
            );
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, 0);
        }
        check(
            "dispatch_after on the main queue fired",
            SELFCHECK_TIMER.load(Ordering::SeqCst),
        );
        let _catcher_view = CatcherView::new(mtm, rect, false);
        check("DragView and CatcherView registered", true);

        let delegate = PromiseDelegate::new();
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &NSString::from_str(&cfg().payload.uti),
            ProtocolObject::from_ref(&*delegate),
        );
        // SAFETY: the delegate method's own selector and argument types.
        let got: Retained<NSString> = unsafe {
            msg_send![&*delegate, filePromiseProvider: &*provider, fileNameForType: ns_string!("public.data")]
        };
        check("promise fileNameForType", got.to_string() == name);
        let out = spike_dir().join("selfcheck");
        let _ = std::fs::create_dir_all(&out);
        let dest = out.join(&name);
        let _ = std::fs::remove_file(&dest);
        let url = NSURL::fileURLWithPath(&NSString::from_str(&dest.to_string_lossy()));
        let errored = std::sync::Arc::new(AtomicBool::new(true));
        let flag = errored.clone();
        let handler =
            RcBlock::new(move |err: *mut NSError| flag.store(!err.is_null(), Ordering::SeqCst));
        // SAFETY: as above; the handler has the documented block signature.
        let () = unsafe {
            msg_send![&*delegate, filePromiseProvider: &*provider, writePromiseToURL: &*url, completionHandler: &*handler]
        };
        let written = std::fs::metadata(&dest).map_or(0, |m| m.len());
        check(
            &format!("promise writer wrote {written}/{size} bytes, completion without error"),
            written == size && !errored.load(Ordering::SeqCst),
        );
        // SAFETY: as above.
        let _queue: Retained<NSOperationQueue> =
            unsafe { msg_send![&*delegate, operationQueueForFilePromiseProvider: &*provider] };
        check("promise operationQueue", true);

        let url_provider = UrlProvider::new();
        let pb_item = NSPasteboardItem::new();
        // SAFETY: a static AppKit string.
        let file_url = unsafe { NSPasteboardTypeFileURL };
        let set = pb_item.setDataProvider_forTypes(
            ProtocolObject::from_ref(&*url_provider),
            &NSArray::from_slice(&[file_url]),
        );
        check("setDataProvider(public.file-url)", set);
        // SAFETY: the provider method's own selector and argument types.
        let () = unsafe {
            msg_send![&*url_provider, pasteboard: None::<&NSPasteboard>, item: &*pb_item, provideDataForType: file_url]
        };
        let want = file_url_string(&placeholder).to_string();
        let have = pb_item.stringForType(file_url).map(|s| s.to_string());
        check(
            &format!("url provider answered {have:?}"),
            have.as_deref() == Some(want.as_str()),
        );

        let item = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*provider),
        );
        let ut = UTType::typeWithIdentifier(&NSString::from_str(&cfg().payload.uti));
        check(&format!("UTType for {}", cfg().payload.uti), ut.is_some());
        if let Some(ut) = ut {
            let icon = NSWorkspace::sharedWorkspace().iconForContentType(&ut);
            icon.setSize(NSSize::new(64.0, 64.0));
            let contents: &AnyObject = &icon;
            // SAFETY: an NSImage is a valid dragging-frame content.
            unsafe { item.setDraggingFrame_contents(rect, Some(contents)) };
            check("dragging item with icon", item.imageComponents().is_some());
        }
        let both = BothProvider::new(&cfg().payload.uti, &delegate);
        let pb = NSPasteboard::pasteboardWithUniqueName();
        // SAFETY: the NSPasteboardWriting methods' own selectors and argument types.
        let types: Retained<NSArray<NSString>> =
            unsafe { msg_send![&*both, writableTypesForPasteboard: &*pb] };
        let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        check(
            &format!("both: writable types {types:?}"),
            types.contains(&file_url_type()) && types.len() > 1,
        );
        let ft = NSString::from_str(&file_url_type());
        // SAFETY: as above.
        let opts: NSPasteboardWritingOptions =
            unsafe { msg_send![&*both, writingOptionsForType: &*ft, pasteboard: &*pb] };
        check("both: file-url written at once", opts.0 == 0);
        // SAFETY: as above.
        let plist: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*both, pasteboardPropertyListForType: &*ft] };
        let plist = plist
            .and_then(|o| o.downcast::<NSString>().ok())
            .map(|s| s.to_string());
        check(
            &format!("both: file-url answer {plist:?}"),
            plist.as_deref() == Some(want.as_str()),
        );
        let promise_types: Vec<String> = types
            .iter()
            .filter(|t| **t != file_url_type())
            .cloned()
            .collect();
        for t in promise_types {
            let nt = NSString::from_str(&t);
            // SAFETY: as above.
            let o: NSPasteboardWritingOptions =
                unsafe { msg_send![&*both, writingOptionsForType: &*nt, pasteboard: &*pb] };
            log!("both: super writingOptions({t}) = {:#x}", o.0);
        }
        // SAFETY: releases the unique pasteboard made above.
        let () = unsafe { msg_send![&*pb, releaseGlobally] };
        log!("selfcheck: {}", if ok { "PASS" } else { "FAIL" });
        std::process::exit(if ok { 0 } else { 1 });
    }

    // ---------------------------------------------------------------- main

    pub fn main() {
        START.get_or_init(Instant::now);
        let args = Args::parse();
        open_log(&args.mode);
        install_exit_guards();
        log_permissions();
        match args.mode.as_str() {
            "source" => run_source(&args),
            "selfcheck" => run_selfcheck(),
            _ => run_catcher(&args),
        }
    }
}
