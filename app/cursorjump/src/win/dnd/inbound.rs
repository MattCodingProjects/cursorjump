//! cj-dnd-in: the shield and the drags from the Mac (`source`).
//!
//! Its own STA thread, apart from cj-dnd-out's catcher: `DoDragDrop` must
//! run on the thread that owns the window the passed press went to (the
//! shield), and a target's synchronous `Drop` (Explorer's) can hold that
//! thread in an outgoing COM call for a whole fetch, during which its loop
//! takes no posted messages and no timers. The catcher goes on meanwhile.
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cj_core::config::Config;
use cj_core::dnd::{DragId, DragItem};
use cj_core::layout::{Rect, Side};
use tracing::{debug, info, warn};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, KillTimer, PostMessageW, SetTimer, WM_NULL,
};
use windows::core::w;

use super::super::input_thread::rect_center;
use super::{
    DndCmd, DndShared, Emit, KEEP_TIMER, Win, WinDndEv, create_window, edge_of, left_button_down,
    register_class, scale_at, shield_rect, source,
};

const KEEP_POLL: Duration = Duration::from_millis(50);
/// Longest a kept shield waits for a `StartIncoming` with none running.
const KEEP_LIMIT: Duration = Duration::from_secs(1);

/// The shield's modes and the drag from the Mac, apart from the window
/// (pure, so it can be tested).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct InModes {
    pub(super) shield_on: bool,
    /// The drag from the Mac that `DoDragDrop` is starting or running.
    pub(super) incoming: Option<DragId>,
    /// ... and its `DoDragDrop` has not called `QueryContinueDrag` yet.
    pub(super) starting: bool,
    /// A `Shield(false)` not applied yet: the press the shield got is what
    /// `DoDragDrop` needs, so the shield stays until the incoming drag has
    /// started (or, with none started, until Windows sees the button up).
    pub(super) shield_kept: bool,
    /// Windows sees the left button down (`GetAsyncKeyState`), as of the
    /// last command or poll.
    pub(super) held: bool,
}

impl InModes {
    /// Whether the shield shows. `parked`: cj-dnd-out's catcher covers the
    /// screen, and with it the park point: the shield above it would take
    /// the riding drag off its drop target.
    pub(super) fn shield(&self, parked: bool) -> bool {
        (self.shield_on || self.shield_kept) && !parked
    }

    /// `Shield`, as routed here; anything else is cj-dnd-out's.
    pub(super) fn command(&mut self, cmd: &DndCmd, session_up: bool, parked: bool) {
        match *cmd {
            DndCmd::Shield(true) if !session_up => {
                debug!("dnd: Shield(true) refused without a session");
            }
            DndCmd::Shield(true) => {
                self.shield_on = true;
                // Back on the Mac: a hide still waiting is moot.
                self.shield_kept = false;
            }
            DndCmd::Shield(false) => {
                // The app's StartIncoming and the input thread's hide come
                // in either order: keep a shown shield while a drag from
                // the Mac is starting, or may yet start.
                let may_start = self.incoming.is_none() && self.held;
                self.shield_kept |= self.shield(parked) && (self.starting || may_start);
                self.shield_on = false;
                if self.shield_kept {
                    debug!(incoming = ?self.incoming, "dnd: shield kept for the press");
                }
            }
            _ => {}
        }
    }

    /// A drag from the Mac is to start: false while another one runs.
    pub(super) fn begin_incoming(&mut self, drag: DragId) -> bool {
        if self.incoming.is_some() {
            return false;
        }
        self.incoming = Some(drag);
        self.starting = true;
        true
    }

    /// Its `DoDragDrop` called `QueryContinueDrag` for the first time: OLE
    /// holds its own capture, and the press has done its work (R38b). Not
    /// when `DoDragDrop` returns (R38a): a target may hold it much longer
    /// (Explorer's `Drop` waits for the whole fetch), and the shield would
    /// sit on the PC all that time, a dead square under the drag.
    pub(super) fn let_go_incoming(&mut self, drag: DragId) {
        if self.incoming == Some(drag) {
            self.starting = false;
            self.shield_kept = false;
        }
    }

    /// Its `DoDragDrop` returned (or never ran): a kept hide applies now.
    pub(super) fn end_incoming(&mut self, drag: DragId) {
        if self.incoming == Some(drag) {
            self.incoming = None;
            self.starting = false;
            self.shield_kept = false;
        }
    }

    /// The button Windows saw is up (or the wait is over) with no drag from
    /// the Mac started: none will start on that press.
    pub(super) fn release_kept(&mut self) {
        if !self.starting {
            self.shield_kept = false;
        }
    }
}

