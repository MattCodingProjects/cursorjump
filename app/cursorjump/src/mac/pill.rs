//! The progress pills on the Mac, for drags coming from the PC.
//!
//! The [`PillBoard`] lives on the main thread (the app's `NSApplication` run
//! loop) with, per visible pill, two of CursorJump's own borderless,
//! non-activating `NSPanel`s at the status level, on every Space:
//! - a painted panel: the renderer's pixmap, shadow included, in an
//!   `NSImageView` over an `NSVisualEffectView` blur the size of the body.
//!   It ignores the mouse. Left alone, a non-opaque window takes the clicks
//!   on every pixel that isn't fully transparent, so its soft shadow halo
//!   would swallow clicks meant for the app below.
//! - an input panel over the body only, shown while the pill is running or
//!   failed, which takes the hover and the ✕ clicks. It draws the capsule
//!   at an alpha too faint to see and leaves the hit test to the window
//!   server, so the corners outside the outline let clicks through.
//!
//! Neither panel can become key or main and both are ordered in with
//! `orderFrontRegardless`, so a pill never takes focus; the input view
//! accepts the first mouse, so one click on the ✕ acts.
//!
//! - A pill stays on the screen the mouse was on at `Start`. Each screen
//!   stacks its own pills, bottom-most first in start order. A pill whose
//!   screen is gone moves to the main screen.
//! - Commands are queued with the time they were sent, and one main-queue
//!   block drains a whole burst. Redraws come from a main-queue timer at
//!   the board's next frame, at most about 30 fps, and none while idle.
//! - A fading pill fades its painted panel's `alphaValue`, so the blur fades
//!   with the pixmap; the pixmap itself is drawn at full opacity.
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use cj_core::dnd::{DragId, DragItem, FailReason};
use cj_core::pill::{Peer, PillBoard, PillView, SHOW_AFTER};
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameDarkAqua,
    NSApplicationDidChangeScreenParametersNotification, NSBackingStoreType, NSColor, NSControl,
    NSEvent, NSImage, NSImageScaling, NSImageView, NSPanel, NSResponder, NSScreen,
    NSStatusWindowLevel, NSTrackingArea, NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFData, CFRetained};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSOperationQueue, NSPoint, NSRect, NSSize,
};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};
use tracing::{info, warn};

use super::dnd::on_main;
use crate::common::pill_render::{
    BOTTOM_MARGIN, Backdrop, PILL_GAP, PILL_H, PILL_W, SHADOW_PAD, close_rect, close_shown,
    pill_px, pixmap_px, render,
};

/// Least time between two redraws: about 30 fps.
const MIN_FRAME: Duration = Duration::from_millis(33);
/// The input panel's capsule: drawn, so the window server routes clicks on
/// it to the panel, but far too faint to see over the pill.
const INPUT_ALPHA: u8 = 5;

/// App → pills.
#[derive(Debug, Clone)]
pub enum PillCmd {
    /// A drag from the PC was dropped: its pill goes on the screen the
    /// mouse is on.
    Start { drag: DragId, items: Vec<DragItem> },
    Progress {
        drag: DragId,
        done: u64,
        total: Option<u64>,
    },
    Finish {
        drag: DragId,
        result: Result<(), FailReason>,
    },
}

/// The pills' host: send it commands from any thread, once the main
/// thread's run loop is up. The board and the panels live on the main
/// thread; once every clone of the returned sender is dropped, the panels
/// close.
///
/// `on_cancel` runs on the main thread when a running pill's ✕ is clicked.
/// It must not block (the app's UI freezes until it returns); a panic in it
/// is caught and logged. A failed pill's ✕ just takes it off the board.
pub fn pill_host(
    on_cancel: impl Fn(DragId) + Send + Sync + 'static,
) -> impl Fn(PillCmd) + Send + Sync + Clone {
    let shared = Arc::new(Shared {
        id: NEXT_HOST.fetch_add(1, Ordering::Relaxed),
        queue: Mutex::new(Vec::new()),
        pending: AtomicBool::new(false),
        on_cancel: Arc::new(on_cancel),
    });
    move |cmd| shared.send(cmd)
}

type OnCancel = Arc<dyn Fn(DragId) + Send + Sync>;

static NEXT_HOST: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Each host's state, by id. Only ever touched on the main thread.
    static HOSTS: RefCell<HashMap<u64, Rc<RefCell<State>>>> = RefCell::new(HashMap::new());
}

/// Run `f` on host `id`'s state. `None` if it is gone or already borrowed
/// (AppKit called back into us mid-update): the event is then dropped.
fn with_host<R>(id: u64, f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let host = HOSTS.with(|h| h.try_borrow().ok()?.get(&id).cloned())?;
    let mut state = host.try_borrow_mut().ok()?;
    Some(f(&mut state))
}

/// What the senders share with the main thread.
struct Shared {
    id: u64,
    /// Commands not yet applied, each with when it was sent.
    queue: Mutex<Vec<(PillCmd, Instant)>>,
    /// Set while a drain is queued and hasn't started, so a burst of
    /// progress queues one block, not one each.
    pending: AtomicBool,
    on_cancel: OnCancel,
}

impl Shared {
    fn send(self: &Arc<Self>, cmd: PillCmd) {
        {
            let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            // Stamped under the lock, so the times follow the queue's order.
            queue.push((cmd, Instant::now()));
        }
        // SeqCst against the drain's swap: either it takes this command, or
        // this swap sees its `false` and queues another drain.
        if !self.pending.swap(true, Ordering::SeqCst) {
            let this = self.clone();
            on_main(move |mtm| this.drain(mtm));
        }
    }

