//! The plan's `smoke_drag_from_fake_pc`, for a person to run on the real
//! main thread (debug builds only):
//! `cargo run -p cursorjump -- --smoke-drag [auto|promise|hold] [file|dir] [drop|esc|home]`.
//! `tests/smoke_drag.rs` runs it as an `#[ignore]`d test. It moves the
//! real cursor and presses the left button.
//!
//! 1. The payload: a 50 MB file, or (`dir`) a small folder tree, made in
//!    the temp folder.
//! 2. Five seconds to point at the drop target (a WhatsApp chat, a Gmail
//!    compose window, a Finder folder, the desktop); then leave the mouse.
//! 3. A fake PC crossing at the left edge, at the target's height, carries
//!    a drag of it into the real injection thread and drag source; the
//!    fake `Fetch` takes about 10 s.
//! 4. Posted moves drag it towards the target. Then, by the last word:
//!    - `drop` (default): it hovers 1.5 s, then the PC's up: posted at once
//!      (`promise`, or `auto` over Finder/WhatsApp) or held with the
//!      jiggle until the payload is complete (`hold`, or `auto` elsewhere);
//!    - `esc`: halfway, Esc typed on the PC's keyboard cancels the
//!      session; the move goes on, and the PC's up is a plain up;
//!    - `home`: halfway, the drag goes back out through the left edge
//!      (Esc, then the up, then the leave).
//! 5. It exits a few seconds after the session ended.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use cj_core::dnd::{DragId, DragItem, FailReason, ItemKind};
use cj_core::proto::{MouseButton, Side};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_graphics::{CGDisplayBounds, CGEvent, CGMainDisplayID};
use tokio::sync::oneshot;
use tracing::{info, warn};

use super::on_main;
use super::source::{DragSource, KVK_ESCAPE, MacDndEv, copy_tree};
use crate::common::transfer::Dest;
use crate::mac::control::{DragPort, InjectMsg, Input, spawn_inject_thread};
use crate::mac::inject::Injector;

const DRAG: DragId = 0x5EED;
/// The fake fetch takes this many steps of half a second.
const FETCH_STEPS: u32 = 20;

/// How the smoke's release behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Release {
    /// The real rule: at once over Finder, the desktop or WhatsApp, else held.
    Auto,
    /// Always at once: promise-only targets.
    Promise,
    /// Always held until the payload is complete.
    Hold,
}

/// What the fake PC drags.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Payload {
    File,
    Dir,
}

/// How the drag ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ending {
    Drop,
    Esc,
    Home,
}

/// The smoke's options, from the words after `--smoke-drag`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SmokeMode {
    pub release: Release,
    pub payload: Payload,
    pub ending: Ending,
}

impl SmokeMode {
    /// None if a word is unknown.
    pub fn parse<'a>(words: impl IntoIterator<Item = &'a str>) -> Option<SmokeMode> {
        let mut m = SmokeMode {
            release: Release::Auto,
            payload: Payload::File,
            ending: Ending::Drop,
        };
        for w in words {
            match w {
                "auto" => m.release = Release::Auto,
                "promise" => m.release = Release::Promise,
                "hold" => m.release = Release::Hold,
                "file" => m.payload = Payload::File,
                "dir" => m.payload = Payload::Dir,
                "drop" => m.ending = Ending::Drop,
                "esc" => m.ending = Ending::Esc,
                "home" => m.ending = Ending::Home,
                _ => return None,
            }
        }
        Some(m)
    }
}

/// The real source, with the release rule the smoke asks for.
struct SmokeDrags {
    source: DragSource,
    release: Release,
}

impl DragPort for SmokeDrags {
    fn prepare(&mut self, drag: DragId, items: Vec<DragItem>, at: (f64, f64)) -> bool {
        self.source.prepare(drag, items, at)
    }
    fn released(&mut self, drag: DragId, at: (f64, f64)) {
        self.source.released(drag, at);
    }
    fn may_release(&mut self, drag: DragId, at: (f64, f64)) -> bool {
        let may = match self.release {
            Release::Auto => self.source.may_release(drag, at),
            Release::Promise => true,
            Release::Hold => self.source.all_complete(drag),
        };
        info!(?self.release, may, "smoke: may the up go out now?");
        may
    }
    fn cancel(&mut self, drag: DragId) {
        self.source.cancel(drag);
    }
}