pub(super) struct InState {
    shared: Arc<DndShared>,
    events: Emit,
    side: Side,
    edge: Option<Rect>,
    shield: Win,
    modes: InModes,
    /// A drag from the Mac to run once the state is let go of.
    start: Option<source::Run>,
    /// Drags from the Mac still served; cancelled when the thread stops.
    live: source::Live,
    keep_timer: bool,
    /// Since when the shield has been kept with no drag from the Mac running.
    kept_since: Option<Instant>,
}

thread_local! {
    static STATE: RefCell<Option<InState>> = const { RefCell::new(None) };
}

/// Run `f` on this thread's state: `None` on cj-dnd-out, or while busy.
fn with_in<R>(f: impl FnOnce(&mut InState) -> R) -> Option<R> {
    STATE.with(|cell| {
        let mut guard = cell.try_borrow_mut().ok()?;
        guard.as_mut().map(f)
    })
}

/// Whether this thread's state is borrowed (a modal loop inside its call).
pub(super) fn busy() -> bool {
    STATE.with(|cell| cell.try_borrow_mut().is_err())
}

/// Builds the state on this thread; returns the shield, the window the
/// relay wakes.
pub(super) fn start(
    cfg: Config,
    shared: Arc<DndShared>,
    events: Emit,
    live: source::Live,
) -> Result<isize> {
    let state = InState::new(cfg, shared, events, live)?;
    let hwnd = state.shield.hwnd.0 as isize;
    STATE.with(|cell| *cell.borrow_mut() = Some(state));
    Ok(hwnd)
}

/// Drops the state: every live drag from the Mac is cancelled, the shield
/// destroyed.
pub(super) fn stop() {
    STATE.with(|cell| cell.borrow_mut().take());
}

/// One command, routed here. A `StartIncoming` runs `DoDragDrop` after the
/// state is let go of: its modal loop dispatches this thread's messages,
/// and so its commands.
pub(super) fn on_command(cmd: DndCmd) {
    let start = with_in(|s| {
        s.on_command(cmd);
        s.start.take()
    })
    .flatten();
    let Some(run) = start else {
        return;
    };
    let drag = run.drag;
    // The relay refuses another StartIncoming while this one runs: this
    // thread may be held in a target's `Drop` for the whole fetch.
    let running = Running::new(drag, run.shared.clone(), run.events.clone());
    source::run_incoming(run);
    drop(running);
    with_in(|s| s.incoming_ended(drag));
}

/// `DndShared::incoming` while a drag from the Mac runs on this thread.
/// Cleared when it ends, unwind included. If this thread unwinds out of
/// the drag (and so out of its loop: it dies), the drag's `IncomingEnded`
/// is sent here, as `run_incoming` never got to.
pub(super) struct Running {
    drag: DragId,
    shared: Arc<DndShared>,
    events: Emit,
}

impl Running {
    pub(super) fn new(drag: DragId, shared: Arc<DndShared>, events: Emit) -> Running {
        shared.incoming.store(drag, Ordering::Release);
        Running {
            drag,
            shared,
            events,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.incoming.store(0, Ordering::Release);
        if std::thread::panicking() {
            let drag = self.drag;
            warn!(drag, "cj-dnd-in: unwinding out of a drag from the Mac");
            let dropped = false;
            self.events.send(WinDndEv::IncomingEnded { drag, dropped });
        }
    }
}

/// The drop source of `drag` said its first `QueryContinueDrag` (called on
/// cj-dnd-in from `DoDragDrop`'s loop, outside the state).
pub(super) fn incoming_let_go(drag: DragId) {
    with_in(|s| s.incoming_let_go(drag));
}

/// cj-dnd-out's catcher covers the screen, or stopped covering it.
pub(super) fn parked_changed() {
    with_in(|s| {
        // cj-dnd-out may have hidden the shield itself (`ShowWindowAsync`).
        s.shield.stale = true;
        s.apply();
    });
}

pub(super) fn display_changed() {
    with_in(|s| {
        s.refresh_edge();
        s.apply();
    });
}

pub(super) fn keep_tick() {
    with_in(InState::check_kept);
}

impl InState {
    pub(super) fn new(
        cfg: Config,
        shared: Arc<DndShared>,
        events: Emit,
        live: source::Live,
    ) -> Result<Self> {
        let hinst = register_class().context("register catcher window class")?;
        let shield = create_window(hinst, w!("CursorJump shield")).context("create shield")?;
        shared.shield.store(shield.0 as isize, Ordering::Release);
        let state = InState {
            shared,
            events,
            side: cfg.mac_side,
            edge: edge_of(cfg.mac_side),
            shield: Win::new(shield),
            modes: InModes::default(),
            start: None,
            live,
            keep_timer: false,
            kept_since: None,
        };
        info!(edge = ?state.edge, "cj-dnd-in: ready");
        Ok(state)
    }