    fn drain(&self, mtm: MainThreadMarker) {
        // Cleared before taking the queue, so a later command queues again.
        self.pending.swap(false, Ordering::SeqCst);
        let cmds = std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner));
        if HOSTS.with(|h| !h.borrow().contains_key(&self.id)) {
            let state = State::new(mtm, self.id, self.on_cancel.clone());
            HOSTS.with(|h| h.borrow_mut().insert(self.id, Rc::new(RefCell::new(state))));
        }
        let applied = with_host(self.id, |s| {
            for (cmd, at) in cmds {
                s.on_command(cmd, at);
            }
        });
        if applied.is_none() {
            warn!("pill: commands dropped, the board was busy");
        }
        pump(self.id);
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let id = self.id;
        on_main(move |_| {
            // Dropped outside the map's borrow: closing a panel may call back.
            let state = HOSTS.with(|h| h.borrow_mut().remove(&id));
            drop(state);
        });
    }
}

/// Redraw host `id` if due, and arm a timer for its next redraw.
fn pump(id: u64) {
    let now = Instant::now();
    let Some(Some(at)) = with_host(id, |s| s.tick(now)) else {
        return;
    };
    let Ok(when) = DispatchTime::try_from(at.saturating_duration_since(now)) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        let fired = catch_unwind(|| {
            with_host(id, |s| {
                if s.timer == Some(at) {
                    s.timer = None;
                }
            });
            pump(id);
        });
        if fired.is_err() {
            warn!("pill: redraw panicked");
        }
    });
}

/// A screen, as the pills see it.
struct Screen {
    id: u32,
    /// (x, y, width, height) in points, AppKit's bottom-left origin.
    frame: (f64, f64, f64, f64),
    visible: (f64, f64, f64, f64),
    scale: f32,
}

