//! The plan's `smoke_catcher_logs_offer`, for a person to run on the real
//! main thread (debug builds only):
//! `cargo run -p cursorjump -- --smoke-catcher [posted|manual]`.
//! `tests/smoke_catcher.rs` runs it as an `#[ignore]`d test. Its log says
//! what the strip saw: `Offer` (items and sources), `OfferGone`,
//! `LateDrop`, the perform's verdict, each promise's result, and whether
//! each caught file still exists a few seconds later.
//!
//! - `posted` (default) moves the real cursor and presses the left
//!   button, like the PC would: five seconds to point at a file or image
//!   to drag (a Finder file, a WhatsApp photo, an image in Chrome), then
//!   leave the mouse alone. A fake PC crossing at the left edge, at that
//!   height, drives the real injection thread and catcher: it moves to
//!   the spot, presses, holds still 200 ms, drags back to the left edge
//!   and pushes out through it. The injection thread then carries the
//!   strip's drag, or waits up to `LATE_DRAG_WAIT` for a late one; the
//!   log shows the `Leave` it would send.
//! - `manual` shows the strip on the main display's left edge for two
//!   minutes and takes any drop on it: drag files there with the trackpad.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use cj_core::dnd::{DragId, DragItem};
use cj_core::proto::{MouseButton, Side};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_graphics::{CGDisplayBounds, CGEvent, CGMainDisplayID};
use tracing::{info, warn};

use super::catcher::{Catcher, LATE_DRAG_WAIT};
use super::{MacDndEv, on_main};
use crate::common::dnd_link::Source;
use crate::mac::control::{CatchPort, InjectEv, InjectMsg, Input, NoDrags, spawn_inject_thread};
use crate::mac::inject::Injector;

/// How the smoke drives the drag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CatcherSmoke {
    Posted,
    Manual,
}

impl CatcherSmoke {
    /// None if a word is unknown.
    pub fn parse<'a>(words: impl IntoIterator<Item = &'a str>) -> Option<CatcherSmoke> {
        let mut m = CatcherSmoke::Posted;
        for w in words {
            m = match w {
                "posted" => CatcherSmoke::Posted,
                "manual" => CatcherSmoke::Manual,
                _ => return None,
            };
        }
        Some(m)
    }
}

/// The offers seen, by drag.
type Offers = Arc<Mutex<HashMap<DragId, (Vec<DragItem>, Vec<Source>)>>>;

pub fn run_smoke(mode: CatcherSmoke) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().context("--smoke-catcher must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    info!(?mode, "smoke: the edge catcher");

    let inject: Arc<OnceLock<crossbeam_channel::Sender<InjectMsg>>> = Arc::new(OnceLock::new());
    let offers: Offers = Arc::default();
    let (late_tx, late_rx) = crossbeam_channel::unbounded::<DragId>();
    let (to_inject, seen) = (inject.clone(), offers.clone());
    let catcher = Catcher::new(move |ev| {
        if let Some(msg) = ev.inject_msg()
            && let Some(tx) = to_inject.get()
        {
            let _ = tx.send(msg);
        }
        match ev {
            MacDndEv::Offer {
                drag,
                items,
                sources,
            } => {
                info!(drag, ?items, ?sources, "smoke: Offer");
                seen.lock().unwrap().insert(drag, (items, sources));
            }
            MacDndEv::OfferGone { drag } => info!(drag, "smoke: OfferGone"),
            MacDndEv::LateDrop { drag } => {
                info!(drag, "smoke: LateDrop");
                let _ = late_tx.send(drag);
            }
            other => info!(?other, "smoke: catcher event"),
        }
    });
    let script = match mode {
        CatcherSmoke::Manual => {
            let catcher = catcher.clone();
            Box::new(move || manual(catcher, &late_rx, &offers)) as Box<dyn FnOnce() + Send>
        }
        CatcherSmoke::Posted => {
            let (leave_tx, leave_rx) = crossbeam_channel::unbounded();
            let tx = spawn_inject_thread(
                Injector::new()?,
                NoDrags,
                Some(catcher.clone()),
                Arc::new(AtomicBool::new(true)),
                move |ev| {
                    info!(?ev, "smoke: injection thread event");
                    if let InjectEv::SendLeave { drag, .. } = ev {
                        let _ = leave_tx.send(drag);
                    }
                },
            )?;
            let _ = inject.set(tx.clone());
            let catcher = catcher.clone();
            Box::new(move || posted(&tx, &catcher, &leave_rx, &offers)) as Box<dyn FnOnce() + Send>
        }
    };
    std::thread::Builder::new()
        .name("cj-catcher-smoke".into())
        .spawn(move || {
            script();
            on_main(|_| std::process::exit(0));
        })
        .context("spawn the smoke script")?;
    app.run();
    Ok(())
}

/// Shows the strip and keeps the late window open, so any drop is taken.
fn manual(mut catcher: Catcher, late: &crossbeam_channel::Receiver<DragId>, offers: &Offers) {
    catcher.zone(Some(Side::Left));
    catcher.push_out();
    info!("smoke: drag files onto the strip at the main display's left edge (2 minutes)");
    let end = Instant::now() + Duration::from_secs(120);
    while let Ok(drag) = late.recv_deadline(end) {
        let caught = catcher.end_late_wait(true);
        info!(drag, ?caught, "smoke: the drop was taken");
        report(drag, offers);
        catcher.push_out();
    }
    catcher.end_late_wait(false);
    catcher.zone(None);
    std::thread::sleep(Duration::from_millis(300));
}

