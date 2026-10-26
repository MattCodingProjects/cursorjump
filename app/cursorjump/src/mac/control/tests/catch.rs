use super::*;

// ------------------------------------------------ drags leaving the Mac

const RIGHT_DOWN: Call = Call::Button(MouseButton::Right, true);
const RIGHT_UP: Call = Call::Button(MouseButton::Right, false);
/// A short late wait, so the thread tests run fast.
const WAIT: Duration = Duration::from_millis(40);

/// A catcher that records its calls in the Fake's list.
pub(crate) struct FakeCatcher {
    calls: Arc<Mutex<Vec<Call>>>,
    /// The drag on the strip: what `push_out` carries.
    offer: Option<DragId>,
    /// What dropped during the late wait: what `end_late_wait` gives.
    caught: Arc<Mutex<Option<DragId>>>,
    /// What `drag_began` says: the drag pasteboard changed (R48a).
    began: bool,
}

impl CatchPort for FakeCatcher {
    fn zone(&mut self, show: Option<Side>) {
        self.calls.lock().unwrap().push(Call::Zone(show));
    }
    fn pressed(&mut self) {
        self.calls.lock().unwrap().push(Call::Pressed);
    }
    fn drag_began(&mut self) -> bool {
        self.began
    }
    fn push_out(&mut self) -> Option<DragId> {
        self.calls.lock().unwrap().push(Call::CatchPushOut);
        self.offer
    }
    fn await_late(&mut self) {
        self.calls.lock().unwrap().push(Call::AwaitLate);
    }
    fn end_late_wait(&mut self, carry: bool) -> Option<DragId> {
        self.calls.lock().unwrap().push(Call::EndLateWait(carry));
        self.caught.lock().unwrap().take()
    }
}

type Catching = Router<Fake, FakeDrags, FakeCatcher>;
/// A router with a fake catcher, the call list, and the catcher's
/// `caught` slot.
type CatchSetup = (Catching, Arc<Mutex<Vec<Call>>>, Arc<Mutex<Option<DragId>>>);

/// Session 1 begun with a fake drag source and catcher; the cursor is
/// near the left edge and pushes out at 0.3. A drag began since the
/// press (R48a). Returns the catcher's `caught` slot.
fn begun_catching(offer: Option<DragId>) -> CatchSetup {
    begun_catching_with(offer, true)
}

/// Like `begun_catching`; `began`: whether a drag began since the
/// press (the drag pasteboard changed).
fn begun_catching_with(offer: Option<DragId>, began: bool) -> CatchSetup {
    let (mut fake, calls) = Fake::new();
    fake.push_out = Some(0.3);
    fake.pos = (10.0, 200.0);
    let caught = Arc::new(Mutex::new(None));
    let catcher = FakeCatcher {
        calls: calls.clone(),
        offer,
        caught: caught.clone(),
        began,
    };
    let drags = FakeDrags::new(&calls);
    let mut r = Router::with_catcher(fake, drags, Some(catcher), granted()).with_late_wait(WAIT);
    assert_eq!(r.handle(InjectMsg::Begin { sid: 1 }), vec![]);
    (r, calls, caught)
}

/// Controlled, with the user's left button down near the left edge:
/// the strip is up; the calls so far are taken.
fn dragging_near_the_edge(offer: Option<DragId>) -> CatchSetup {
    dragging_near_the_edge_with(offer, true)
}

fn dragging_near_the_edge_with(offer: Option<DragId>, began: bool) -> CatchSetup {
    let (mut r, calls, caught) = begun_catching_with(offer, began);
    r.handle(input(1, enter(0.5)));
    r.handle(input(1, button(true)));
    assert_eq!(take(&calls).last(), Some(&Call::Zone(Some(Side::Left))));
    (r, calls, caught)
}

fn zones(calls: &[Call]) -> Vec<Option<Side>> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Zone(z) => Some(*z),
            _ => None,
        })
        .collect()
}

fn push(r: &mut Catching) -> Vec<InjectEv> {
    r.handle(input(1, Input::Motion { dx: -9, dy: 0 }))
}