fn rect_tuple(r: NSRect) -> (f64, f64, f64, f64) {
    (r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn screens(mtm: MainThreadMarker) -> Vec<Screen> {
    NSScreen::screens(mtm)
        .iter()
        .map(|s| Screen {
            id: s.CGDirectDisplayID(),
            frame: rect_tuple(s.frame()),
            visible: rect_tuple(s.visibleFrame()),
            scale: s.backingScaleFactor() as f32,
        })
        .collect()
}

fn main_screen(mtm: MainThreadMarker) -> Option<u32> {
    NSScreen::mainScreen(mtm).map(|s| s.CGDirectDisplayID())
}

/// The screen whose frame holds `at`, if any.
fn screen_at(screens: &[Screen], at: (f64, f64)) -> Option<u32> {
    screens
        .iter()
        .find(|s| {
            let (x, y, w, h) = s.frame;
            (x..=x + w).contains(&at.0) && (y..=y + h).contains(&at.1)
        })
        .map(|s| s.id)
}

/// Where in `screens` a pill wanting `want` goes: that screen, else the
/// main one, else the first. `None` with no screens at all.
fn pick_screen(screens: &[Screen], want: Option<u32>, main: Option<u32>) -> Option<usize> {
    let find = |id: Option<u32>| id.and_then(|id| screens.iter().position(|s| s.id == id));
    find(want)
        .or_else(|| find(main))
        .or((!screens.is_empty()).then_some(0))
}

/// The shadow pad, in pixmap px, on each side of the body.
fn pad_px(scale: f32) -> u32 {
    (pixmap_px(scale).0 - pill_px(scale).0) / 2
}

/// The bottom-left corner, in points, of the `index`th pill's painted panel
/// (shadow pad included) on a screen whose visible frame is `visible`
/// (x, y, width, height; AppKit's bottom-left origin): centred,
/// `BOTTOM_MARGIN` above the bottom, stacking upward `PILL_GAP` apart.
/// macOS backing scales are whole numbers, so the pad is `SHADOW_PAD` points
/// at any scale, and a whole-point origin is on a pixel.
fn stack_origin_pt(visible: (f64, f64, f64, f64), index: usize) -> (f64, f64) {
    let (x, y, w, _) = visible;
    let (bw, bh) = (f64::from(PILL_W), f64::from(PILL_H));
    let left = x + ((w - bw) / 2.0).floor();
    let bottom = y + f64::from(BOTTOM_MARGIN) + index as f64 * (bh + f64::from(PILL_GAP));
    let pad = f64::from(SHADOW_PAD);
    (left - pad, bottom - pad)
}

/// For each pill, bottom-most first, its place in its screen's stack.
fn stack_slots(screens: &[u32]) -> Vec<usize> {
    screens
        .iter()
        .enumerate()
        .map(|(i, s)| screens[..i].iter().filter(|o| *o == s).count())
        .collect()
}

/// Whether a click at `at` (body points from its top-left corner) lands on
/// a drawn ✕.
fn close_hit(view: &PillView, hover: bool, scale: f32, at: (f64, f64)) -> bool {
    let r = close_rect(scale);
    let (pad, scale) = (f64::from(pad_px(scale)), f64::from(scale));
    let (x, y) = (pad + at.0 * scale, pad + at.1 * scale);
    let (left, top) = (f64::from(r.left()), f64::from(r.top()));
    let (right, bottom) = (f64::from(r.right()), f64::from(r.bottom()));
    close_shown(view, hover) && x >= left && x < right && y >= top && y < bottom
}

/// When to redraw next: now on a change, else at the board's next frame,
/// but never within `MIN_FRAME` of the last draw. `None`: no timer.
fn redraw_at(
    now: Instant,
    dirty: bool,
    next_frame: Option<Instant>,
    last_draw: Option<Instant>,
) -> Option<Instant> {
    let want = if dirty { Some(now) } else { next_frame };
    want.map(|t| last_draw.map_or(t, |last| t.max(last + MIN_FRAME)))
}

/// When the panels are next redrawn.
#[derive(Debug, Default)]
struct Schedule {
    /// Something changed that the panels don't show yet.
    dirty: bool,
    last_draw: Option<Instant>,
    /// The board's next frame as of the last draw. Asking again later
    /// would not do: while animating it is always 16 ms from the asking.
    next_frame: Option<Instant>,
}

impl Schedule {
    /// The panels now show `board` as of `now`.
    fn drawn(&mut self, board: &PillBoard, now: Instant) {
        self.dirty = false;
        self.last_draw = Some(now);
        self.next_frame = board.next_frame(now);
    }

    fn due(&self, now: Instant) -> Option<Instant> {
        redraw_at(now, self.dirty, self.next_frame, self.last_draw)
    }
}

/// Where a drag's pill goes: the screen of its `Start`, and when that was.
struct Anchor {
    drag: DragId,
    screen: Option<u32>,
    since: Instant,
}

/// One host's main-thread state.
struct State {
    mtm: MainThreadMarker,
    id: u64,
    board: PillBoard,
    on_cancel: OnCancel,
    anchors: Vec<Anchor>,
    /// In no particular order; matched to the views by drag.
    panels: Vec<Panels>,
    schedule: Schedule,
    /// When the latest armed timer fires. Timers can't be taken back, so
    /// one armed earlier may still fire; it then just finds nothing due.
    timer: Option<Instant>,
    /// Screen changes re-place the panels.
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

impl State {
    fn new(mtm: MainThreadMarker, id: u64, on_cancel: OnCancel) -> State {
        let block = RcBlock::new(move |_: NonNull<NSNotification>| {
            if with_host(id, |s| s.schedule.dirty = true).is_some() {
                pump(id);
            }
        });
        // SAFETY: a static AppKit name; the block is copied by the center and
        // run on the main queue; the observer is removed on drop.
        let observer = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSApplicationDidChangeScreenParametersNotification),
                None,
                Some(&NSOperationQueue::mainQueue()),
                &block,
            )
        };
        State {
            mtm,
            id,
            board: PillBoard::new(Peer::Pc, Peer::Mac),
            on_cancel,
            anchors: Vec::new(),
            panels: Vec::new(),
            schedule: Schedule::default(),
            timer: None,
            observer,
        }
    }

    fn on_command(&mut self, cmd: PillCmd, at: Instant) {
        match cmd {
            PillCmd::Start { drag, items } => {
                self.board.start(drag, &items, at);
                if !self.anchors.iter().any(|a| a.drag == drag) {
                    let mouse = NSEvent::mouseLocation();
                    let screen = screen_at(&screens(self.mtm), (mouse.x, mouse.y))
                        .or_else(|| main_screen(self.mtm));
                    self.anchors.push(Anchor {
                        drag,
                        screen,
                        since: at,
                    });
                }
            }
            PillCmd::Progress { drag, done, total } => self.board.progress(drag, done, total, at),
            PillCmd::Finish { drag, result } => self.board.finish(drag, result, at),
        }
        self.schedule.dirty = true;
    }

    /// Redraw if due; then the time to arm a timer for, unless one already
    /// fires by then.
    fn tick(&mut self, now: Instant) -> Option<Instant> {
        if self.schedule.due(now).is_some_and(|t| t <= now) {
            self.redraw(now);
        }
        let due = self.schedule.due(now)?;
        if self.timer.is_some_and(|t| t <= due) {
            return None;
        }
        self.timer = Some(due);
        Some(due)
    }

    fn redraw(&mut self, now: Instant) {
        self.schedule.drawn(&self.board, now);
        let views = self.board.views(now);
        let on_board = |drag: DragId| views.iter().any(|v| v.drag == drag);
        // A drag not yet shown keeps its anchor.
        self.anchors
            .retain(|a| on_board(a.drag) || now.saturating_duration_since(a.since) < SHOW_AFTER);
        self.panels.retain(|p| on_board(p.drag));
        let screens = screens(self.mtm);
        let main = main_screen(self.mtm);
        let placed: Vec<Option<usize>> = views
            .iter()
            .map(|v| {
                let anchor = self.anchors.iter().find(|a| a.drag == v.drag);
                pick_screen(&screens, anchor.and_then(|a| a.screen), main)
            })
            .collect();
        let Some(placed) = placed.into_iter().collect::<Option<Vec<usize>>>() else {
            return; // No screens at all.
        };
        let ids: Vec<u32> = placed.iter().map(|&i| screens[i].id).collect();
        for ((view, &at), slot) in views.iter().zip(&placed).zip(stack_slots(&ids)) {
            let screen = &screens[at];
            let i = match self.panels.iter().position(|p| p.drag == view.drag) {
                Some(i) => i,
                None => {
                    self.panels.push(Panels::new(self.mtm, self.id, view.drag));
                    self.panels.len() - 1
                }
            };
            let panels = &mut self.panels[i];
            let origin = stack_origin_pt(screen.visible, slot);
            if let Err(e) = panels.show(view, screen.scale, origin)
                && !std::mem::replace(&mut panels.warned, true)
            {
                warn!("pill: draw failed: {e:#}");
            }
        }
    }

    fn panels(&mut self, drag: DragId) -> Option<&mut Panels> {
        self.panels.iter_mut().find(|p| p.drag == drag)
    }

    /// Whether the hover changed.
    fn on_hover(&mut self, drag: DragId, hover: bool) -> bool {
        let Some(p) = self.panels(drag) else {
            return false;
        };
        if p.hover == hover || !p.input_shown {
            return false;
        }
        p.hover = hover;
        self.schedule.dirty = true;
        true
    }

    /// A click at `at` (body points from the top-left) on `drag`'s input
    /// panel: only a drawn ✕ acts. The cancel handler to call, if any.
    fn on_click(&mut self, drag: DragId, at: (f64, f64)) -> Option<OnCancel> {
        let p = self.panels(drag)?;
        let shown = p.shown.as_ref()?;
        // As drawn: a ✕ that hover was about to show isn't there yet.
        let hit = close_hit(&shown.view, shown.hover, shown.scale, at);
        let (running, failed) = (shown.view.running, shown.view.failed);
        self.on_hover(drag, true);
        if hit && running {
            Some(self.on_cancel.clone())
        } else if hit && failed {
            info!(drag, "pill: failed pill dismissed");
            self.board.dismiss(drag);
            self.schedule.dirty = true;
            None
        } else {
            None
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let observer: &AnyObject = self.observer.as_ref();
        // SAFETY: the observer this state added.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(observer) };
    }
}

fn on_hover(host: u64, drag: DragId, hover: bool) {
    if with_host(host, |s| s.on_hover(drag, hover)) == Some(true) {
        pump(host);
    }
}

fn on_click(host: u64, drag: DragId, at: (f64, f64)) {
    // Called outside the state's borrow: it may send commands right back.
    if let Some(Some(cancel)) = with_host(host, |s| s.on_click(drag, at)) {
        info!(drag, "pill: ✕ clicked; cancelling");
        if catch_unwind(AssertUnwindSafe(|| cancel(drag))).is_err() {
            warn!("pill: on_cancel panicked for drag {drag}");
        }
    }
    pump(host);
}

/// What a pill's panels show now.
#[derive(Clone, PartialEq)]
struct Shown {
    view: PillView,
    scale: f32,
    hover: bool,
    origin: (f64, f64),
}

impl Shown {
    /// Whether `other` draws the same pixels (the opacity is the panel's).
    fn same_pixels(&self, other: &Shown) -> bool {
        self.scale == other.scale
            && self.hover == other.hover
            && PillView {
                opacity: other.view.opacity,
                ..self.view.clone()
            } == other.view
    }
}

/// One pill's panels. Dropping it closes them.
struct Panels {
    drag: DragId,
    paint: Retained<PillPanel>,
    blur: Retained<NSVisualEffectView>,
    image: Retained<NSImageView>,
    input: Retained<PillPanel>,
    input_view: Retained<InputView>,
    hover: bool,
    input_shown: bool,
    shown: Option<Shown>,
    /// A failed draw is logged once per pill.
    warned: bool,
}

impl Panels {
    fn new(mtm: MainThreadMarker, host: u64, drag: DragId) -> Panels {
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let paint = PillPanel::new(mtm, zero);
        paint.setIgnoresMouseEvents(true);
        let content = NSView::initWithFrame(NSView::alloc(mtm), zero);
        let blur = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), zero);
        blur.setMaterial(NSVisualEffectMaterial::HUDWindow);
        blur.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        blur.setState(NSVisualEffectState::Active);
        // SAFETY: a static AppKit name.
        let dark = NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua });
        blur.setAppearance(dark.as_deref());
        let image = NSImageView::initWithFrame(NSImageView::alloc(mtm), zero);
        image.setImageScaling(NSImageScaling::ScaleAxesIndependently);
        content.addSubview(&blur);
        content.addSubview(&image);
        paint.setContentView(Some(&content));

        let input = PillPanel::new(mtm, zero);
        let input_view = InputView::new(mtm, host, drag);
        input.setContentView(Some(&input_view));
        Panels {
            drag,
            paint,
            blur,
            image,
            input,
            input_view,
            hover: false,
            input_shown: false,
            shown: None,
            warned: false,
        }
    }

    /// Bring the panels up to `view`, the painted one at `origin` (points)
    /// on a screen at `scale`.
    fn show(&mut self, view: &PillView, scale: f32, origin: (f64, f64)) -> Result<()> {
        let clickable = view.running || view.failed;
        if !clickable && self.input_shown {
            self.input.orderOut(None);
            self.input_shown = false;
            self.hover = false;
        }
        let want = Shown {
            view: view.clone(),
            scale,
            hover: self.hover,
            origin,
        };
        if self.shown.as_ref() == Some(&want) {
            return Ok(());
        }
        let k = f64::from(scale);
        let pt = |px: u32| f64::from(px) / k;
        let ((pw, ph), (bw, bh)) = (pixmap_px(scale), pill_px(scale));
        let pad = pt(pad_px(scale));
        let body_size = NSSize::new(pt(bw), pt(bh));
        let moved = self
            .shown
            .as_ref()
            .is_none_or(|s| s.origin != origin || s.scale != scale);
        if moved {
            let at = NSPoint::new(origin.0, origin.1);
            let frame = NSRect::new(at, NSSize::new(pt(pw), pt(ph)));
            self.paint.setFrame_display(frame, false);
            let body = NSPoint::new(origin.0 + pad, origin.1 + pad);
            self.input
                .setFrame_display(NSRect::new(body, body_size), false);
        }
        if self.shown.as_ref().is_none_or(|s| s.scale != scale) {
            let full = NSRect::new(NSPoint::new(0.0, 0.0), frame_size(scale));
            self.image.setFrame(full);
            self.blur
                .setFrame(NSRect::new(NSPoint::new(pad, pad), body_size));
            let mask = capsule(scale, 255).context("draw the blur's mask")?;
            self.blur.setMaskImage(Some(&*ns_image(&mask, scale)?));
            let faint = capsule(scale, INPUT_ALPHA).context("draw the input capsule")?;
            self.input_view.setImage(Some(&*ns_image(&faint, scale)?));
        }
        if !self.shown.as_ref().is_some_and(|s| s.same_pixels(&want)) {
            let opaque = PillView {
                opacity: 1.0,
                ..view.clone()
            };
            let pm = render(&opaque, scale, Backdrop::Blurred, self.hover);
            self.image.setImage(Some(&*ns_image(&pm, scale)?));
        }
        self.paint
            .setAlphaValue(f64::from(view.opacity.clamp(0.0, 1.0)));
        if self.shown.is_none() {
            self.paint.orderFrontRegardless();
        }
        if clickable && !self.input_shown {
            // Over the painted panel: both are at the same level.
            self.input.orderFrontRegardless();
            self.input_shown = true;
        }
        self.shown = Some(want);
        Ok(())
    }
}