    fn session_up(&self) -> bool {
        self.shared.session_up.load(Ordering::Acquire)
    }

    fn parked(&self) -> bool {
        self.shared.parked.load(Ordering::Acquire)
    }

    pub(super) fn on_command(&mut self, cmd: DndCmd) {
        debug!(?cmd, "cj-dnd-in: command");
        self.modes.held = left_button_down();
        match cmd {
            DndCmd::StartIncoming { drag, items } => self.start_incoming(drag, items),
            DndCmd::CancelIncoming { drag } => {
                // The relay cancels a drag already registered. One it had
                // not registered yet (the cancel came during its
                // placeholders' file I/O) is registered by now: commands
                // reach this thread in order, so this one is drained inside
                // that drag's DoDragDrop, after `run_incoming` registered it.
                let reached = source::cancel_live(&self.live, Some(drag));
                let running = self.modes.incoming == Some(drag);
                debug!(drag, running, reached, "incoming: CancelIncoming");
                if reached > 0 {
                    // One more QueryContinueDrag, now the flag is set.
                    // SAFETY: plain FFI call on our own window.
                    let _ = unsafe {
                        PostMessageW(Some(self.shield.hwnd), WM_NULL, WPARAM(0), LPARAM(0))
                    };
                }
            }
            DndCmd::ApplyConfig(cfg) => {
                self.side = cfg.mac_side;
                self.refresh_edge();
            }
            cmd => {
                let (session_up, parked) = (self.session_up(), self.parked());
                self.modes.command(&cmd, session_up, parked);
            }
        }
        self.apply();
    }

    /// Queued for `on_command` to run once the state is let go of.
    fn start_incoming(&mut self, drag: DragId, items: Vec<DragItem>) {
        if !self.modes.begin_incoming(drag) {
            warn!(drag, running = ?self.modes.incoming, "incoming: another drag from the Mac runs; refused");
            self.events.send(WinDndEv::IncomingEnded {
                drag,
                dropped: false,
            });
            return;
        }
        self.start = Some(source::Run {
            drag,
            items,
            live: self.live.clone(),
            shared: self.shared.clone(),
            events: self.events.clone(),
        });
    }

    fn incoming_let_go(&mut self, drag: DragId) {
        self.modes.let_go_incoming(drag);
        self.apply();
    }

    fn incoming_ended(&mut self, drag: DragId) {
        self.modes.end_incoming(drag);
        self.apply();
    }

    fn refresh_edge(&mut self) {
        self.edge = edge_of(self.side);
        self.shield.stale = true;
    }

    fn apply(&mut self) {
        let at = self
            .edge
            .filter(|_| self.modes.shield(self.parked()))
            .map(|e| rect_center(&e))
            .map(|park| shield_rect(park, scale_at(park)));
        self.shield.place(at);
        self.set_keep_timer(self.modes.shield_kept && !self.modes.starting);
    }

    fn set_keep_timer(&mut self, on: bool) {
        if on == self.keep_timer {
            return;
        }
        self.keep_timer = on;
        self.kept_since = None;
        let hwnd = self.shield.hwnd;
        if on {
            // SAFETY: our own window's timer; WM_TIMER arrives in `catcher_proc`.
            if unsafe { SetTimer(Some(hwnd), KEEP_TIMER, KEEP_POLL.as_millis() as u32, None) } == 0
            {
                // The next command or the incoming drag's end lets it go.
                warn!(
                    "dnd: SetTimer failed: {}",
                    windows::core::Error::from_thread()
                );
            }
        } else {
            // SAFETY: kills our own timer; failure means it was not running.
            let _ = unsafe { KillTimer(Some(hwnd), KEEP_TIMER) };
        }
    }

    /// A shield kept with no drag from the Mac running goes once Windows
    /// sees the button up, or after `KEEP_LIMIT`.
    fn check_kept(&mut self) {
        let since = *self.kept_since.get_or_insert_with(Instant::now);
        self.modes.held = left_button_down();
        if self.modes.held && since.elapsed() < KEEP_LIMIT {
            return;
        }
        debug!(held = self.modes.held, "dnd: kept shield let go");
        self.modes.release_kept();
        self.apply();
    }
}

impl Drop for InState {
    fn drop(&mut self) {
        // A target still waiting for bytes gets E_FAIL, not a hang.
        source::cancel_live(&self.live, None);
        self.shared.shield.store(0, Ordering::Release);
        // SAFETY: our own timer and window, on their thread, destroyed once.
        unsafe {
            let _ = KillTimer(Some(self.shield.hwnd), KEEP_TIMER);
            let _ = DestroyWindow(self.shield.hwnd);
        }
    }
}
