use super::*;

#[test]
fn begin_drag_prepares_then_presses_left() {
    let (fake, calls) = Fake::new();
    let mut r = begun_drags(fake, &calls, |_| {});
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(2),
        },
    ));
    assert_eq!(take(&calls), vec![], "an offer alone does nothing");
    assert_eq!(
        r.handle(input(1, enter_with(7))),
        vec![InjectEv::Controlled { sid: 1, on: true }]
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::Wake,
            Call::Warp(Side::Left, 0.5),
            Call::Prepare(7, 2),
            LEFT_DOWN
        ]
    );
    // Motion drags the session along.
    r.handle(input(1, Input::Motion { dx: 4, dy: 1 }));
    assert_eq!(take(&calls), vec![Call::Motion(4, 1)]);
}

#[test]
fn prepare_failure_presses_nothing() {
    let (mut fake, calls) = Fake::new();
    fake.push_out = Some(0.3);
    let mut r = begun_drags(fake, &calls, |d| d.prepare_ok = false);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(1),
        },
    ));
    r.handle(input(1, enter_with(7)));
    assert_eq!(
        take(&calls),
        vec![Call::Wake, Call::Warp(Side::Left, 0.5), Call::Prepare(7, 1)]
    );
    // The drag is over: the PC's up passes as a plain up, and pushing
    // out is a plain leave, with no Esc.
    r.handle(input(1, button(false)));
    assert_eq!(
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![LEFT_UP, Call::Motion(-9, 0), Call::ReleaseAll]
    );

    // A crossing whose offer never came: no items, so prepare fails.
    r.handle(input(1, enter_with(8)));
    assert_eq!(
        take(&calls),
        vec![Call::Wake, Call::Warp(Side::Left, 0.5), Call::Prepare(8, 0)]
    );
}

#[test]
fn cancel_drag_posts_escape_before_release() {
    let (mut r, calls) = riding(false);
    assert_eq!(
        r.handle(input(1, Input::ReleaseAll)),
        vec![InjectEv::Controlled { sid: 1, on: false }]
    );
    assert_eq!(
        take(&calls),
        vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll]
    );
}

#[test]
fn release_over_a_taker_or_when_complete_drops_at_once() {
    let (mut fake, calls) = Fake::new();
    fake.push_out = Some(0.3);
    let mut r = begun_drags(fake, &calls, |d| d.may_release = true);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(1),
        },
    ));
    r.handle(input(1, enter_with(7)));
    take(&calls);
    r.handle(input(1, button(false)));
    assert_eq!(
        take(&calls),
        vec![Call::Released(7), Call::MayRelease(7), LEFT_UP]
    );
    assert_eq!(r.next_tick(), None);
    // Dropped: the next push out is a plain leave.
    r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
    assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
}

#[test]
fn release_while_incomplete_holds_the_up_and_freezes_motion() {
    let (mut r, calls) = riding(false);
    r.handle(input(1, button(false)));
    assert_eq!(take(&calls), vec![Call::Released(7), Call::MayRelease(7)]);
    // The cursor stays on the drop spot; repeated ups do nothing.
    for i in [
        Input::Motion { dx: -50, dy: 3 },
        Input::Scroll { dx: 0, dy: 5 },
        button(false),
        button(true),
    ] {
        assert_eq!(r.handle(input(1, i)), vec![]);
    }
    assert_eq!(take(&calls), vec![]);
    // Keys still pass (Esc from the PC's keyboard cancels).
    r.handle(input(1, key(0x00, true)));
    assert_eq!(take(&calls), vec![Call::Key(0x00, true)]);

    // The jiggle: due every JIGGLE_EVERY, nudged then back.
    let due = r.next_tick().unwrap();
    r.tick(due - Duration::from_millis(1));
    assert_eq!(take(&calls), vec![], "not yet");
    r.tick(due);
    let next = r.next_tick().unwrap();
    assert_eq!(next, due + JIGGLE_EVERY);
    r.tick(next);
    assert_eq!(take(&calls), vec![Call::Jiggle(true), Call::Jiggle(false)]);
}