impl Drop for Panels {
    fn drop(&mut self) {
        self.input.orderOut(None);
        self.input.close();
        self.paint.orderOut(None);
        self.paint.close();
    }
}

/// The painted panel's size, in points.
fn frame_size(scale: f32) -> NSSize {
    let (w, h) = pixmap_px(scale);
    let k = f64::from(scale);
    NSSize::new(f64::from(w) / k, f64::from(h) / k)
}

/// The body's capsule, filled black at `alpha`, at `scale`.
fn capsule(scale: f32, alpha: u8) -> Option<Pixmap> {
    let (w, h) = pill_px(scale);
    let mut pm = Pixmap::new(w, h)?;
    let (w, h) = (w as f32, h as f32);
    let r = h / 2.0;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(0, 0, 0, alpha));
    paint.anti_alias = true;
    // Filled one by one: overlapping shapes in one path could cancel out.
    let shapes = [
        PathBuilder::from_rect(Rect::from_xywh(r, 0.0, w - 2.0 * r, h)?),
        PathBuilder::from_circle(r, r, r)?,
        PathBuilder::from_circle(w - r, r, r)?,
    ];
    for shape in &shapes {
        pm.fill_path(
            shape,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    Some(pm)
}

/// A premultiplied RGBA pixmap as an image of `pixels / scale` points.
fn ns_image(pm: &Pixmap, scale: f32) -> Result<Retained<NSImage>> {
    let cg = cg_image(pm).context("CGImageCreate")?;
    let k = f64::from(scale);
    let size = NSSize::new(f64::from(pm.width()) / k, f64::from(pm.height()) / k);
    Ok(NSImage::initWithCGImage_size(NSImage::alloc(), &cg, size))
}

/// tiny-skia's premultiplied RGBA, bytes in R, G, B, A order: alpha
/// premultiplied and last, in the default (byte-wise) order.
fn cg_image(pm: &Pixmap) -> Option<CFRetained<CGImage>> {
    let data = CFData::from_bytes(pm.data());
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    // SAFETY: a static CoreGraphics name.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let info =
        CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::OrderDefault.0);
    // SAFETY: the provider holds w * h 4-byte pixels; no decode array.
    unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

define_class!(
    /// A pill's panel: it never becomes key or main, so it never takes focus.
    // SAFETY: NSPanel has no subclassing requirements; PillPanel has no Drop.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJPillPanel"]
    struct PillPanel;

    unsafe impl NSObjectProtocol for PillPanel {}

    impl PillPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            false
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }
    }
);