#[test]
fn push_out_carries_catcher_offer_only_with_left_alone() {
    // Only Left: the strip's drag goes with the release and the leave.
    let (mut r, calls, _) = dragging_near_the_edge(Some(9));
    assert_eq!(
        push(&mut r),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: Some(9)
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![Call::Motion(-9, 0), Call::CatchPushOut, Call::ReleaseAll],
        "asked just before the release"
    );
    assert!(r.next_tick().is_some(), "the strip lingers");

    // Right held too: a wall, and the catcher is not asked.
    let (mut r, calls, _) = dragging_near_the_edge(Some(9));
    r.handle(input(
        1,
        Input::Button {
            button: MouseButton::Right,
            down: true,
        },
    ));
    assert_eq!(push(&mut r), vec![]);
    let c = take(&calls);
    assert!(!c.contains(&Call::CatchPushOut), "{c:?}");
    assert!(!c.contains(&Call::ReleaseAll), "{c:?}");

    // Nothing held: a plain leave, no offer.
    let (mut r, calls, _) = begun_catching(Some(9));
    r.handle(input(1, enter(0.5)));
    take(&calls);
    assert_eq!(
        push(&mut r),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
    assert_eq!(r.next_tick(), None);
}

#[test]
fn zone_shown_only_while_controlled_with_left_alone_near_the_edge() {
    let (mut r, calls, _) = begun_catching(None);
    let left = |down| input(1, button(down));
    let right = |down| {
        input(
            1,
            Input::Button {
                button: MouseButton::Right,
                down,
            },
        )
    };
    let mv = || input(1, Input::Motion { dx: 1, dy: 0 });
    r.inj.push_out = None;
    // Not controlled: nothing, whatever is pressed.
    r.handle(left(true));
    r.handle(mv());
    assert_eq!(zones(&take(&calls)), vec![]);
    r.handle(input(1, enter(0.5)));
    r.inj.pos = (300.0, 200.0);
    r.handle(left(true));
    r.handle(mv());
    assert_eq!(zones(&take(&calls)), vec![], "far from the edge");
    r.inj.pos = (47.0, 200.0);
    r.handle(mv());
    r.handle(mv());
    assert_eq!(
        zones(&take(&calls)),
        vec![Some(Side::Left)],
        "once, at the change"
    );
    r.handle(right(true));
    assert_eq!(take(&calls), vec![RIGHT_DOWN, Call::Zone(None)]);
    r.handle(right(false));
    assert_eq!(take(&calls), vec![RIGHT_UP, Call::Zone(Some(Side::Left))]);
    r.inj.pos = (49.0, 200.0);
    r.handle(mv());
    assert_eq!(zones(&take(&calls)), vec![None]);
    r.inj.pos = (2.0, 200.0);
    r.handle(mv());
    assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
    r.handle(left(false));
    assert_eq!(zones(&take(&calls)), vec![None], "let go");
    r.handle(left(true));
    assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
    // The PC takes the cursor back; then the session ends.
    r.handle(input(1, Input::ReleaseAll));
    assert_eq!(zones(&take(&calls)), vec![None]);
    r.handle(input(1, enter(0.5)));
    r.handle(left(true));
    assert_eq!(zones(&take(&calls)), vec![Some(Side::Left)]);
    r.handle(InjectMsg::End { sid: 1 });
    assert_eq!(zones(&take(&calls)), vec![None]);
}

#[test]
fn zone_hidden_while_an_incoming_drag_rides() {
    let (mut r, calls, _) = begun_catching(None);
    r.inj.push_out = None;
    r.inj.pos = (300.0, 200.0);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(1),
        },
    ));
    r.handle(input(1, enter_with(7)));
    r.inj.pos = (10.0, 200.0);
    r.handle(input(1, Input::Motion { dx: -1, dy: 0 }));
    assert!(r.inj.button_held(MouseButton::Left), "pressed for the ride");
    assert_eq!(zones(&take(&calls)), vec![]);
    // The ride's session ends (Esc on the PC); the PC still holds the
    // button, which is no drag of the user's here.
    r.handle(InjectMsg::DragEnded {
        drag: 7,
        crossing: 1,
    });
    r.handle(input(1, Input::Motion { dx: -1, dy: 0 }));
    assert_eq!(zones(&take(&calls)), vec![]);
    // Pushing out with it is still a wall.
    r.inj.push_out = Some(0.3);
    assert_eq!(push(&mut r), vec![]);
    assert!(!take(&calls).contains(&Call::CatchPushOut));
}