#[test]
fn hold_ends_with_the_up_when_complete() {
    let (mut r, calls) = holding();
    let due = r.next_tick().unwrap();
    r.tick(due);
    assert_eq!(
        r.handle(InjectMsg::DragReady {
            drag: 8,
            crossing: 1
        }),
        vec![]
    );
    assert_eq!(take(&calls), vec![Call::Jiggle(true)], "another drag");
    assert_eq!(
        r.handle(InjectMsg::DragReady {
            drag: 7,
            crossing: 1
        }),
        vec![]
    );
    // Back on the drop spot, then the up.
    assert_eq!(take(&calls), vec![Call::Jiggle(false), LEFT_UP]);
    assert_eq!(r.next_tick(), None);
    // Motion flows again, and the session's end changes nothing: the
    // next push out is a plain leave, with no Esc.
    r.handle(InjectMsg::DragEnded {
        drag: 7,
        crossing: 1,
    });
    r.handle(input(1, Input::Motion { dx: 2, dy: 2 }));
    assert_eq!(take(&calls), vec![Call::Motion(2, 2), Call::ReleaseAll]);
}

#[test]
fn cancel_during_hold_posts_escape_then_up() {
    for msg in [
        InjectMsg::CancelDrag { drag: 7 },
        InjectMsg::DragFailed {
            drag: 7,
            crossing: 1,
        },
    ] {
        let (mut r, calls) = holding();
        assert_eq!(r.handle(msg.clone()), vec![]);
        assert_eq!(
            take(&calls),
            vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, LEFT_UP],
            "{msg:?}"
        );
        assert_eq!(r.next_tick(), None);
        // Over: pushing out is a plain leave.
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
        assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
    }
    // A session that ends by itself during a hold (Esc typed on the PC)
    // lets go too.
    let (mut r, calls) = holding();
    r.handle(InjectMsg::DragEnded {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![LEFT_UP]);
    assert_eq!(r.next_tick(), None);
}

#[test]
fn cancel_while_riding_leaves_the_button_to_the_pc() {
    let (mut r, calls) = riding(false);
    r.handle(InjectMsg::DragFailed {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![Call::CancelSource(7), ESC_DOWN, ESC_UP]);
    // The PC's up is its own.
    r.handle(input(1, button(false)));
    assert_eq!(take(&calls), vec![LEFT_UP]);
    // A cancel for a drag no longer riding only stops its fetches.
    r.handle(InjectMsg::CancelDrag { drag: 7 });
    assert_eq!(take(&calls), vec![Call::CancelSource(7)]);
}

#[test]
fn disconnect_during_hold_releases_everything() {
    let (mut r, calls) = holding();
    assert_eq!(
        r.handle(InjectMsg::End { sid: 1 }),
        vec![InjectEv::Controlled { sid: 1, on: false }]
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            LEFT_UP,
            Call::ReleaseAll
        ]
    );
    assert_eq!(r.next_tick(), None);

    // Losing Accessibility during a hold sends the cursor back the same way.
    let (mut r, calls) = holding();
    assert_eq!(
        r.handle(InjectMsg::AccessLost),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.5,
                drag: Some(7)
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            LEFT_UP,
            Call::ReleaseAll
        ]
    );
}

#[test]
fn disconnect_while_riding_cancels_then_releases() {
    let (mut r, calls) = riding(false);
    assert_eq!(
        r.handle(InjectMsg::End { sid: 1 }),
        vec![InjectEv::Controlled { sid: 1, on: false }]
    );
    assert_eq!(
        take(&calls),
        vec![Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll]
    );
    assert_eq!(r.next_tick(), None);
}

