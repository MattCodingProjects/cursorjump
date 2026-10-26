//! The drag source's AppKit side, on the main thread: the drag panel and
//! its view (the posted down starts the session there), each item's file
//! promise that also offers the placeholder's URL, and the promise's
//! delegate, whose writes run on its own queue.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2::{define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem,
    NSDraggingSession, NSDraggingSource, NSEvent, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSPanel, NSPasteboard, NSPasteboardTypeFileURL,
    NSPasteboardWritingOptions, NSResponder, NSStatusWindowLevel, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{
    NSArray, NSCopying, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL,
    ns_string,
};
use objc2_uniform_type_identifiers::UTType;
use tracing::{info, warn};

use super::{
    Drag, FOLDER_UTT, ICON_CASCADE, ICON_SIZE, Inner, KEEP_AFTER_CLOSE, MAX_CASCADE, MacDndEv,
    PANEL_ALPHA, PANEL_SIZE, Slot, utt_for_name, write_promise,
};

/// A drag's panel and the objects its session uses, kept on the main
/// thread until well after the session ended (a promise's delegate is
/// held weakly by AppKit).
struct Kept {
    drag: Arc<Drag>,
    panel: Retained<NSPanel>,
    _view: Retained<DragView>,
    closed_at: Option<Instant>,
}

thread_local! {
    /// Only ever touched on the main thread.
    static KEPT: RefCell<Vec<Kept>> = const { RefCell::new(Vec::new()) };
}

/// Hides `drag`'s panel (this crossing's: a drag that came back has a
/// panel per crossing); its objects stay a while for late promise writes.
pub(super) fn close_panel(drag: &Arc<Drag>) {
    KEPT.with(|k| {
        let Ok(mut kept) = k.try_borrow_mut() else {
            return;
        };
        for e in kept
            .iter_mut()
            .filter(|e| Arc::ptr_eq(&e.drag, drag) && e.closed_at.is_none())
        {
            e.panel.orderOut(None);
            e.closed_at = Some(Instant::now());
        }
    });
}

/// Lets go of closed drags' objects once no promise write can need them.
fn prune() {
    let gone: Vec<Kept> = KEPT.with(|k| {
        let Ok(mut kept) = k.try_borrow_mut() else {
            return Vec::new();
        };
        let (gone, keep) = std::mem::take(&mut *kept).into_iter().partition(|e| {
            e.closed_at.is_some_and(|t| t.elapsed() >= KEEP_AFTER_CLOSE)
                && e.drag.writes.load(Ordering::SeqCst) == 0
        });
        *kept = keep;
        gone
    });
    drop(gone); // Outside the borrow.
}

/// Main thread: the panel under `at` with the drag's items armed, drawn
/// and flushed to the window server. Its window number, for `prepare` to
/// see it on screen before pressing; None if `prepare` gave up already.
pub(super) fn arm(
    mtm: MainThreadMarker,
    inner: &Arc<Inner>,
    drag: &Arc<Drag>,
    at: (f64, f64),
) -> Option<isize> {
    if drag.lock().cancelled {
        return None; // `prepare` gave up waiting.
    }
    prune();
    let armed = drag
        .slots
        .iter()
        .enumerate()
        .map(|(i, slot)| Armed::new(drag, i, slot))
        .collect();
    let view = DragView::new(
        mtm,
        ViewIvars {
            inner: inner.clone(),
            drag: drag.clone(),
            armed,
        },
    );
    let panel = drag_panel(mtm, at, &view);
    let number = panel.windowNumber();
    KEPT.with(|k| {
        k.borrow_mut().push(Kept {
            drag: drag.clone(),
            panel,
            _view: view,
            closed_at: None,
        });
    });
    Some(number)
}

/// An 8×8 pt borderless, non-activating, nearly invisible panel centred
/// on `at` (global points), at the status level on every Space.
fn drag_panel(mtm: MainThreadMarker, at: (f64, f64), view: &NSView) -> Retained<NSPanel> {
    let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
    let half = PANEL_SIZE / 2.0;
    let frame = NSRect::new(
        NSPoint::new(at.0 - half, main_h - at.1 - half),
        NSSize::new(PANEL_SIZE, PANEL_SIZE),
    );
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: `Kept` owns the panel; closing must not release it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setLevel(NSStatusWindowLevel);
    panel.setHidesOnDeactivate(false);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setOpaque(false);
    panel.setHasShadow(false);
    panel.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.5, PANEL_ALPHA)));
    panel.setIgnoresMouseEvents(false);
    panel.setContentView(Some(view));
    panel.orderFrontRegardless();
    // The window server hit-tests per pixel: until the panel has a drawn
    // backing it is transparent there, and a click falls through to the
    // window behind. Draw it now and push it out.
    panel.display();
    super::super::flush_core_animation();
    #[expect(deprecated, reason = "the window must reach the window server now")]
    panel.flushWindow();
    panel
}