#[test]
fn late_perform_within_wait_becomes_the_offer() {
    let (mut r, calls, caught) = dragging_near_the_edge(None);
    assert_eq!(
        push(&mut r),
        vec![InjectEv::Controlled { sid: 1, on: false }],
        "no leave yet"
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::Motion(-9, 0),
            Call::CatchPushOut,
            Call::AwaitLate,
            Call::ReleaseAll
        ],
        "the release went out over the strip"
    );
    let due = r.next_tick().unwrap();
    *caught.lock().unwrap() = Some(11);
    assert_eq!(
        r.handle(InjectMsg::LateDrop { drag: 11 }),
        vec![InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: Some(11)
        }]
    );
    assert_eq!(take(&calls), vec![Call::EndLateWait(true)]);
    // The strip stays up for the rest of the wait, then goes.
    assert_eq!(r.next_tick(), Some(due));
    assert_eq!(r.tick(due - Duration::from_millis(1)), vec![]);
    assert_eq!(take(&calls), vec![]);
    assert_eq!(r.tick(due), vec![], "the leave went already");
    assert_eq!(take(&calls), vec![Call::Zone(None)]);
    assert_eq!(r.next_tick(), None);
    // A late message after that is no news.
    assert_eq!(r.handle(InjectMsg::LateDrop { drag: 11 }), vec![]);
}

#[test]
fn no_perform_within_wait_leaves_without_offer() {
    let (mut r, calls, _) = dragging_near_the_edge(None);
    push(&mut r);
    take(&calls);
    let due = r.next_tick().unwrap();
    assert_eq!(r.tick(due - Duration::from_millis(1)), vec![]);
    assert_eq!(take(&calls), vec![], "the strip is still up");
    assert_eq!(
        r.tick(due),
        vec![InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: None
        }]
    );
    assert_eq!(
        take(&calls),
        vec![Call::EndLateWait(true), Call::Zone(None)]
    );
    assert_eq!(r.next_tick(), None);
}

#[test]
fn taking_the_cursor_back_during_the_wait_leaves_nothing_behind() {
    for msg in [
        input(1, Input::ReleaseAll),
        InjectMsg::End { sid: 1 },
        InjectMsg::Begin { sid: 2 },
    ] {
        let (mut r, calls, caught) = dragging_near_the_edge(None);
        push(&mut r);
        take(&calls);
        *caught.lock().unwrap() = Some(11);
        let out = r.handle(msg.clone());
        assert!(
            !out.iter().any(|e| matches!(e, InjectEv::SendLeave { .. })),
            "{msg:?}: {out:?}"
        );
        let c = take(&calls);
        assert!(c.contains(&Call::EndLateWait(false)), "{msg:?}: {c:?}");
        // The strip may linger (a late drag still ends on it, refused),
        // but no leave comes.
        assert_eq!(r.tick(Instant::now() + WAIT * 2), vec![], "{msg:?}");
        let c = take(&calls);
        assert!(!c.contains(&Call::EndLateWait(true)), "{msg:?}: {c:?}");
        assert_eq!(r.next_tick(), None, "{msg:?}");
        assert_eq!(r.zone, None, "{msg:?}");
    }
}

#[test]
fn late_wait_does_not_block_other_input() {
    let (mut r, calls, _) = dragging_near_the_edge(None);
    push(&mut r);
    let due = r.next_tick().unwrap();
    // Input during the wait is handled at once, before the wait ends:
    // dropped, the PC no longer controlling this Mac.
    for i in [
        Input::Motion { dx: 5, dy: 0 },
        key(0x00, true),
        button(true),
        Input::Scroll { dx: 0, dy: 3 },
    ] {
        assert_eq!(r.handle(input(1, i)), vec![]);
    }
    let c = take(&calls);
    assert_eq!(
        c,
        vec![
            Call::Motion(-9, 0),
            Call::CatchPushOut,
            Call::AwaitLate,
            Call::ReleaseAll
        ],
        "nothing injected after the release"
    );
    assert_eq!(r.next_tick(), Some(due), "the leave is still due");
    assert_eq!(
        r.tick(due),
        vec![InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: None
        }]
    );
}