#[test]
fn left_counts_as_held_from_begin_drag() {
    let (mut r, calls) = riding(false);
    // Esc on the Mac's side ended the session; the PC still holds the
    // button, so pushing out is a wall.
    r.handle(InjectMsg::DragEnded {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(r.handle(input(1, Input::Motion { dx: -9, dy: 0 })), vec![]);
    assert_eq!(take(&calls), vec![Call::Motion(-9, 0)]);
    // Once the PC lets go, the cursor may leave.
    r.handle(input(1, button(false)));
    assert_eq!(
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![LEFT_UP, Call::Motion(-9, 0), Call::ReleaseAll]
    );
}

#[test]
fn going_home_cancels_the_prefetch() {
    let (mut r, calls) = riding(false);
    assert_eq!(
        r.handle(input(1, Input::Motion { dx: -9, dy: 0 })),
        vec![
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: Some(7)
            },
            InjectEv::Controlled { sid: 1, on: false }
        ]
    );
    assert_eq!(
        take(&calls),
        vec![
            Call::Motion(-9, 0),
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            Call::ReleaseAll
        ]
    );
    assert_eq!(r.next_tick(), None);
}

/// Never a second left-down without an up in between.
fn assert_downs_and_ups_alternate(calls: &[Call]) {
    let mut down = true; // Drag 7's down came first.
    for c in calls {
        match c {
            c if *c == LEFT_DOWN => {
                assert!(!down, "a second down: {calls:?}");
                down = true;
            }
            c if *c == LEFT_UP => down = false,
            _ => {}
        }
    }
}

#[test]
fn a_new_crossing_over_a_riding_drag_cancels_it_first() {
    let (mut r, calls) = riding(false);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 8,
            items: items(3),
        },
    ));
    r.handle(input(1, enter_with(8)));
    let got = take(&calls);
    assert_eq!(
        got,
        vec![
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            Call::Wake,
            Call::Warp(Side::Left, 0.5),
            LEFT_UP,
            Call::Prepare(8, 3),
            LEFT_DOWN
        ]
    );
    assert_downs_and_ups_alternate(&got);
}

#[test]
fn a_new_crossing_over_a_held_drop_lets_go_first() {
    let (mut r, calls) = holding();
    let due = r.next_tick().unwrap();
    r.tick(due);
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 8,
            items: items(1),
        },
    ));
    r.handle(input(1, enter_with(8)));
    let got = take(&calls);
    assert_eq!(
        got,
        vec![
            Call::Jiggle(true),
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            Call::Jiggle(false),
            LEFT_UP,
            Call::Wake,
            Call::Warp(Side::Left, 0.5),
            Call::Prepare(8, 1),
            LEFT_DOWN
        ]
    );
    assert_downs_and_ups_alternate(&got);
    assert_eq!(r.next_tick(), None, "the new drag rides, not held");
}

#[test]
fn a_panic_during_a_ride_cancels_the_drag_then_releases() {
    let (mut fake, calls) = Fake::new();
    fake.motion_panics = true;
    let drags = FakeDrags::new(&calls);
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let tx = spawn_inject_thread(fake, drags, None::<NoCatcher>, granted(), move |ev| {
        let _ = ev_tx.send(ev);
    })
    .unwrap();
    for msg in [
        InjectMsg::Begin { sid: 4 },
        input(
            4,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ),
        input(4, enter_with(7)),
        input(4, Input::Motion { dx: 1, dy: 0 }),
    ] {
        tx.send(msg).unwrap();
    }
    let t = Duration::from_secs(2);
    while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
    let calls = take(&calls);
    assert_eq!(
        calls[calls.len() - 4..],
        [Call::CancelSource(7), ESC_DOWN, ESC_UP, Call::ReleaseAll],
        "{calls:?}"
    );
}

#[test]
fn thread_jiggles_a_held_drop() {
    let (mut fake, calls) = Fake::new();
    fake.push_out = None;
    let drags = FakeDrags::new(&calls);
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let every = Duration::from_millis(5);
    let router = Router::new(fake, drags, granted()).with_jiggle_every(every);
    let tx = spawn_router(router, move |ev| {
        let _ = ev_tx.send(ev);
    })
    .unwrap();
    for msg in [
        InjectMsg::Begin { sid: 2 },
        input(
            2,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ),
        input(2, enter_with(7)),
        input(2, button(false)),
    ] {
        tx.send(msg).unwrap();
    }
    std::thread::sleep(every * 2 + Duration::from_millis(40));
    tx.send(InjectMsg::DragReady {
        drag: 7,
        crossing: 1,
    })
    .unwrap();
    tx.send(InjectMsg::Quit).unwrap();
    let t = Duration::from_secs(2);
    while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
    let calls = take(&calls);
    let jiggles = calls
        .iter()
        .filter(|c| matches!(c, Call::Jiggle(_)))
        .count();
    assert!(jiggles >= 2, "{calls:?}");
    let up = calls.iter().position(|c| *c == LEFT_UP).unwrap();
    assert!(calls[..up].contains(&Call::Jiggle(true)));
    assert_eq!(calls[up - 1], Call::Jiggle(false), "{calls:?}");
}

