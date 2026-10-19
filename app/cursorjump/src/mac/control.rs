//! The injection thread (`cj-inject`). It owns the `Injector` and the
//! `MacMachine`, and gets every input-related message in arrival order
//! through one channel, straight from the session readers: input never
//! waits on the app's loop.
//!
//! Input that arrives while the PC is not controlling this Mac is dropped,
//! except the up of a key or button the Injector still holds: a PC key-down
//! can be queued ahead of this Mac's own `Leave`, and the PC then never
//! sends its up, so injecting the down would leave it stuck.

use anyhow::{Context, Result};
use cj_core::mac_state::{MacAction, MacEvent, MacMachine};
use cj_core::proto::{MouseButton, Side};
use tracing::warn;

use super::inject::Injector;
use super::{permissions, power};

/// What the thread needs from the Injector; a trait so the routing can be
/// tested without posting real events.
pub trait Inject {
    /// Whether this process may post events (Accessibility).
    fn can_post(&self) -> bool;
    fn wake_display(&mut self);
    fn warp_to(&mut self, edge: Side, frac: f32);
    /// The fraction along the entry edge when the move pushes out through it.
    fn motion(&mut self, dx: i16, dy: i16) -> Option<f32>;
    fn scroll(&mut self, dx: i32, dy: i32);
    fn button(&mut self, b: MouseButton, down: bool);
    fn key(&mut self, mac_keycode: u16, down: bool);
    fn release_all(&mut self);
    fn buttons_held(&self) -> bool;
    fn button_held(&self, b: MouseButton) -> bool;
    fn key_held(&self, mac_keycode: u16) -> bool;
}

impl Inject for Injector {
    fn can_post(&self) -> bool {
        permissions::has_post_event_access()
    }
    fn wake_display(&mut self) {
        power::wake_display();
    }
    fn warp_to(&mut self, edge: Side, frac: f32) {
        Injector::warp_to(self, edge, frac);
    }
    fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
        Injector::motion(self, dx, dy)
    }
    fn scroll(&mut self, dx: i32, dy: i32) {
        Injector::scroll(self, dx, dy);
    }
    fn button(&mut self, b: MouseButton, down: bool) {
        Injector::button(self, b, down);
    }
    fn key(&mut self, mac_keycode: u16, down: bool) {
        Injector::key(self, mac_keycode, down);
    }
    fn release_all(&mut self) {
        Injector::release_all(self);
    }
    fn buttons_held(&self) -> bool {
        Injector::buttons_held(self)
    }
    fn button_held(&self, b: MouseButton) -> bool {
        Injector::button_held(self, b)
    }
    fn key_held(&self, mac_keycode: u16) -> bool {
        Injector::key_held(self, mac_keycode)
    }
}

/// Input from the PC, as the session readers pass it on.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Enter {
        edge: Side,
        frac: f32,
    },
    Motion {
        dx: i16,
        dy: i16,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Key {
        mac_keycode: u16,
        down: bool,
    },
    /// The PC took the cursor back.
    ReleaseAll,
}

/// What the thread is sent. Input is tagged with its session, so anything
/// a replaced session's readers still deliver is dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectMsg {
    /// Session `sid`'s input follows; a previous session ends first.
    Begin {
        sid: u64,
    },
    Input {
        sid: u64,
        input: Input,
    },
    /// Session `sid` is gone: release everything.
    End {
        sid: u64,
    },
    /// Release everything and stop the thread.
    Quit,
}

/// What the thread reports to the app's loop.
#[derive(Debug, Clone, PartialEq)]
pub enum InjectEv {
    /// Send `Control::Leave { frac }` on session `sid`.
    SendLeave { sid: u64, frac: f32 },
    /// The PC started (true) or stopped controlling this Mac.
    Controlled { sid: u64, on: bool },
    /// The thread has released everything and exited.
    Stopped,
}

/// Routes messages to the Injector through the `MacMachine`.
pub struct Router<I: Inject> {
    inj: I,
    machine: MacMachine,
    /// The session whose input is accepted.
    sid: Option<u64>,
}

impl<I: Inject> Router<I> {
    pub fn new(inj: I) -> Self {
        Router {
            inj,
            machine: MacMachine::new(),
            sid: None,
        }
    }