#[test]
fn thread_sends_the_late_leave_after_the_wait_meanwhile_handling_input() {
    let (r, calls, _) = dragging_near_the_edge(None);
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let tx = spawn_router(r, move |ev| {
        let _ = ev_tx.send((Instant::now(), ev));
    })
    .unwrap();
    let t0 = Instant::now();
    tx.send(input(1, Input::Motion { dx: -9, dy: 0 })).unwrap();
    let t = Duration::from_secs(2);
    let (_, ev) = ev_rx.recv_timeout(t).unwrap();
    assert_eq!(
        ev,
        InjectEv::Controlled { sid: 1, on: false },
        "reported first, not after the wait"
    );
    // Messages sent during the wait are handled during it: a cancel of
    // some drag shows in the calls (CancelSource), and must come before
    // the wait's end (EndLateWait).
    tx.send(input(1, key(0x00, true))).unwrap();
    tx.send(InjectMsg::CancelDrag { drag: 3 }).unwrap();
    let (at, ev) = ev_rx.recv_timeout(t).unwrap();
    assert_eq!(
        ev,
        InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: None
        }
    );
    assert!(at - t0 >= WAIT, "the leave waited");
    tx.send(InjectMsg::Quit).unwrap();
    while ev_rx.recv_timeout(t).unwrap().1 != InjectEv::Stopped {}
    let c = take(&calls);
    assert!(!c.contains(&Call::Key(0x00, true)), "{c:?}");
    let pos = |x: &Call| c.iter().position(|y| y == x);
    let (cancel, end) = (pos(&Call::CancelSource(3)), pos(&Call::EndLateWait(true)));
    assert!(
        cancel.is_some() && cancel < end,
        "handled before the wait ended: {c:?}"
    );
    assert_eq!(
        zones(&c),
        vec![None],
        "the strip went at the end of the wait: {c:?}"
    );
}

#[test]
fn without_a_catcher_left_held_at_the_edge_is_still_a_wall() {
    let (mut fake, calls) = Fake::new();
    fake.push_out = Some(0.3);
    fake.pos = (0.0, 200.0);
    let mut r = begun(fake);
    r.handle(input(1, enter(0.5)));
    r.handle(input(1, button(true)));
    take(&calls);
    assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
    assert_eq!(take(&calls), vec![Call::Motion(-9, 0)]);
    assert_eq!(r.next_tick(), None);
}

#[test]
fn the_users_left_down_marks_the_drag_pasteboard() {
    let (mut r, calls, _) = begun_catching(None);
    r.inj.push_out = None;
    r.handle(input(1, button(true)));
    assert_eq!(take(&calls), vec![], "not controlled: nothing pressed");
    r.handle(input(1, enter(0.5)));
    take(&calls);
    r.handle(input(1, button(true)));
    assert_eq!(
        take(&calls),
        vec![Call::Pressed, LEFT_DOWN, Call::Zone(Some(Side::Left))],
        "the baseline is read before the down is posted"
    );
    r.handle(input(1, button(false)));
    r.handle(input(
        1,
        Input::Button {
            button: MouseButton::Right,
            down: true,
        },
    ));
    assert!(!take(&calls).contains(&Call::Pressed), "only the left one");
}

#[test]
fn left_held_push_out_without_drag_evidence_stays_a_wall() {
    // A window drag or a text selection reaching the edge: no drag
    // pasteboard change since the press.
    let (mut r, calls, _) = dragging_near_the_edge_with(None, false);
    assert_eq!(push(&mut r), vec![]);
    assert_eq!(
        take(&calls),
        vec![Call::Motion(-9, 0), Call::CatchPushOut],
        "no release, no late window"
    );
    assert_eq!(r.next_tick(), None, "no wait and no linger");
    assert!(r.machine.controlled(), "still on the Mac");
    assert!(r.inj.button_held(MouseButton::Left));
    // The strip stays up while the drag goes on at the edge, and the
    // drag carries on as before.
    assert_eq!(r.zone, Some(Side::Left));
    r.handle(input(1, button(false)));
    assert_eq!(take(&calls), vec![LEFT_UP, Call::Zone(None)]);
}

#[test]
fn left_held_push_out_with_drag_evidence_waits_for_a_late_drop() {
    let (mut r, calls, caught) = dragging_near_the_edge_with(None, true);
    assert_eq!(
        push(&mut r),
        vec![InjectEv::Controlled { sid: 1, on: false }]
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::Motion(-9, 0),
            Call::CatchPushOut,
            Call::AwaitLate,
            Call::ReleaseAll
        ]
    );
    assert!(r.next_tick().is_some());
    *caught.lock().unwrap() = Some(12);
    assert_eq!(
        r.handle(InjectMsg::LateDrop { drag: 12 }),
        vec![InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: Some(12)
        }]
    );
}

