use super::*;

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
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.3,
                drag: None
            },
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
    let (fake, calls) = Fake::new();
    let mut r = begun_with(fake, Arc::new(AtomicBool::new(false)));
    assert_eq!(
        r.handle(input(1, enter(0.7))),
        vec![
            InjectEv::Controlled { sid: 1, on: true },
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.7,
                drag: None
            },
            InjectEv::Controlled { sid: 1, on: false },
        ]
    );
    r.handle(input(1, key(0x00, true)));
    assert_eq!(
        take(&calls),
        vec![Call::ReleaseAll],
        "no wake and no warp: the Mac's cursor does not jump"
    );
}

#[test]
fn enter_follows_the_cached_access_state() {
    let (fake, calls) = Fake::new();
    let access = Arc::new(AtomicBool::new(false));
    let mut r = begun_with(fake, access.clone());
    let bounced = r.handle(input(1, enter(0.7)));
    assert!(bounced.contains(&InjectEv::SendLeave {
        sid: 1,
        frac: 0.7,
        drag: None
    }));
    access.store(true, Ordering::Relaxed);
    take(&calls);
    assert_eq!(
        r.handle(input(1, enter(0.6))),
        vec![InjectEv::Controlled { sid: 1, on: true }],
        "granted since: the PC keeps the cursor here"
    );
}

#[test]
fn losing_access_while_controlled_sends_the_cursor_back() {
    let (mut fake, calls) = Fake::new();
    fake.at_frac = 0.8;
    let access = granted();
    let mut r = begun_with(fake, access.clone());
    r.handle(input(1, enter(0.3)));
    r.handle(input(1, key(0x00, true)));
    take(&calls);
    access.store(false, Ordering::Relaxed);
    assert_eq!(
        r.handle(InjectMsg::AccessLost),
        vec![
            // Where the cursor is now along the entry edge, not where it came in.
            InjectEv::SendLeave {
                sid: 1,
                frac: 0.8,
                drag: None
            },
            InjectEv::Controlled { sid: 1, on: false },
        ]
    );
    assert_eq!(take(&calls), vec![Call::ReleaseAll]);
    assert_eq!(r.handle(InjectMsg::AccessLost), vec![], "already gone");
}

#[test]
fn log_limit_lets_one_line_through_per_period_and_counts_the_rest() {
    let t0 = std::time::Instant::now();
    let mut l = LogLimit::new(Duration::from_secs(10));
    assert_eq!(l.allow(t0), Some(0), "the first line goes through");
    assert_eq!(l.allow(t0 + Duration::from_secs(1)), None);
    assert_eq!(l.allow(t0 + Duration::from_millis(9_999)), None);
    assert_eq!(
        l.allow(t0 + Duration::from_secs(10)),
        Some(2),
        "after the period, with the count of lines held back"
    );
    assert_eq!(l.allow(t0 + Duration::from_secs(11)), None);
    assert_eq!(l.allow(t0 + Duration::from_secs(25)), Some(1));
    assert_eq!(l.allow(t0 + Duration::from_secs(40)), Some(0));
}

#[test]
fn losing_access_while_not_controlled_does_nothing() {
    let (fake, calls) = Fake::new();
    let mut r = begun(fake);
    assert_eq!(r.handle(InjectMsg::AccessLost), vec![]);
    assert_eq!(take(&calls), vec![]);
}

#[test]
fn update_access_tells_the_thread_only_when_access_goes() {
    let access = AtomicBool::new(true);
    let (tx, rx) = crossbeam_channel::unbounded();
    assert!(!update_access(&access, true, &tx), "unchanged");
    assert!(update_access(&access, false, &tx));
    assert!(!access.load(Ordering::Relaxed));
    assert_eq!(rx.try_recv(), Ok(InjectMsg::AccessLost));
    assert!(!update_access(&access, false, &tx));
    assert!(update_access(&access, true, &tx));
    assert!(access.load(Ordering::Relaxed));
    assert!(
        rx.try_recv().is_err(),
        "nothing to do when access comes back"
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
    let tx = spawn_inject_thread(fake, NoDrags, None::<NoCatcher>, granted(), move |ev| {
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

#[test]
fn a_panic_releases_everything_and_reports_stopped() {
    let (mut fake, calls) = Fake::new();
    fake.key_panics = true;
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
    let tx = spawn_inject_thread(fake, NoDrags, None::<NoCatcher>, granted(), move |ev| {
        let _ = ev_tx.send(ev);
    })
    .unwrap();
    for msg in [
        InjectMsg::Begin { sid: 3 },
        input(3, enter(0.5)),
        input(
            3,
            Input::Button {
                button: MouseButton::Left,
                down: true,
            },
        ),
        input(3, key(0x00, true)),
    ] {
        tx.send(msg).unwrap();
    }
    let t = Duration::from_secs(2);
    assert_eq!(
        ev_rx.recv_timeout(t).unwrap(),
        InjectEv::Controlled { sid: 3, on: true }
    );
    assert_eq!(ev_rx.recv_timeout(t).unwrap(), InjectEv::Stopped);
    assert_eq!(
        take(&calls).last(),
        Some(&Call::ReleaseAll),
        "the held button is let go"
    );
}