impl PillPanel {
    /// Borderless, non-activating, clear and shadowless (the pixmap has its
    /// own), at the status level on every Space and over full-screen apps.
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<PillPanel> {
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let this = PillPanel::alloc(mtm).set_ivars(());
        // SAFETY: NSWindow's designated initializer.
        let panel: Retained<PillPanel> = unsafe {
            msg_send![super(this), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        // SAFETY: `Panels` owns the panel; closing must not release it.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary,
        );
        panel.setLevel(NSStatusWindowLevel);
        panel.setBecomesKeyOnlyIfNeeded(true);
        // CursorJump is never the active app; its pills must still show.
        panel.setHidesOnDeactivate(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setOpaque(false);
        panel.setHasShadow(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel
    }
}

/// Which pill an input view belongs to.
#[derive(Clone, Copy)]
struct InputIvars {
    host: u64,
    drag: DragId,
}

define_class!(
    /// The input panel's view: the faint capsule, the hover and the clicks.
    // SAFETY: NSImageView has no subclassing requirements; InputView has no Drop.
    #[unsafe(super(NSImageView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CJPillInputView"]
    #[ivars = InputIvars]
    struct InputView;

    unsafe impl NSObjectProtocol for InputView {}

    impl InputView {
        /// One click acts, though CursorJump is never the active app.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = self.convertPoint_fromView(event.locationInWindow(), None);
            let y = if self.isFlipped() {
                p.y
            } else {
                self.bounds().size.height - p.y
            };
            let InputIvars { host, drag } = *self.ivars();
            on_click(host, drag, (p.x, y));
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            on_hover(self.ivars().host, self.ivars().drag, true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            on_hover(self.ivars().host, self.ivars().drag, false);
        }
    }
);

impl InputView {
    fn new(mtm: MainThreadMarker, host: u64, drag: DragId) -> Retained<InputView> {
        let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1.0, 1.0));
        let this = InputView::alloc(mtm).set_ivars(InputIvars { host, drag });
        // SAFETY: NSView's designated initializer.
        let view: Retained<InputView> = unsafe { msg_send![super(this), initWithFrame: zero] };
        view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
        // Always active: CursorJump is never the active app. The rect
        // follows the view's bounds.
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::InVisibleRect;
        let owner: &AnyObject = view.as_ref();
        // SAFETY: the owner is this view, which handles mouseEntered:/mouseExited:
        // and outlives the area it owns.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                zero,
                options,
                Some(owner),
                None,
            )
        };
        view.addTrackingArea(&area);
        view
    }
}