#[test]
fn a_cancel_posts_no_escape_without_a_live_session() {
    let (mut r, calls) = riding(false);
    r.drags.session = false;
    r.handle(InjectMsg::CancelDrag { drag: 7 });
    assert_eq!(take(&calls), vec![Call::CancelSource(7)], "no stray Esc");
    let (mut r, calls) = holding();
    r.drags.session = false;
    r.handle(InjectMsg::CancelDrag { drag: 7 });
    assert_eq!(take(&calls), vec![Call::CancelSource(7), LEFT_UP]);
    // A session that started late asks for its Esc.
    r.drags.session = true;
    r.handle(InjectMsg::EscapeSession {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![ESC_DOWN, ESC_UP]);
}

#[test]
fn a_late_escape_request_for_an_ended_session_posts_nothing() {
    // Going home: the up ended the session before its Esc request came.
    let (mut r, calls) = riding(false);
    r.drags.session = false;
    r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
    take(&calls);
    r.handle(InjectMsg::EscapeSession {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![], "no Esc for the front-most app");
}

#[test]
fn an_earlier_crossing_never_touches_a_later_ride() {
    let (mut r, calls) = riding(false);
    // Home (crossing 1 cancelled), then back: crossing 2.
    r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
    r.handle(input(
        1,
        Input::DragOffer {
            drag: 7,
            items: items(2),
        },
    ));
    r.handle(input(1, enter_with(7)));
    take(&calls);
    // Crossing 1's late news.
    r.handle(InjectMsg::DragEnded {
        drag: 7,
        crossing: 1,
    });
    r.handle(InjectMsg::DragFailed {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![], "ignored");
    // Crossing 2 still rides: the PC's up is its release.
    r.handle(input(1, button(false)));
    assert_eq!(take(&calls), vec![Call::Released(7), Call::MayRelease(7)]);
    r.handle(InjectMsg::DragReady {
        drag: 7,
        crossing: 1,
    });
    assert_eq!(take(&calls), vec![], "an earlier crossing's Ready");
    r.handle(InjectMsg::DragReady {
        drag: 7,
        crossing: 2,
    });
    assert_eq!(take(&calls), vec![LEFT_UP]);
}

#[test]
fn enter_without_a_drag_over_a_ride_lets_go() {
    let (mut r, calls) = riding(false);
    r.handle(input(1, enter(0.5)));
    let got = take(&calls);
    assert_eq!(
        got,
        vec![
            Call::CancelSource(7),
            ESC_DOWN,
            ESC_UP,
            Call::Wake,
            Call::Warp(Side::Left, 0.5),
            LEFT_UP
        ]
    );
    assert_downs_and_ups_alternate(&got);
    // Nothing stays held: pushing out is a plain leave.
    r.handle(input(1, Input::Motion { dx: -9, dy: 0 }));
    assert_eq!(take(&calls), vec![Call::Motion(-9, 0), Call::ReleaseAll]);
}

#[test]
fn a_panic_while_cancelling_still_releases_everything() {
    let (mut fake, calls) = Fake::new();
    fake.key_panics = true;
    let drags = FakeDrags::new(&calls);
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let tx = spawn_inject_thread(fake, drags, None::<NoCatcher>, granted(), move |ev| {
        let _ = ev_tx.send(ev);
    })
    .unwrap();
    for msg in [
        InjectMsg::Begin { sid: 4 },
        input(
            4,
            Input::DragOffer {
                drag: 7,
                items: items(1),
            },
        ),
        input(4, enter_with(7)),
        // Panics; so does the Esc of the cancel after it.
        input(4, key(0x00, true)),
    ] {
        tx.send(msg).unwrap();
    }
    let t = Duration::from_secs(2);
    while ev_rx.recv_timeout(t).unwrap() != InjectEv::Stopped {}
    let calls = take(&calls);
    assert_eq!(
        calls[calls.len() - 2..],
        [Call::CancelSource(7), Call::ReleaseAll],
        "{calls:?}"
    );
}