/// Run `f`, catching and logging a panic: for code AppKit calls.
fn guarded(what: &str, f: impl FnOnce()) {
    super::super::guarded("drag source", what, (), f);
}

/// One item's pasteboard writer and its delegate.
struct Armed {
    provider: Retained<PromiseProvider>,
    _delegate: Retained<PromiseDelegate>,
    uti: String,
}

impl Armed {
    fn new(drag: &Arc<Drag>, item: usize, slot: &Slot) -> Armed {
        // Looked up here, on the main thread, not on cj-inject.
        let uti = if slot.is_dir {
            FOLDER_UTT.into()
        } else {
            utt_for_name(&slot.name)
        };
        let delegate = PromiseDelegate::new(drag.clone(), item);
        let provider = PromiseProvider::new(slot, &uti, &delegate);
        Armed {
            provider,
            _delegate: delegate,
            uti,
        }
    }
}

struct ViewIvars {
    inner: Arc<Inner>,
    drag: Arc<Drag>,
    armed: Vec<Armed>,
}

define_class!(
    /// The drag panel's view: the posted down starts the session here.
    // SAFETY: NSView has no subclassing requirements; DragView has no Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJIncomingDragView"]
    #[ivars = ViewIvars]
    struct DragView;

    unsafe impl NSObjectProtocol for DragView {}

    impl DragView {
        /// The down reaches us though CursorJump is never the active app.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            guarded("mouseDown:", || self.start(event));
        }
    }

    unsafe impl NSDraggingSource for DragView {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            // A cancelled drag drops nowhere, even if an up beats its Esc.
            if self.ivars().drag.lock().cancelled {
                NSDragOperation::None
            } else {
                NSDragOperation::Copy
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _at: NSPoint, op: NSDragOperation) {
            guarded("endedAtPoint:", || {
                let iv = self.ivars();
                iv.inner.end(&iv.drag, op != NSDragOperation::None);
                close_panel(&iv.drag);
            });
        }
    }
);

impl DragView {
    fn new(mtm: MainThreadMarker, ivars: ViewIvars) -> Retained<DragView> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANEL_SIZE, PANEL_SIZE));
        let this = DragView::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Starts the session with one dragging item per item, each with its
    /// type's icon, fanned out a little.
    fn start(&self, event: &NSEvent) {
        let iv = self.ivars();
        // Starting and cancelling exclude each other: once `began` is set,
        // `cancel` and `released` know a session is (being) made.
        if !iv.drag.try_begin() {
            return; // Cancelled, or not the first down.
        }
        let at = self.convertPoint_fromView(event.locationInWindow(), None);
        let items: Vec<Retained<NSDraggingItem>> = iv
            .armed
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let writer: &NSFilePromiseProvider = &a.provider;
                let item = NSDraggingItem::initWithPasteboardWriter(
                    NSDraggingItem::alloc(),
                    ProtocolObject::from_ref(writer),
                );
                let k = i.min(MAX_CASCADE) as f64 * ICON_CASCADE;
                let half = ICON_SIZE / 2.0;
                let frame = NSRect::new(
                    NSPoint::new(at.x - half + k, at.y - half - k),
                    NSSize::new(ICON_SIZE, ICON_SIZE),
                );
                let ut = UTType::typeWithIdentifier(&NSString::from_str(&a.uti))
                    .or_else(|| UTType::typeWithIdentifier(ns_string!("public.data")));
                match ut {
                    Some(ut) => {
                        let icon = NSWorkspace::sharedWorkspace().iconForContentType(&ut);
                        icon.setSize(NSSize::new(ICON_SIZE, ICON_SIZE));
                        let contents: &AnyObject = &icon;
                        // SAFETY: an NSImage is a valid dragging-frame content.
                        unsafe { item.setDraggingFrame_contents(frame, Some(contents)) };
                    }
                    None => item.setDraggingFrame(frame),
                }
                item
            })
            .collect();
        let items = NSArray::from_retained_slice(&items);
        let session = self.beginDraggingSessionWithItems_event_source(
            &items,
            event,
            ProtocolObject::from_ref(self),
        );
        session.setAnimatesToStartingPositionsOnCancelOrFail(false);
        if iv.drag.session_started() {
            // Cancelled while it started: cj-inject's Esc may have come
            // before the session could take it.
            iv.inner.emit(MacDndEv::NeedsEscape {
                drag: iv.drag.id,
                crossing: iv.drag.crossing,
            });
        }
        info!(drag = iv.drag.id, "the drag session from the PC began");
    }
}