/// For a person to look at, on the real main thread, for about 10 s, then
/// exits: `cargo run -p cursorjump -- --smoke-pill` (debug builds only).
/// No network, injector or tray. Watch for:
/// - a fast drag that never shows (it ends within `SHOW_AFTER`);
/// - "Holiday.mov" (known size) and, from 1 s, "2 files" (unknown size: an
///   indeterminate track) stacking on the screen under the mouse;
/// - "2 files" failing at 5 s: it stays, and its ✕ dismisses it;
/// - "Holiday.mov" finishing at 6 s: ✓, hold, fade;
/// - hovering a running pill shows its ✕; clicking it logs and cancels;
/// - no pill ever takes focus from the frontmost app.
///
/// The plan's `smoke_pill_runs_5s` is this run: a bin crate's unit tests
/// don't get `CARGO_BIN_EXE_cursorjump`, and libtest never runs a test on
/// the process's main thread, which AppKit needs.
#[cfg(debug_assertions)]
pub fn run_smoke() -> Result<()> {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let mtm = MainThreadMarker::new().context("--smoke-pill must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<DragId>();
    let pills = pill_host(move |drag| {
        info!(drag, "smoke: ✕ clicked; the drag is cancelled");
        let _ = cancel_tx.send(drag);
    });
    std::thread::Builder::new()
        .name("cj-pill-smoke".into())
        .spawn(move || {
            smoke_script(&pills, &cancel_rx);
            drop(pills);
            std::thread::sleep(Duration::from_millis(300));
            on_main(|_| std::process::exit(0));
        })
        .context("spawn the smoke script")?;
    app.run();
    Ok(())
}

#[cfg(debug_assertions)]
fn smoke_script(send: &impl Fn(PillCmd), cancelled: &std::sync::mpsc::Receiver<DragId>) {
    use cj_core::dnd::ItemKind;

    const TOTAL: u64 = 250_000_000;
    let ms = Duration::from_millis;
    let file = |name: &str, size: Option<u64>| DragItem {
        name: name.into(),
        kind: ItemKind::File,
        size,
    };
    let t0 = Instant::now();
    info!("smoke: a fast drag (never shown) and Holiday.mov (known size)");
    send(PillCmd::Start {
        drag: 1,
        items: vec![file("notes.txt", Some(2_000))],
    });
    send(PillCmd::Start {
        drag: 2,
        items: vec![file("Holiday.mov", Some(TOTAL))],
    });
    // Drags 1 to 3, by index: still running.
    let mut running = [true, true, false];
    let mut started_3 = false;
    while t0.elapsed() < ms(10_000) {
        let t = t0.elapsed();
        while let Ok(drag) = cancelled.try_recv() {
            running[(drag - 1) as usize] = false;
            send(PillCmd::Finish {
                drag,
                result: Err(FailReason::Cancelled),
            });
        }
        if running[0] && t >= ms(150) {
            running[0] = false;
            send(PillCmd::Finish {
                drag: 1,
                result: Ok(()),
            });
        }
        if running[1] {
            // About 40 MB/s.
            let done = (TOTAL as f64 * (t.as_secs_f64() / 6.0).min(1.0)) as u64;
            send(PillCmd::Progress {
                drag: 2,
                done,
                total: Some(TOTAL),
            });
            if t >= ms(6_000) {
                running[1] = false;
                info!("smoke: Holiday.mov done (✓, then fade)");
                send(PillCmd::Finish {
                    drag: 2,
                    result: Ok(()),
                });
            }
        }
        if !started_3 && t >= ms(1_000) {
            started_3 = true;
            running[2] = true;
            info!("smoke: 2 files of unknown size (indeterminate track)");
            send(PillCmd::Start {
                drag: 3,
                items: vec![file("a.png", None), file("b.png", None)],
            });
        }
        if running[2] {
            let done = t.saturating_sub(ms(1_000)).as_millis() as u64 * 9_000;
            send(PillCmd::Progress {
                drag: 3,
                done,
                total: None,
            });
            if t >= ms(5_000) {
                running[2] = false;
                info!("smoke: 2 files failed (stays; its ✕ dismisses it)");
                send(PillCmd::Finish {
                    drag: 3,
                    result: Err(FailReason::Disconnected),
                });
            }
        }
        // Progress at 20 Hz, as a transfer reports it.
        std::thread::sleep(ms(50));
    }
    info!("smoke: done");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::dnd::ItemKind;
    use cj_core::pill::PillIcon;

    fn view(running: bool, failed: bool) -> PillView {
        PillView {
            drag: 1,
            icon: PillIcon::Video,
            name: "Holiday.mov".into(),
            right: String::new(),
            sub: String::new(),
            fraction: None,
            sheen_phase: 0.0,
            check_scale: 0.0,
            opacity: 1.0,
            failed,
            running,
        }
    }

    fn screen(id: u32, frame: (f64, f64, f64, f64)) -> Screen {
        Screen {
            id,
            frame,
            visible: frame,
            scale: 2.0,
        }
    }

    #[test]
    fn stack_origin_pt_bottom_centre_and_upward() {
        // A 1440 x 875 visible frame above a 25 pt Dock: the 360 x 52 body is
        // centred, 24 pt above the bottom, inside a 12 pt shadow pad.
        let visible = (0.0, 25.0, 1440.0, 875.0);
        assert_eq!(stack_origin_pt(visible, 0), (540.0 - 12.0, 49.0 - 12.0));
        // The next one sits PILL_GAP above it: 52 + 8 higher.
        assert_eq!(stack_origin_pt(visible, 1), (528.0, 109.0 - 12.0));
        assert_eq!(stack_origin_pt(visible, 2), (528.0, 169.0 - 12.0));
        // A screen left of and below the primary one.
        let left = (-1920.0, -300.0, 1920.0, 1055.0);
        assert_eq!(
            stack_origin_pt(left, 0),
            (-1920.0 + 780.0 - 12.0, -276.0 - 12.0)
        );
    }

    #[test]
    fn stack_origin_pt_stays_on_whole_points() {
        // An odd width can't centre exactly: it rounds down to a whole point.
        assert_eq!(stack_origin_pt((0.0, 0.0, 1441.0, 900.0), 0).0, 528.0);
    }

    #[test]
    fn neighbouring_bodies_dont_overlap() {
        let visible = (0.0, 25.0, 1440.0, 875.0);
        let (lower, upper) = (stack_origin_pt(visible, 0), stack_origin_pt(visible, 1));
        assert!(lower.1 + f64::from(PILL_H) < upper.1);
    }

    #[test]
    fn each_screen_stacks_its_own_pills() {
        assert_eq!(stack_slots(&[]), Vec::<usize>::new());
        assert_eq!(stack_slots(&[7, 7, 7]), [0, 1, 2]);
        assert_eq!(stack_slots(&[7, 9, 7, 9, 9]), [0, 0, 1, 1, 2]);
    }

    #[test]
    fn the_screen_under_the_mouse() {
        let screens = [
            screen(1, (0.0, 0.0, 1440.0, 900.0)),
            screen(2, (1440.0, -200.0, 1920.0, 1080.0)),
        ];
        assert_eq!(screen_at(&screens, (100.0, 100.0)), Some(1));
        assert_eq!(screen_at(&screens, (2000.0, -100.0)), Some(2));
        // The top edge belongs to the screen (the mouse can sit there).
        assert_eq!(screen_at(&screens, (100.0, 900.0)), Some(1));
        assert_eq!(screen_at(&screens, (100.0, 2000.0)), None);
    }

    #[test]
    fn a_gone_screen_falls_back_to_the_main_one() {
        let screens = [
            screen(1, (0.0, 0.0, 1440.0, 900.0)),
            screen(2, (1440.0, 0.0, 1920.0, 1080.0)),
        ];
        assert_eq!(pick_screen(&screens, Some(2), Some(1)), Some(1));
        assert_eq!(pick_screen(&screens, Some(9), Some(2)), Some(1));
        assert_eq!(pick_screen(&screens, None, Some(2)), Some(1));
        // No main screen either: the first.
        assert_eq!(pick_screen(&screens, Some(9), None), Some(0));
        assert_eq!(pick_screen(&[], Some(1), Some(1)), None);
    }

    #[test]
    fn close_hit_only_on_a_shown_close() {
        // The ✕ centre in body points from the top-left.
        let at = |scale: f32| {
            let r = close_rect(scale);
            let pad = pad_px(scale) as f32;
            (
                f64::from(((r.left() + r.right()) / 2.0 - pad) / scale),
                f64::from(((r.top() + r.bottom()) / 2.0 - pad) / scale),
            )
        };
        for scale in [1.0, 2.0] {
            let hit = at(scale);
            assert!(close_hit(&view(true, false), true, scale, hit));
            assert!(!close_hit(&view(true, false), false, scale, hit));
            assert!(close_hit(&view(false, true), false, scale, hit));
            // Done or cancelled: no ✕, so no click.
            assert!(!close_hit(&view(false, false), true, scale, hit));
            // Elsewhere on the body.
            assert!(!close_hit(&view(false, true), true, scale, (10.0, 10.0)));
        }
    }

    #[test]
    fn close_hit_edges_follow_close_rect() {
        let scale = 2.0;
        let r = close_rect(scale);
        let pad = pad_px(scale) as f32;
        let pt = |px: f32| f64::from((px - pad) / scale);
        let failed = view(false, true);
        let (l, t, rt, b) = (pt(r.left()), pt(r.top()), pt(r.right()), pt(r.bottom()));
        assert!(close_hit(&failed, false, scale, (l, t)));
        assert!(close_hit(&failed, false, scale, (rt - 0.5, b - 0.5)));
        assert!(!close_hit(&failed, false, scale, (l - 0.5, t)));
        assert!(!close_hit(&failed, false, scale, (rt, t)));
        assert!(!close_hit(&failed, false, scale, (l, b)));
    }

    #[test]
    fn the_panels_match_the_pixmap() {
        for scale in [1.0, 2.0, 3.0] {
            // The pad is SHADOW_PAD points, as stack_origin_pt assumes.
            assert_eq!(f64::from(pad_px(scale)) / f64::from(scale), 12.0);
            let size = frame_size(scale);
            assert_eq!((size.width, size.height), (384.0, 76.0));
        }
    }

    #[test]
    fn redraws_are_capped_and_idle_sleeps() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // Nothing to do: no timer at all.
        assert_eq!(redraw_at(t0, false, None, Some(t0)), None);
        // A change draws now, unless the last draw was under 33 ms ago.
        assert_eq!(redraw_at(t0, true, None, None), Some(t0));
        assert_eq!(
            redraw_at(t0 + ms(50), true, None, Some(t0)),
            Some(t0 + ms(50))
        );
        assert_eq!(
            redraw_at(t0 + ms(5), true, None, Some(t0)),
            Some(t0 + ms(33))
        );
        // An animation frame 16 ms out waits for the cap; a far one is kept.
        assert_eq!(
            redraw_at(t0, false, Some(t0 + ms(16)), Some(t0)),
            Some(t0 + ms(33))
        );
        assert_eq!(
            redraw_at(t0, false, Some(t0 + ms(500)), Some(t0)),
            Some(t0 + ms(500))
        );
    }

    #[test]
    fn an_animating_board_is_redrawn_every_frame() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        let item = DragItem {
            name: "Holiday.mov".into(),
            kind: ItemKind::File,
            size: Some(10_000_000),
        };
        board.start(1, &[item], t0);
        board.progress(1, 1_000, Some(10_000_000), t0 + ms(600));
        let mut schedule = Schedule {
            dirty: true,
            ..Schedule::default()
        };
        let mut now = t0 + ms(600);
        for frame in 0..10 {
            let due = schedule.due(now).expect("animating");
            assert!(due <= now, "frame {frame} not due at its timer");
            schedule.drawn(&board, now);
            let next = schedule.due(now).expect("animating");
            assert_eq!(next, now + MIN_FRAME, "frame {frame}");
            now = next;
        }
        // Once done and faded, no timer.
        board.finish(1, Ok(()), now);
        schedule.drawn(&board, now + Duration::from_secs(5));
        assert_eq!(schedule.due(now + Duration::from_secs(5)), None);
    }

    #[test]
    fn only_a_change_of_pixels_redraws_the_image() {
        let shown = Shown {
            view: view(true, false),
            scale: 2.0,
            hover: false,
            origin: (0.0, 0.0),
        };
        let mut fading = shown.clone();
        fading.view.opacity = 0.4;
        fading.origin = (5.0, 5.0);
        assert!(shown.same_pixels(&fading));
        let mut hovered = shown.clone();
        hovered.hover = true;
        assert!(!shown.same_pixels(&hovered));
        let mut done = shown.clone();
        done.view.running = false;
        assert!(!shown.same_pixels(&done));
    }

    #[test]
    fn capsule_is_the_body_with_round_ends() {
        let pm = capsule(2.0, 255).expect("a capsule");
        assert_eq!((pm.width(), pm.height()), pill_px(2.0));
        let alpha = |x: u32, y: u32| pm.pixel(x, y).expect("in the pixmap").alpha();
        // Filled in the middle and across the round ends' centres.
        assert_eq!(alpha(pm.width() / 2, pm.height() / 2), 255);
        assert_eq!(alpha(pm.height() / 2, pm.height() / 2), 255);
        // Clear in the corners, outside the outline.
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(pm.width() - 1, pm.height() - 1), 0);
        let faint = capsule(1.0, INPUT_ALPHA).expect("a capsule");
        let mid = faint
            .pixel(faint.width() / 2, faint.height() / 2)
            .expect("in");
        assert_eq!((mid.red(), mid.alpha()), (0, INPUT_ALPHA));
    }

    #[test]
    fn cg_image_reads_the_pixmap_as_rgba_premultiplied() {
        // Opaque red, then green at half alpha (premultiplied).
        let px = [255, 0, 0, 255, 0, 128, 0, 128];
        let mut pm = Pixmap::new(2, 1).expect("a pixmap");
        pm.data_mut().copy_from_slice(&px);
        let cg = cg_image(&pm).expect("an image");
        // Drawn into a bitmap of the same layout, it comes back unchanged.
        let mut out = [0u8; 8];
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).expect("sRGB");
        let info = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::OrderDefault.0;
        // SAFETY: `out` holds the 2 x 1 4-byte pixels and outlives the context.
        let ctx = unsafe {
            objc2_core_graphics::CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                2,
                1,
                8,
                8,
                Some(&space),
                info,
            )
        }
        .expect("a bitmap context");
        let all = objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(2.0, 1.0),
        );
        objc2_core_graphics::CGContext::draw_image(Some(&ctx), all, Some(&cg));
        drop(ctx);
        for (got, want) in out.iter().zip(px) {
            assert!(got.abs_diff(want) <= 1, "{out:?} != {px:?}");
        }
    }
}