#[test]
fn drag_evidence_without_a_perform_in_time_leaves_without_a_drag() {
    let (mut r, calls, _) = dragging_near_the_edge_with(None, true);
    push(&mut r);
    take(&calls);
    let due = r.next_tick().unwrap();
    assert_eq!(
        r.tick(due),
        vec![InjectEv::SendLeave {
            sid: 1,
            frac: 0.3,
            drag: None
        }]
    );
    assert_eq!(
        take(&calls),
        vec![Call::EndLateWait(true), Call::Zone(None)]
    );
}

#[test]
fn an_offer_on_the_strip_is_evidence_enough() {
    let (mut r, calls, _) = dragging_near_the_edge_with(Some(9), false);
    assert_eq!(
        push(&mut r),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: Some(9)
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![Call::Motion(-9, 0), Call::CatchPushOut, Call::ReleaseAll]
    );
}

#[test]
fn other_buttons_held_at_a_push_out_are_a_wall() {
    for other in [
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        let (mut r, calls, _) = dragging_near_the_edge(Some(9));
        r.handle(input(
            1,
            Input::Button {
                button: other,
                down: true,
            },
        ));
        take(&calls);
        assert_eq!(push(&mut r), vec![], "{other:?}");
        let c = take(&calls);
        assert_eq!(
            c,
            vec![Call::Motion(-9, 0)],
            "{other:?}: no catcher, no release"
        );
        assert_eq!(r.zone, None, "{other:?}: no strip");
    }
}

#[test]
fn every_button_but_left_counts_as_another() {
    use super::super::catch::{BUTTONS, not_left};
    let others: Vec<MouseButton> = BUTTONS.into_iter().filter(|&b| not_left(b)).collect();
    assert_eq!(
        others,
        [
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward
        ]
    );
    assert!(BUTTONS.contains(&MouseButton::Left));
}

#[test]
fn losing_accessibility_hides_the_strip() {
    let (mut r, calls, _) = dragging_near_the_edge(None);
    r.access.store(false, Ordering::Relaxed);
    let out = r.handle(InjectMsg::AccessLost);
    assert!(
        out.iter()
            .any(|e| matches!(e, InjectEv::SendLeave { drag: None, .. })),
        "{out:?}"
    );
    let c = take(&calls);
    assert_eq!(zones(&c), vec![None], "{c:?}");
    assert!(!c.contains(&Call::CatchPushOut), "a bounce carries nothing");
    assert_eq!(r.next_tick(), None, "no wait, no linger");
}

#[test]
fn an_enter_during_the_late_wait_ends_it_cleanly() {
    let (mut r, calls, caught) = dragging_near_the_edge(None);
    push(&mut r);
    take(&calls);
    *caught.lock().unwrap() = Some(11);
    let out = r.handle(input(1, enter(0.5)));
    assert_eq!(out, vec![InjectEv::Controlled { sid: 1, on: true }]);
    let c = take(&calls);
    assert!(
        c.contains(&Call::EndLateWait(false)),
        "the gate closes; what it caught is withdrawn: {c:?}"
    );
    // The linger runs out; no leave ever comes.
    let due = r.next_tick().unwrap();
    assert_eq!(r.tick(due), vec![]);
    assert_eq!(take(&calls), vec![Call::Zone(None)]);
    assert_eq!(r.next_tick(), None);
    assert_eq!(r.handle(InjectMsg::LateDrop { drag: 11 }), vec![]);
}

#[test]
fn a_new_crossing_drops_the_strip_of_an_earlier_push_out() {
    let (mut r, calls, _) = dragging_near_the_edge(Some(9));
    push(&mut r);
    assert!(r.next_tick().is_some(), "lingering");
    take(&calls);
    // The PC's drag rides in at once: no strip while it rides.
    r.inj.pos = (300.0, 200.0);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(1),
        },
    ));
    r.handle(input(1, enter_with(7)));
    let c = take(&calls);
    assert_eq!(zones(&c), vec![None], "{c:?}");
    assert_eq!(r.next_tick(), None);
}
