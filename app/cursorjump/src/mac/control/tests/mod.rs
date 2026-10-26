//! The routing's tests: fakes for the Injector, the drag source and the
//! catcher that record every call, and helpers shared by the test files.

use super::*;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Call {
    Wake,
    Warp(Side, f32),
    Motion(i16, i16),
    Scroll(i32, i32),
    Button(MouseButton, bool),
    Key(u16, bool),
    ReleaseAll,
    Jiggle(bool),
    // The drag source's calls.
    Prepare(DragId, usize),
    Released(DragId),
    MayRelease(DragId),
    CancelSource(DragId),
    // The catcher's calls.
    Zone(Option<Side>),
    Pressed,
    CatchPushOut,
    AwaitLate,
    EndLateWait(bool),
}

/// Records every call; holds keys and buttons like the Injector does.
pub(crate) struct Fake {
    calls: Arc<Mutex<Vec<Call>>>,
    /// `key` panics, like a bug in the Injector would.
    key_panics: bool,
    /// `motion` panics.
    motion_panics: bool,
    /// What every `motion` returns.
    push_out: Option<f32>,
    /// What `entry_frac` returns: where the cursor is along the entry edge.
    at_frac: f32,
    keys: BTreeSet<u16>,
    buttons: Vec<MouseButton>,
    /// Where the cursor is, in global points.
    pos: (f64, f64),
}

impl Fake {
    pub(crate) fn new() -> (Fake, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            calls: calls.clone(),
            key_panics: false,
            motion_panics: false,
            push_out: None,
            at_frac: 0.5,
            keys: BTreeSet::new(),
            buttons: Vec::new(),
            pos: (300.0, 200.0),
        };
        (fake, calls)
    }

    fn log(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

impl Inject for Fake {
    fn wake_display(&mut self) {
        self.log(Call::Wake);
    }
    fn warp_to(&mut self, edge: Side, frac: f32) {
        self.log(Call::Warp(edge, frac));
    }
    fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
        assert!(!self.motion_panics, "injector bug");
        self.log(Call::Motion(dx, dy));
        self.push_out
    }
    fn scroll(&mut self, dx: i32, dy: i32) {
        self.log(Call::Scroll(dx, dy));
    }
    fn button(&mut self, b: MouseButton, down: bool) {
        self.buttons.retain(|&x| x != b);
        if down {
            self.buttons.push(b);
        }
        self.log(Call::Button(b, down));
    }
    fn key(&mut self, kc: u16, down: bool) {
        assert!(!self.key_panics, "injector bug");
        if down {
            self.keys.insert(kc);
        } else {
            self.keys.remove(&kc);
        }
        self.log(Call::Key(kc, down));
    }
    fn release_all(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.log(Call::ReleaseAll);
    }
    fn buttons_held(&self) -> bool {
        !self.buttons.is_empty()
    }
    fn button_held(&self, b: MouseButton) -> bool {
        self.buttons.contains(&b)
    }
    fn key_held(&self, kc: u16) -> bool {
        self.keys.contains(&kc)
    }
    fn entry_frac(&self) -> f32 {
        self.at_frac
    }
    fn cursor(&mut self) -> (f64, f64) {
        self.pos
    }
    fn pos(&self) -> (f64, f64) {
        self.pos
    }
    fn bounds(&self) -> Rectf {
        Rectf {
            x: 0.0,
            y: 0.0,
            w: 1512.0,
            h: 982.0,
        }
    }
    fn jiggle(&mut self, nudge: bool) {
        self.log(Call::Jiggle(nudge));
    }
}

/// A drag source that records its calls in the Fake's list.
pub(crate) struct FakeDrags {
    calls: Arc<Mutex<Vec<Call>>>,
    prepare_ok: bool,
    may_release: bool,
    /// What `cancel` says: whether a session is live.
    session: bool,
    /// The last crossing token handed out.
    crossings: Crossing,
}

impl FakeDrags {
    pub(crate) fn new(calls: &Arc<Mutex<Vec<Call>>>) -> FakeDrags {
        FakeDrags {
            calls: calls.clone(),
            prepare_ok: true,
            may_release: false,
            session: true,
            crossings: 0,
        }
    }