pub fn run_smoke(mode: SmokeMode) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().context("--smoke-drag must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let payload = make_payload(mode.payload).context("making the payload")?;
    info!(?mode, payload = %payload.display(), "smoke: drag from a fake PC");

    let inject: Arc<OnceLock<crossbeam_channel::Sender<InjectMsg>>> = Arc::new(OnceLock::new());
    let (ended_tx, ended_rx) = crossbeam_channel::unbounded::<bool>();
    let (to_inject, src) = (inject.clone(), payload.clone());
    let source = DragSource::new(move |ev| {
        if let Some(msg) = ev.inject_msg()
            && let Some(tx) = to_inject.get()
        {
            let _ = tx.send(msg);
        }
        match ev {
            MacDndEv::Fetch {
                drag,
                item,
                dest,
                reply,
            } => {
                info!(drag, item, ?dest, "smoke: fetch (fake, about 10 s)");
                fake_fetch(src.clone(), dest, reply);
            }
            MacDndEv::Ended { drag, dropped } => {
                info!(drag, dropped, "smoke: ended");
                let _ = ended_tx.send(dropped);
            }
            other => info!(?other, "smoke: source event"),
        }
    });
    let drags = SmokeDrags {
        source,
        release: mode.release,
    };
    let tx = spawn_inject_thread(
        Injector::new()?,
        drags,
        Arc::new(AtomicBool::new(true)),
        |ev| info!(?ev, "smoke: injection thread event"),
    )?;
    let _ = inject.set(tx.clone());
    let item = DragItem {
        name: payload
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: match mode.payload {
            Payload::File => ItemKind::File,
            Payload::Dir => ItemKind::Dir,
        },
        size: match mode.payload {
            Payload::File => Some(fs::metadata(&payload)?.len()),
            Payload::Dir => None,
        },
    };
    std::thread::Builder::new()
        .name("cj-drag-smoke".into())
        .spawn(move || {
            script(&tx, &ended_rx, item, mode.ending);
            on_main(|_| std::process::exit(0));
        })
        .context("spawn the smoke script")?;
    app.run();
    Ok(())
}

fn make_payload(kind: Payload) -> io::Result<PathBuf> {
    use std::io::Write;

    let dir = std::env::temp_dir().join("cursorjump-smoke-drag");
    fs::create_dir_all(&dir)?;
    if kind == Payload::Dir {
        let root = dir.join("Smoke Folder");
        fs::create_dir_all(root.join("Photos").join("2026"))?;
        fs::write(root.join("notes.txt"), b"a folder dragged from the PC\n")?;
        fs::write(root.join("Photos").join("a.txt"), vec![b'a'; 200_000])?;
        fs::write(root.join("Photos").join("2026").join("b.txt"), b"b")?;
        return Ok(root);
    }
    let path = dir.join("smoke-50MB.bin");
    if fs::metadata(&path).is_ok_and(|m| m.len() == 50 << 20) {
        return Ok(path);
    }
    let mut f = fs::File::create(&path)?;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut buf = vec![0u8; 1 << 20];
    for _ in 0..50 {
        for chunk in buf.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
        f.write_all(&buf)?;
    }
    Ok(path)
}

/// A fake fetch: a file is written in 20 parts over about 10 s; a folder
/// is copied after about 10 s. Stops when the source drops the reply.
fn fake_fetch(src: PathBuf, dest: Dest, reply: oneshot::Sender<Result<PathBuf, FailReason>>) {
    use std::io::Write;

    std::thread::spawn(move || {
        let Dest::InDir(dir) = dest else {
            return;
        };
        let out = dir.join(src.file_name().unwrap_or_default());
        let stopped = || -> io::Result<()> {
            if reply.is_closed() {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Ok(())
            }
        };
        let step = || std::thread::sleep(Duration::from_millis(500));
        let result = (|| -> io::Result<PathBuf> {
            if fs::metadata(&src)?.is_dir() {
                for _ in 0..FETCH_STEPS {
                    stopped()?;
                    step();
                }
                copy_tree(&src, &out)?;
                return Ok(out.clone());
            }
            let data = fs::read(&src)?;
            let mut f = fs::File::create_new(&out)?;
            let part = data.len().div_ceil(FETCH_STEPS as usize).max(1);
            for chunk in data.chunks(part) {
                stopped()?;
                f.write_all(chunk)?;
                step();
            }
            Ok(out.clone())
        })();
        let result = result.map_err(|e| {
            warn!("smoke: the fake fetch stopped: {e}");
            if e.kind() == io::ErrorKind::Interrupted {
                FailReason::Cancelled
            } else {
                FailReason::Unwritable
            }
        });
        let _ = reply.send(result);
    });
}

