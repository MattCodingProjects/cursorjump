//! The thread that moves the input thread's output into the link loop, so
//! the blocking source stays off the tokio workers. (The clipboard bridge is
//! `common::clipboard_bridge`.)

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;

use super::app::Ev;
use super::input_thread::InputMsg;

/// Moves input-thread messages into the loop.
pub(super) fn bridge_input(
    rx: crossbeam_channel::Receiver<InputMsg>,
    events: UnboundedSender<Ev>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("cj-input-bridge".into())
        .spawn(move || {
            for msg in rx {
                if events.send(Ev::Input(msg)).is_err() {
                    return;
                }
            }
            let _ = events.send(Ev::InputGone);
        })
        .context("starting the input bridge")?;
    Ok(())
}
