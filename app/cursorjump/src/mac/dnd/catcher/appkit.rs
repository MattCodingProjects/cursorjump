//! The catcher's AppKit side, on the main thread: the strip panel, made
//! and drawn once and shown with order-front/order-out, and its view, the
//! drag destination. Promises are received on the view's own queue.

use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;

use block2::RcBlock;
use cj_core::dnd::DragId;
use cj_core::proto::Side;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSFilePromiseReceiver, NSPanel, NSPasteboard, NSPasteboardTypeFileURL, NSResponder,
    NSStatusWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{
    NSArray, NSCopying, NSDictionary, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString,
    NSURL, ns_string,
};
use tracing::{debug, info, warn};

use super::{Dragged, Facts, Promise, Reading, STRIP_ALPHA, Shared, strip_frame};
use crate::mac::dnd::{flush_core_animation, guarded};
use crate::mac::inject::Rectf;

/// A promised file of this type is a folder.
const FOLDER_UTT: &str = "public.folder";

/// The strip, made once; only ever touched on the main thread.
struct Strip {
    panel: Retained<StripPanel>,
    _view: Retained<StripView>,
    /// Its frame now (Cocoa coordinates), to skip unchanged moves.
    frame: Option<Rectf>,
}

thread_local! {
    static STRIP: RefCell<Option<Strip>> = const { RefCell::new(None) };
}

/// Makes the strip panel (hidden) unless it exists.
pub(super) fn make(mtm: MainThreadMarker, shared: &Arc<Shared>) {
    STRIP.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else {
            return;
        };
        if s.is_none() {
            *s = Some(new_strip(mtm, shared));
        }
    });
}

/// Shows the strip along `show`'s edge of the main display, drawn and
/// flushed so the window server can hit-test it at once; or hides it,
/// letting go of a drag still on it.
pub(super) fn zone(mtm: MainThreadMarker, shared: &Arc<Shared>, show: Option<Side>) {
    make(mtm, shared);
    STRIP.with(|s| {
        let Ok(mut s) = s.try_borrow_mut() else {
            return;
        };
        let Some(strip) = s.as_mut() else {
            return;
        };
        match show {
            Some(edge) => {
                let b = CGDisplayBounds(CGMainDisplayID());
                let main = Rectf {
                    x: b.origin.x,
                    y: b.origin.y,
                    w: b.size.width,
                    h: b.size.height,
                };
                let f = strip_frame(main, edge);
                if strip.frame != Some(f) {
                    let rect = NSRect::new(NSPoint::new(f.x, f.y), NSSize::new(f.w, f.h));
                    strip.panel.setFrame_display(rect, false);
                    strip.frame = Some(f);
                }
                strip.panel.orderFrontRegardless();
                strip.panel.display();
                flush_core_animation();
                #[expect(deprecated, reason = "the window must reach the window server now")]
                strip.panel.flushWindow();
                debug!(?edge, "the strip is up");
            }
            None => {
                strip.panel.orderOut(None);
                debug!("the strip is down");
            }
        }
    });
    if show.is_none() {
        // A drag still on it gets no exit: it is no longer on the strip.
        shared.left(None);
    }
}

fn new_strip(mtm: MainThreadMarker, shared: &Arc<Shared>) -> Strip {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(2.0, 2.0));
    let view = StripView::new(mtm, shared.clone(), frame);
    // SAFETY: a static AppKit string.
    let mut types = vec![unsafe { NSPasteboardTypeFileURL }.copy()];
    types.extend(NSFilePromiseReceiver::readableDraggedTypes().to_vec());
    view.registerForDraggedTypes(&NSArray::from_retained_slice(&types));
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    // SAFETY: NSPanel's designated initializer, on our subclass.
    let panel: Retained<StripPanel> = unsafe {
        msg_send![
            StripPanel::alloc(mtm),
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    // SAFETY: `Strip` owns the panel; closing must not release it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setLevel(NSStatusWindowLevel);
    panel.setHidesOnDeactivate(false);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.5, STRIP_ALPHA)));
    panel.setIgnoresMouseEvents(false);
    panel.setContentView(Some(&view));
    info!("the catcher's strip is ready");
    Strip {
        panel,
        _view: view,
        frame: None,
    }
}