/// Moves by `dx` (points, either way) in small posted steps.
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

fn script(
    tx: &crossbeam_channel::Sender<InjectMsg>,
    ended: &crossbeam_channel::Receiver<bool>,
    item: DragItem,
    ending: Ending,
) {
    let sleep = |ms| std::thread::sleep(Duration::from_millis(ms));
    let send = |input| {
        let _ = tx.send(InjectMsg::Input { sid: 1, input });
    };
    for s in (1..=5).rev() {
        info!("smoke: point at the drop target, then leave the mouse alone: {s}");
        sleep(1000);
    }
    let target = CGEvent::new(None).map_or((400.0, 400.0), |ev| {
        let p = CGEvent::location(Some(&ev));
        (p.x, p.y)
    });
    let b = CGDisplayBounds(CGMainDisplayID());
    let frac = ((target.1 - b.origin.y) / (b.size.height - 1.0)).clamp(0.0, 1.0) as f32;
    info!(
        ?target,
        frac,
        ?ending,
        "smoke: the PC's cursor and drag cross at the left edge"
    );
    let _ = tx.send(InjectMsg::Begin { sid: 1 });
    send(Input::DragOffer {
        drag: DRAG,
        items: vec![item],
    });
    send(Input::Enter {
        edge: Side::Left,
        frac,
        drag: Some(DRAG),
    });
    sleep(300);
    // Three small steps cross the drag threshold.
    for _ in 0..3 {
        send(Input::Motion { dx: 3, dy: 0 });
        sleep(8);
    }
    let distance = (target.0 - b.origin.x - 11.0).max(0.0) as i64;
    let up = || {
        send(Input::Button {
            button: MouseButton::Left,
            down: false,
        })
    };
    match ending {
        Ending::Drop => {
            move_by(&send, distance);
            info!("smoke: hovering 1.5 s, then the PC's up");
            sleep(1500);
            up();
        }
        Ending::Esc => {
            move_by(&send, distance / 2);
            info!("smoke: Esc typed on the PC, halfway");
            send(Input::Key {
                mac_keycode: KVK_ESCAPE,
                down: true,
            });
            send(Input::Key {
                mac_keycode: KVK_ESCAPE,
                down: false,
            });
            move_by(&send, distance - distance / 2);
            sleep(500);
            info!("smoke: the PC's up (a plain up now)");
            up();
        }
        Ending::Home => {
            move_by(&send, distance / 2);
            sleep(500);
            info!("smoke: going home through the left edge");
            move_by(&send, -(distance / 2 + 40));
            // The PC lets go at home; the Mac has let go already.
            up();
        }
    }
    match ended.recv_timeout(Duration::from_secs(90)) {
        Ok(dropped) => info!(dropped, "smoke: the session ended"),
        Err(_) => warn!("smoke: no end of the session within 90 s"),
    }
    sleep(3000);
    let _ = tx.send(InjectMsg::Quit);
    sleep(300);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_words_parse() {
        assert_eq!(
            SmokeMode::parse([]),
            Some(SmokeMode {
                release: Release::Auto,
                payload: Payload::File,
                ending: Ending::Drop
            })
        );
        assert_eq!(
            SmokeMode::parse(["hold", "dir", "esc"]),
            Some(SmokeMode {
                release: Release::Hold,
                payload: Payload::Dir,
                ending: Ending::Esc
            })
        );
        assert_eq!(
            SmokeMode::parse(["home", "promise"]).unwrap().ending,
            Ending::Home
        );
        assert_eq!(SmokeMode::parse(["sideways"]), None);
    }

    #[test]
    fn the_folder_payload_is_a_small_tree() {
        let root = make_payload(Payload::Dir).unwrap();
        assert!(root.join("Photos").join("2026").join("b.txt").is_file());
        assert!(root.join("notes.txt").is_file());
    }
}