/// A file promise that also offers `public.file-url`: the placeholder.
struct ProviderIvars {
    /// The placeholder's file URL, as a string.
    url: String,
}

define_class!(
    // SAFETY: NSFilePromiseProvider may be subclassed (AppKit documents
    // overriding its NSPasteboardWriting methods to add types); no Drop.
    #[unsafe(super(NSFilePromiseProvider, NSObject))]
    #[name = "CJIncomingPromiseProvider"]
    #[ivars = ProviderIvars]
    struct PromiseProvider;

    unsafe impl NSObjectProtocol for PromiseProvider {}

    impl PromiseProvider {
        #[unsafe(method_id(writableTypesForPasteboard:))]
        fn writable_types(&self, pasteboard: &NSPasteboard) -> Retained<NSArray<NSString>> {
            // SAFETY: the superclass's own method with the same arguments.
            let base: Retained<NSArray<NSString>> =
                unsafe { msg_send![super(self), writableTypesForPasteboard: pasteboard] };
            let mut types = base.to_vec();
            types.push(file_url_type());
            NSArray::from_retained_slice(&types)
        }

        #[unsafe(method(writingOptionsForType:pasteboard:))]
        fn writing_options(
            &self,
            ty: &NSString,
            pasteboard: &NSPasteboard,
        ) -> NSPasteboardWritingOptions {
            self.options_for(ty, pasteboard)
        }

        #[unsafe(method_id(pasteboardPropertyListForType:))]
        fn property_list(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
            self.value_for(ty)
        }
    }
);

impl PromiseProvider {
    /// The file URL is written at once; the promise's own types as the
    /// superclass says.
    fn options_for(&self, ty: &NSString, pasteboard: &NSPasteboard) -> NSPasteboardWritingOptions {
        if *ty == *file_url_type() {
            return NSPasteboardWritingOptions(0);
        }
        // SAFETY: the superclass's own method with the same arguments.
        unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pasteboard] }
    }

    /// The placeholder's URL for `public.file-url`; the promise's own
    /// types from the superclass.
    fn value_for(&self, ty: &NSString) -> Option<Retained<AnyObject>> {
        if *ty == *file_url_type() {
            let url = NSString::from_str(&self.ivars().url);
            return Some(url.into_super().into_super());
        }
        // SAFETY: the superclass's own method with the same argument.
        unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
    }

    fn new(slot: &Slot, uti: &str, delegate: &PromiseDelegate) -> Retained<PromiseProvider> {
        let path = NSString::from_str(&slot.placeholder.to_string_lossy());
        let url = NSURL::fileURLWithPath_isDirectory(&path, slot.is_dir)
            .absoluteString()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let this = PromiseProvider::alloc().set_ivars(ProviderIvars { url });
        let uti = NSString::from_str(uti);
        let delegate = ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(delegate);
        // SAFETY: NSFilePromiseProvider's designated initializer.
        unsafe { msg_send![super(this), initWithFileType: &*uti, delegate: delegate] }
    }
}

fn file_url_type() -> Retained<NSString> {
    // SAFETY: a static AppKit string.
    unsafe { NSPasteboardTypeFileURL }.copy()
}

struct DelegateIvars {
    drag: Arc<Drag>,
    item: usize,
    /// The promise's writes run here, never on the main thread.
    queue: Retained<NSOperationQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop.
    #[unsafe(super(NSObject))]
    #[name = "CJIncomingPromiseDelegate"]
    #[ivars = DelegateIvars]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            let iv = self.ivars();
            let name = iv.drag.slots.get(iv.item).map_or("_", |s| s.name.as_str());
            NSString::from_str(name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_to(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let iv = self.ivars();
            let (drag, item) = (iv.drag.id, iv.item);
            let dest = url.path().map(|p| PathBuf::from(p.to_string()));
            let result = catch_unwind(AssertUnwindSafe(|| match &dest {
                Some(dest) => write_promise(&iv.drag, item, dest),
                None => Err("the promise's URL has no path".into()),
            }))
            .unwrap_or_else(|_| Err("the promise write panicked".into()));
            match &result {
                Ok(()) => info!(drag, item, ?dest, "promise written"),
                Err(e) => warn!(drag, item, ?dest, "promise write failed: {e}"),
            }
            let error = result
                .err()
                .map(|_| NSError::new(-1, ns_string!("CursorJump")));
            let error = error
                .as_ref()
                .map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e).cast_mut());
            completion.call((error,));
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(drag: Arc<Drag>, item: usize) -> Retained<PromiseDelegate> {
        let queue = NSOperationQueue::new();
        queue.setName(Some(ns_string!("cj-promise-writes")));
        let this = PromiseDelegate::alloc().set_ivars(DelegateIvars { drag, item, queue });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}