    /// Handles one message; returns what to report to the loop.
    pub fn handle(&mut self, msg: InjectMsg) -> Vec<InjectEv> {
        match msg {
            InjectMsg::Begin { sid } => {
                let out = self.end_session();
                self.sid = Some(sid);
                out
            }
            InjectMsg::Input { sid, input } if self.sid == Some(sid) => self.input(sid, input),
            InjectMsg::End { sid } if self.sid == Some(sid) => self.end_session(),
            InjectMsg::Input { .. } | InjectMsg::End { .. } => Vec::new(),
            InjectMsg::Quit => {
                let out = self.end_session();
                self.inj.release_all();
                out
            }
        }
    }

    fn end_session(&mut self) -> Vec<InjectEv> {
        match self.sid.take() {
            Some(sid) => self.feed(sid, MacEvent::Disconnected),
            None => Vec::new(),
        }
    }

    fn input(&mut self, sid: u64, input: Input) -> Vec<InjectEv> {
        let controlled = self.machine.controlled();
        match input {
            Input::Enter { edge, frac } => {
                let allowed = self.inj.can_post();
                let mut out = self.feed(sid, MacEvent::Enter { edge, frac });
                if !allowed {
                    // Never strand the PC's cursor on a Mac that cannot be controlled.
                    warn!("no Accessibility permission: sending the cursor back to the PC");
                    out.extend(self.feed(
                        sid,
                        MacEvent::PushOut {
                            frac,
                            buttons_held: false,
                        },
                    ));
                }
                out
            }
            Input::Motion { dx, dy } if controlled => match self.inj.motion(dx, dy) {
                Some(frac) => {
                    let buttons_held = self.inj.buttons_held();
                    self.feed(sid, MacEvent::PushOut { frac, buttons_held })
                }
                None => Vec::new(),
            },
            Input::Scroll { dx, dy } if controlled => {
                self.inj.scroll(dx, dy);
                Vec::new()
            }
            Input::Button { button, down } => {
                if controlled || (!down && self.inj.button_held(button)) {
                    self.inj.button(button, down);
                }
                Vec::new()
            }
            Input::Key { mac_keycode, down } => {
                if controlled || (!down && self.inj.key_held(mac_keycode)) {
                    self.inj.key(mac_keycode, down);
                }
                Vec::new()
            }
            Input::ReleaseAll => self.feed(sid, MacEvent::ReleaseAll),
            Input::Motion { .. } | Input::Scroll { .. } => Vec::new(),
        }
    }

    /// Feeds the machine and carries out its actions.
    fn feed(&mut self, sid: u64, ev: MacEvent) -> Vec<InjectEv> {
        let mut out = Vec::new();
        for action in self.machine.on(ev) {
            match action {
                MacAction::WakeDisplay => self.inj.wake_display(),
                MacAction::WarpTo { edge, frac } => self.inj.warp_to(edge, frac),
                MacAction::SendLeave { frac } => out.push(InjectEv::SendLeave { sid, frac }),
                MacAction::ReleaseAllInjected => self.inj.release_all(),
                MacAction::StartControl => out.push(InjectEv::Controlled { sid, on: true }),
                MacAction::StopControl => out.push(InjectEv::Controlled { sid, on: false }),
            }
        }
        out
    }
}

/// Starts the injection thread; `report` passes its events to the app's loop.
pub fn spawn_inject_thread<I: Inject + Send + 'static>(
    inj: I,
    report: impl Fn(InjectEv) + Send + 'static,
) -> Result<crossbeam_channel::Sender<InjectMsg>> {
    let (tx, rx) = crossbeam_channel::unbounded::<InjectMsg>();
    std::thread::Builder::new()
        .name("cj-inject".into())
        .spawn(move || {
            set_user_interactive_qos();
            let mut router = Router::new(inj);
            for msg in rx {
                let quit = msg == InjectMsg::Quit;
                for ev in router.handle(msg) {
                    report(ev);
                }
                if quit {
                    break;
                }
            }
            report(InjectEv::Stopped);
        })
        .context("starting the injection thread")?;
    Ok(tx)
}