/// Moves by (`dx`, 0) in small posted steps.
fn move_by(send: &impl Fn(Input), dx: i64) {
    let sign = dx.signum();
    let mut left = dx.abs();
    while left > 0 {
        let d = left.min(8);
        send(Input::Motion {
            dx: (d * sign) as i16,
            dy: 0,
        });
        left -= d;
        std::thread::sleep(Duration::from_millis(8));
    }
}

fn posted(
    tx: &crossbeam_channel::Sender<InjectMsg>,
    catcher: &Catcher,
    leave: &crossbeam_channel::Receiver<Option<DragId>>,
    offers: &Offers,
) {
    let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
    let send = |input| {
        let _ = tx.send(InjectMsg::Input { sid: 1, input });
    };
    for s in (1..=5).rev() {
        info!("smoke: point at the file or image to drag, then leave the mouse alone: {s}");
        sleep(1000);
    }
    let spot = CGEvent::new(None).map_or((400.0, 400.0), |ev| {
        let p = CGEvent::location(Some(&ev));
        (p.x, p.y)
    });
    let b = CGDisplayBounds(CGMainDisplayID());
    let frac = ((spot.1 - b.origin.y) / (b.size.height - 1.0)).clamp(0.0, 1.0) as f32;
    info!(
        ?spot,
        frac, "smoke: the PC's cursor crosses at the left edge"
    );
    let _ = tx.send(InjectMsg::Begin { sid: 1 });
    send(Input::Enter {
        edge: Side::Left,
        frac,
        drag: None,
    });
    sleep(300);
    let distance = (spot.0 - b.origin.x - 2.0).max(0.0) as i64;
    move_by(&send, distance);
    sleep(300);
    info!("smoke: pressing on the spot, holding still 200 ms");
    send(Input::Button {
        button: MouseButton::Left,
        down: true,
    });
    sleep(200);
    info!("smoke: dragging to the left edge");
    move_by(&send, -distance);
    sleep(100);
    info!(
        offer = ?catcher.offer(),
        "smoke: at the edge; the strip's drag before the release (None: a late drag, or none)"
    );
    info!("smoke: pushing out");
    let pushed = Instant::now();
    move_by(&send, -40);
    match leave.recv_timeout(Duration::from_secs(3)) {
        Ok(drag) => {
            info!(
                ?drag,
                ms = pushed.elapsed().as_millis() as u64,
                late_wait_ms = LATE_DRAG_WAIT.as_millis() as u64,
                "smoke: the Leave the PC would get"
            );
            if let Some(drag) = drag {
                report(drag, offers);
            }
        }
        Err(_) => warn!("smoke: no Leave within 3 s: the push-out was a wall?"),
    }
    // The PC's up, which the Mac ignores by now.
    send(Input::Button {
        button: MouseButton::Left,
        down: false,
    });
    sleep(500);
    let _ = tx.send(InjectMsg::Quit);
    sleep(300);
}

/// Logs what drag `drag` carries once each source resolves (up to 30 s),
/// then whether each file is still there 5 s later.
fn report(drag: DragId, offers: &Offers) {
    let Some((items, sources)) = offers.lock().unwrap().get(&drag).cloned() else {
        warn!(drag, "smoke: no offer for the drag");
        return;
    };
    let end = Instant::now() + Duration::from_secs(30);
    let mut paths: Vec<PathBuf> = Vec::new();
    for (item, source) in items.iter().zip(sources) {
        match source {
            Source::Ready(p) => {
                info!(drag, name = %item.name, path = %p.display(), "smoke: ready");
                paths.push(p);
            }
            Source::Pending(mut rx) => loop {
                let value = rx.borrow_and_update().clone();
                match value {
                    Some(Ok(p)) => {
                        info!(drag, name = %item.name, path = %p.display(), "smoke: promise received");
                        paths.push(p);
                        break;
                    }
                    Some(Err(e)) => {
                        warn!(drag, name = %item.name, ?e, "smoke: promise failed");
                        break;
                    }
                    None if rx.has_changed().is_err() => {
                        warn!(drag, name = %item.name, "smoke: promise dropped unread");
                        break;
                    }
                    None if Instant::now() >= end => {
                        warn!(drag, name = %item.name, "smoke: promise still pending after 30 s");
                        break;
                    }
                    None => std::thread::sleep(Duration::from_millis(50)),
                }
            },
        }
    }
    std::thread::sleep(Duration::from_secs(5));
    for p in paths {
        let size = std::fs::metadata(&p).map(|m| m.len()).ok();
        info!(drag, path = %p.display(), ?size, "smoke: still there 5 s later?");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_words_parse() {
        assert_eq!(CatcherSmoke::parse([]), Some(CatcherSmoke::Posted));
        assert_eq!(CatcherSmoke::parse(["manual"]), Some(CatcherSmoke::Manual));
        assert_eq!(CatcherSmoke::parse(["posted"]), Some(CatcherSmoke::Posted));
        assert_eq!(CatcherSmoke::parse(["sideways"]), None);
    }
}