    fn log(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

impl DragPort for FakeDrags {
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> Option<Crossing> {
        assert_eq!(at, (300.0, 200.0), "under the cursor");
        self.log(Call::Prepare(drag, items.len()));
        self.crossings += 1;
        self.prepare_ok.then_some(self.crossings)
    }
    fn released(&mut self, drag: DragId, at: (f64, f64)) {
        assert_eq!(at, (300.0, 200.0));
        self.log(Call::Released(drag));
    }
    fn may_release(&mut self, drag: DragId, _at: (f64, f64)) -> bool {
        self.log(Call::MayRelease(drag));
        self.may_release
    }
    fn cancel(&mut self, drag: DragId) -> bool {
        self.log(Call::CancelSource(drag));
        self.session
    }
    fn session_live(&mut self, _drag: DragId, _crossing: Crossing) -> bool {
        self.session
    }
}

fn input(sid: u64, input: Input) -> InjectMsg {
    InjectMsg::Input { sid, input }
}

fn enter(frac: f32) -> Input {
    Input::Enter {
        edge: Side::Left,
        frac,
        drag: None,
    }
}

fn key(kc: u16, down: bool) -> Input {
    Input::Key {
        mac_keycode: kc,
        down,
    }
}

fn button(down: bool) -> Input {
    Input::Button {
        button: MouseButton::Left,
        down,
    }
}

fn granted() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(true))
}

/// A router with session 1 begun, with or without Accessibility.
fn begun_with(fake: Fake, access: Arc<AtomicBool>) -> Router<Fake, NoDrags> {
    let mut r = Router::new(fake, NoDrags, access);
    assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
    r
}

/// A router with session 1 begun.
fn begun(fake: Fake) -> Router<Fake, NoDrags> {
    begun_with(fake, granted())
}

pub(crate) fn take(calls: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
    std::mem::take(&mut *calls.lock().unwrap())
}

// ------------------------------------------------ drags from the PC

use cj_core::dnd::ItemKind;

const LEFT_DOWN: Call = Call::Button(MouseButton::Left, true);
const LEFT_UP: Call = Call::Button(MouseButton::Left, false);
const ESC_DOWN: Call = Call::Key(KVK_ESCAPE, true);
const ESC_UP: Call = Call::Key(KVK_ESCAPE, false);

fn items(n: usize) -> Vec<DragItem> {
    (0..n)
        .map(|i| DragItem {
            name: format!("f{i}.jpg"),
            kind: ItemKind::File,
            size: Some(10),
        })
        .collect()
}

fn enter_with(drag: DragId) -> Input {
    Input::Enter {
        edge: Side::Left,
        frac: 0.5,
        drag: Some(drag),
    }
}

/// Session 1 begun on a fake drag source set up by `setup`.
fn begun_drags(
    fake: Fake,
    calls: &Arc<Mutex<Vec<Call>>>,
    setup: impl FnOnce(&mut FakeDrags),
) -> Router<Fake, FakeDrags> {
    let mut drags = FakeDrags::new(calls);
    setup(&mut drags);
    let mut r = Router::new(fake, drags, granted());
    assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
    r
}

/// Drag 7 (two items) riding on the Mac, its source saying `may_release`
/// at the release; the calls so far are taken.
fn riding(may_release: bool) -> (Router<Fake, FakeDrags>, Arc<Mutex<Vec<Call>>>) {
    let (mut fake, calls) = Fake::new();
    fake.push_out = Some(0.3);
    let mut r = begun_drags(fake, &calls, |d| d.may_release = may_release);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(2),
        },
    ));
    r.handle(input(1, enter_with(7)));
    take(&calls);
    (r, calls)
}

/// Drag 7 let go of while incomplete: the up is held.
fn holding() -> (Router<Fake, FakeDrags>, Arc<Mutex<Vec<Call>>>) {
    let (mut r, calls) = riding(false);
    r.handle(input(1, button(false)));
    assert!(r.next_tick().is_some(), "jiggling");
    take(&calls);
    (r, calls)
}

mod catch;
mod ride;
mod routing;