define_class!(
    /// The strip's panel: it never becomes key or main, so showing it
    /// never takes focus from the user's app.
    // SAFETY: NSPanel may be subclassed; StripPanel has no ivars and no Drop.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJCatcherPanel"]
    struct StripPanel;

    impl StripPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }
    }
);

struct ViewIvars {
    shared: Arc<Shared>,
    /// Promise reads run here, never on the main thread.
    queue: Retained<NSOperationQueue>,
}

define_class!(
    /// The strip's view: the drag destination.
    // SAFETY: NSView has no subclassing requirements; StripView has no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJCatcherView"]
    #[ivars = ViewIvars]
    struct StripView;

    unsafe impl NSObjectProtocol for StripView {}

    unsafe impl NSDraggingDestination for StripView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            guarded("catcher", "draggingEntered:", NSDragOperation::None, || {
                self.entered(sender)
            })
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            guarded("catcher", "draggingUpdated:", NSDragOperation::None, || {
                if self.ivars().shared.updated(sender.draggingSequenceNumber()) {
                    NSDragOperation::Copy
                } else {
                    NSDragOperation::None
                }
            })
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            guarded("catcher", "draggingExited:", (), || {
                let seq = sender.map(|s| s.draggingSequenceNumber());
                self.ivars().shared.left(seq);
            });
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            guarded("catcher", "performDragOperation:", false, || {
                self.perform(sender)
            })
        }

        #[unsafe(method(draggingEnded:))]
        fn dragging_ended(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) {
            guarded("catcher", "draggingEnded:", (), || {
                // After a drop the strip's drag is gone already; this only
                // ever lets go of one that ended otherwise.
                self.ivars()
                    .shared
                    .left(Some(sender.draggingSequenceNumber()));
            });
        }
    }
);

impl StripView {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>, frame: NSRect) -> Retained<StripView> {
        let queue = NSOperationQueue::new();
        queue.setName(Some(ns_string!("cj-catcher-receive")));
        let this = StripView::alloc(mtm).set_ivars(ViewIvars { shared, queue });
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
        if sender.draggingSource().is_some() {
            // Our own drag (one from the PC, riding here): never caught.
            return NSDragOperation::None;
        }
        let seq = sender.draggingSequenceNumber();
        let dragged = read_dragged(&sender.draggingPasteboard());
        match self.ivars().shared.entered(seq, dragged) {
            Some(_) => NSDragOperation::Copy,
            None => NSDragOperation::None,
        }
    }

    /// Takes the drop if a `Leave` carries it, and receives its promises.
    fn perform(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        let iv = self.ivars();
        let Some((drag, promises)) = iv.shared.perform(sender.draggingSequenceNumber()) else {
            return false;
        };
        if !promises.is_empty() {
            let receivers = read_receivers(&sender.draggingPasteboard());
            for p in promises {
                match receivers.get(p.receiver) {
                    Some(recv) => self.receive(drag, recv, p),
                    None => {
                        // Its files' sources fail as `p` is dropped.
                        warn!(drag, receiver = p.receiver, "a promise is gone at the drop");
                    }
                }
            }
        }
        true
    }

    /// Receives promise `p` of `drag` into its own folder; each read
    /// resolves the next of its files, on our queue.
    fn receive(&self, drag: DragId, recv: &NSFilePromiseReceiver, p: Promise) {
        let iv = self.ivars();
        let first = p.files.first().map_or(0, |f| f.item);
        let dir = iv.shared.promise_dir(drag, first);
        let reading = Arc::new(Reading::new(drag, p.files));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            // Its files' sources fail as `reading` is dropped.
            warn!(drag, dir = %dir.display(), "the promise's folder can't be made: {e}");
            return;
        }
        let r = reading.clone();
        let reader = RcBlock::new(move |url: NonNull<NSURL>, err: *mut NSError| {
            guarded("catcher", "the promise reader", (), || {
                // SAFETY: AppKit passes a valid URL, and a valid error or null.
                let (url, err) = unsafe { (url.as_ref(), err.as_ref()) };
                let path = url.path().map(|p| PathBuf::from(p.to_string()));
                if let Some(e) = err {
                    warn!(
                        drag,
                        ?path,
                        "promise read failed: {}",
                        e.localizedDescription()
                    );
                }
                r.read(path.unwrap_or_default(), err.is_none());
            });
        });
        let dest =
            NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&dir.to_string_lossy()), true);
        info!(drag, dir = %dir.display(), "receiving a promise");
        // SAFETY: an empty options dictionary, our own queue and a reader
        // block with the documented signature.
        unsafe {
            recv.receivePromisedFilesAtDestination_options_operationQueue_reader(
                &dest,
                &NSDictionary::new(),
                &iv.queue,
                &reader,
            );
        }
    }
}