/// Runs this thread at the highest QoS class, so input is never delayed
/// behind background work.
fn set_user_interactive_qos() {
    // SAFETY: plain call that only changes the calling thread's QoS.
    let err = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if err != 0 {
        warn!("pthread_set_qos_class_self_np failed: {err}");
    }
}

#[cfg(test)]
pub(super) mod tests {
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
    }

    /// Records every call; holds keys and buttons like the Injector does.
    pub(crate) struct Fake {
        calls: Arc<Mutex<Vec<Call>>>,
        access: bool,
        /// What every `motion` returns.
        push_out: Option<f32>,
        keys: BTreeSet<u16>,
        buttons: Vec<MouseButton>,
    }

    impl Fake {
        pub(crate) fn new() -> (Fake, Arc<Mutex<Vec<Call>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let fake = Fake {
                calls: calls.clone(),
                access: true,
                push_out: None,
                keys: BTreeSet::new(),
                buttons: Vec::new(),
            };
            (fake, calls)
        }

        fn log(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }
    }

    impl Inject for Fake {
        fn can_post(&self) -> bool {
            self.access
        }
        fn wake_display(&mut self) {
            self.log(Call::Wake);
        }
        fn warp_to(&mut self, edge: Side, frac: f32) {
            self.log(Call::Warp(edge, frac));
        }
        fn motion(&mut self, dx: i16, dy: i16) -> Option<f32> {
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
    }

    fn input(sid: u64, input: Input) -> InjectMsg {
        InjectMsg::Input { sid, input }
    }

    fn enter(frac: f32) -> Input {
        Input::Enter {
            edge: Side::Left,
            frac,
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

    /// A router with session 1 begun, and its call log.
    fn begun(fake: Fake) -> Router<Fake> {
        let mut r = Router::new(fake);
        assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
        r
    }

    pub(crate) fn take(calls: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
        std::mem::take(&mut *calls.lock().unwrap())
    }

    #[test]
    fn input_while_not_controlled_is_dropped() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        for i in [
            Input::Motion { dx: 5, dy: 5 },
            Input::Scroll { dx: 0, dy: 10 },
            key(0x00, true),
            key(0x00, false),
            button(true),
            button(false),
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert_eq!(take(&calls), vec![]);
    }

    #[test]
    fn enter_wakes_and_warps_then_input_flows_in_order() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        assert_eq!(
            r.handle(input(1, enter(0.4))),
            vec![InjectEv::Controlled { sid: 1, on: true }]
        );
        for i in [
            key(0x00, true),
            key(0x00, false),
            Input::Motion { dx: 3, dy: -2 },
            button(true),
            button(false),
            Input::Scroll { dx: 1, dy: 2 },
        ] {
            assert_eq!(r.handle(input(1, i)), vec![]);
        }
        assert_eq!(
            take(&calls),
            vec![
                Call::Wake,
                Call::Warp(Side::Left, 0.4),
                Call::Key(0x00, true),
                Call::Key(0x00, false),
                Call::Motion(3, -2),
                Call::Button(MouseButton::Left, true),
                Call::Button(MouseButton::Left, false),
                Call::Scroll(1, 2),
            ]
        );
    }

    #[test]
    fn ups_while_not_controlled_only_if_still_held() {
        let (mut fake, calls) = Fake::new();
        fake.keys.insert(0x00);
        fake.buttons.push(MouseButton::Right);
        let mut r = begun(fake);
        let right = |down| Input::Button {
            button: MouseButton::Right,
            down,
        };
        for i in [
            key(0x00, false),
            key(0x01, false),
            key(0x02, true),
            right(false),
            button(false),
            right(true),
        ] {
            r.handle(input(1, i));
        }
        assert_eq!(
            take(&calls),
            vec![
                Call::Key(0x00, false),
                Call::Button(MouseButton::Right, false)
            ]
        );
    }

    #[test]
    fn push_out_releases_and_sends_leave() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        take(&calls);
        assert_eq!(
            r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
            vec![
                InjectEv::SendLeave { sid: 1, frac: 0.3 },
                InjectEv::Controlled { sid: 1, on: false }
            ]
        );
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        assert_eq!(take(&calls), vec![], "no input after leaving");
    }

    #[test]
    fn push_out_with_a_button_held_is_a_wall() {
        let (mut fake, calls) = Fake::new();
        fake.push_out = Some(0.3);
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, button(true)));
        take(&calls);
        assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
        r.handle(input(1, button(false)));
        assert_eq!(
            take(&calls),
            vec![Call::Motion(-9, 0), Call::Button(MouseButton::Left, false)],
            "still controlled: the drag goes on"
        );
    }

    #[test]
    fn release_all_from_the_pc_releases_and_stops_control() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x37, true)));
        take(&calls);
        assert_eq!(
            r.handle(input(1, Input::ReleaseAll)),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        r.handle(input(1, Input::Motion { dx: 1, dy: 1 }));
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
    }

    #[test]
    fn end_of_the_session_releases_everything() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x00, true)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::End { sid: 1 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
        assert_eq!(r.handle(input(1, enter(0.5))), vec![], "session is over");
        assert_eq!(take(&calls), vec![]);
    }

    #[test]
    fn a_new_session_ends_the_old_one_and_drops_its_input() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::Begin { sid: 2 }),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        assert_eq!(take(&calls), vec![Call::ReleaseAll]);
        r.handle(input(1, Input::Motion { dx: 1, dy: 1 }));
        r.handle(input(1, enter(0.5)));
        r.handle(InjectMsg::End { sid: 1 });
        assert_eq!(
            take(&calls),
            vec![],
            "the old session's leftovers do nothing"
        );
        assert_eq!(
            r.handle(input(2, enter(0.2))),
            vec![InjectEv::Controlled { sid: 2, on: true }]
        );
        assert_eq!(take(&calls), vec![Call::Wake, Call::Warp(Side::Left, 0.2)]);
    }

    #[test]
    fn without_accessibility_enter_leaves_at_once() {
        let (mut fake, calls) = Fake::new();
        fake.access = false;
        let mut r = begun(fake);
        assert_eq!(
            r.handle(input(1, enter(0.7))),
            vec![
                InjectEv::Controlled { sid: 1, on: true },
                InjectEv::SendLeave { sid: 1, frac: 0.7 },
                InjectEv::Controlled { sid: 1, on: false },
            ]
        );
        r.handle(input(1, key(0x00, true)));
        assert_eq!(
            take(&calls),
            vec![Call::Wake, Call::Warp(Side::Left, 0.7), Call::ReleaseAll]
        );
    }

    #[test]
    fn quit_releases_everything() {
        let (fake, calls) = Fake::new();
        let mut r = begun(fake);
        r.handle(input(1, enter(0.5)));
        r.handle(input(1, key(0x00, true)));
        take(&calls);
        assert_eq!(
            r.handle(InjectMsg::Quit),
            vec![InjectEv::Controlled { sid: 1, on: false }]
        );
        let calls = take(&calls);
        assert!(!calls.is_empty() && calls.iter().all(|c| *c == Call::ReleaseAll));
    }

    #[test]
    fn thread_injects_in_order_and_reports_until_quit() {
        let (fake, calls) = Fake::new();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let tx = spawn_inject_thread(fake, move |ev| {
            let _ = ev_tx.send(ev);
        })
        .unwrap();
        for msg in [
            InjectMsg::Begin { sid: 7 },
            input(7, enter(0.5)),
            input(7, key(0x00, true)),
            input(7, Input::Motion { dx: 1, dy: 0 }),
            input(7, key(0x00, false)),
            InjectMsg::Quit,
        ] {
            tx.send(msg).unwrap();
        }
        let t = Duration::from_secs(2);
        assert_eq!(
            ev_rx.recv_timeout(t).unwrap(),
            InjectEv::Controlled { sid: 7, on: true }
        );
        assert_eq!(
            ev_rx.recv_timeout(t).unwrap(),
            InjectEv::Controlled { sid: 7, on: false }
        );
        assert_eq!(ev_rx.recv_timeout(t).unwrap(), InjectEv::Stopped);
        let calls = take(&calls);
        assert_eq!(
            calls[..5],
            [
                Call::Wake,
                Call::Warp(Side::Left, 0.5),
                Call::Key(0x00, true),
                Call::Motion(1, 0),
                Call::Key(0x00, false),
            ]
        );
        assert!(calls[5..].iter().all(|c| *c == Call::ReleaseAll));
    }
}