/// What each pasteboard item of the drag carries, in order: its file URL
/// (the real file, preferred), else its file promise.
fn read_dragged(pb: &NSPasteboard) -> Vec<Dragged> {
    // SAFETY: a static AppKit string.
    let file_url = unsafe { NSPasteboardTypeFileURL };
    let promise_types: Vec<String> = NSFilePromiseReceiver::readableDraggedTypes()
        .iter()
        .map(|t| t.to_string())
        .collect();
    let receivers = read_receivers(pb);
    let items = pb.pasteboardItems().map(|a| a.to_vec()).unwrap_or_default();
    let mut out = Vec::with_capacity(items.len());
    // The receivers are one per item with promise types, in item order.
    let mut next_receiver = 0;
    for item in &items {
        let types: Vec<String> = item.types().iter().map(|t| t.to_string()).collect();
        let receiver = types.iter().any(|t| promise_types.contains(t)).then(|| {
            next_receiver += 1;
            next_receiver - 1
        });
        if let Some(path) = item.stringForType(file_url).and_then(|s| file_path(&s)) {
            let facts = Facts::of(&path);
            out.push(Dragged::File { path, facts });
        } else if let Some(receiver) = receiver {
            let recv = receivers.get(receiver);
            let names = recv.and_then(|r| file_names(r));
            let dirs = recv
                .and_then(|r| file_types(r))
                .map_or_else(Vec::new, |t| t.iter().map(|t| t == FOLDER_UTT).collect());
            out.push(Dragged::Promise {
                receiver,
                names,
                dirs,
            });
        }
    }
    if next_receiver != receivers.len() {
        warn!(
            items = next_receiver,
            receivers = receivers.len(),
            "promise items and receivers don't match"
        );
    }
    debug!(?out, "what the drag carries");
    out
}

/// A `public.file-url` value as a path (file reference URLs resolved).
fn file_path(url: &NSString) -> Option<PathBuf> {
    let url = NSURL::URLWithString(url)?;
    let path = url.filePathURL()?.path()?;
    Some(PathBuf::from(path.to_string()))
}

/// The pasteboard's file promises, one per item that has one.
fn read_receivers(pb: &NSPasteboard) -> Vec<Retained<NSFilePromiseReceiver>> {
    let classes = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
    // SAFETY: NSFilePromiseReceiver implements NSPasteboardReading.
    let objs = unsafe { pb.readObjectsForClasses_options(&classes, None) };
    objs.map(|a| {
        a.iter()
            .filter_map(|o| o.downcast::<NSFilePromiseReceiver>().ok())
            .collect()
    })
    .unwrap_or_default()
}

/// `fileTypes`, read like `fileNames`: the generated binding assumes
/// non-null.
fn file_types(recv: &NSFilePromiseReceiver) -> Option<Vec<String>> {
    // SAFETY: `fileTypes` takes no arguments and returns an NSArray or nil.
    let types: Option<Retained<NSArray<NSString>>> = unsafe { msg_send![recv, fileTypes] };
    types.map(|t| t.iter().map(|s| s.to_string()).collect())
}

/// `fileNames`, which Chrome leaves nil before the drop (the generated
/// binding assumes non-null and would panic).
fn file_names(recv: &NSFilePromiseReceiver) -> Option<Vec<String>> {
    // SAFETY: `fileNames` takes no arguments and returns an NSArray or nil.
    let names: Option<Retained<NSArray<NSString>>> = unsafe { msg_send![recv, fileNames] };
    names.map(|n| n.iter().map(|s| s.to_string()).collect())
}
