//! Audio sharing in the Mac app's loop: the rules (`MacAudio`) hear about
//! the connection, the switch, the PC's devices (`Control::AudioStatus`)
//! and what `cj-audio` reports; each change of plan goes to `cj-audio`
//! (`AudioCmd::Apply`), and each change of want to the PC
//! (`Control::AudioWant`), which every new session also gets.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use cj_core::audio::{AudioStatus, AudioWant};
use cj_core::audio_state::{AudioPlan, Capture, MacAudio};
use cj_net::session::Session;
use tracing::{error, info, warn};

use crate::mac::audio::{AudioCmd, MacAudioEv, MacAudioIo, MicTx, recover_input};
use crate::ui::tray::AudioMenu;

/// How long quitting waits for `cj-audio` to give the input back.
const AUDIO_QUIT: Duration = Duration::from_secs(1);

/// From the plan `cj-audio` and the PC have to the next one: whether
/// `cj-audio` needs it (the tap or the mic changed), and whether the PC
/// needs the want.
pub(super) fn audio_updates(prev: &AudioPlan, next: &AudioPlan) -> (bool, bool) {
    let apply = prev.tap != next.tap || prev.mic_default != next.mic_default;
    (apply, prev.want != next.want)
}

/// The Mac's half of audio sharing, as the loop sees it.
pub(super) struct MacSharing {
    rules: MacAudio,
    io: MacAudioIo,
    /// The plan `cj-audio` and the PC have.
    plan: AudioPlan,
    /// A new session: the PC hears the want even if it did not change.
    want_due: bool,
}

impl MacSharing {
    pub fn new(sharing: bool, io: MacAudioIo) -> MacSharing {
        MacSharing {
            rules: MacAudio::new(sharing),
            io,
            // What `cj-audio` starts with: nothing.
            plan: AudioPlan {
                tap: false,
                mic_default: false,
                want: AudioWant::default(),
                note: None,
                recheck_at: None,
            },
            want_due: false,
        }
    }

    /// For the datagram reader: where the PC's mic goes.
    pub fn tx(&self) -> MicTx {
        self.io.tx()
    }

    /// The live session changed to `s`, or ended (None).
    pub fn session(&mut self, s: Option<&Session>, now: Instant) {
        self.io.send(AudioCmd::Session(s.cloned()));
        self.link(s.is_some(), now);
    }

    /// A new session (`up`) or none.
    fn link(&mut self, up: bool, now: Instant) {
        if up {
            self.rules.connected(now);
            self.want_due = true;
        } else {
            self.rules.disconnected(now);
        }
    }

    pub fn set_sharing(&mut self, on: bool) {
        self.rules.set_sharing(on);
    }

    pub fn pc_status(&mut self, s: AudioStatus) {
        info!(?s, "audio: the PC's devices");
        self.rules.set_pc(s);
    }

    pub fn event(&mut self, ev: MacAudioEv) {
        match ev {
            MacAudioEv::Capture(c) => {
                if c == Capture::Denied {
                    info!("audio: the Mac's sound stays on the Mac (capture refused)");
                }
                self.rules.set_capture(c);
            }
            MacAudioEv::Driver(present) => self.rules.set_driver(present),
            MacAudioEv::MicListening(on) => {
                info!(on, "audio: an app records from CursorJump Microphone");
                self.rules.set_mic_listening(on);
            }
        }
    }

    /// Works out the plan as of `now` and sends `cj-audio` a changed one;
    /// returns the want when the PC must hear it.
    pub fn update(&mut self, now: Instant) -> Option<AudioWant> {
        let next = self.rules.plan(now);
        let (apply, want_changed) = audio_updates(&self.plan, &next);
        if apply {
            info!(tap = next.tap, mic = next.mic_default, "audio: plan");
            self.io.send(AudioCmd::Apply(next.clone()));
        }
        if want_changed {
            info!(
                sound = next.want.sound,
                mic = next.want.mic,
                "audio: what the Mac wants from the PC"
            );
        }
        let send = want_changed || std::mem::take(&mut self.want_due);
        self.plan = next;
        send.then_some(self.plan.want)
    }

    /// When the plan changes by itself (a dropped link's grace ends).
    pub fn recheck_at(&self) -> Option<Instant> {
        self.plan.recheck_at
    }

    /// The menu's switch (`on`: the setting) and its note.
    pub fn menu(&self, on: bool) -> AudioMenu {
        AudioMenu {
            on,
            note: self.plan.note.map(String::from),
        }
    }

    /// With the latency log, while the PC's mic plays.
    pub fn log_stats(&self) {
        if self.io.feed_running() {
            info!("{}", self.io.mic_stats().snapshot().log_line("PC mic"));
        }
    }

    /// Asks `cj-audio` to give the input back and stop. The returned
    /// closure blocks: it waits up to [`AUDIO_QUIT`] for `cj-audio`, and
    /// gives the input back itself if no answer comes.
    pub fn quit(&self) -> impl FnOnce() + Send + 'static {
        let (tx, rx) = std::sync::mpsc::channel();
        self.io.send(AudioCmd::Quit(tx));
        let restore = self.io.restore_file();
        move || await_quit(&rx, AUDIO_QUIT, || recover_input(&restore))
    }
}

/// Waits up to `within` for `cj-audio`'s Quit reply; without one (it is
/// wedged, or gone), runs `recover` so the user's input still comes back.
fn await_quit(stopped: &Receiver<()>, within: Duration, recover: impl FnOnce()) {
    match stopped.recv_timeout(within) {
        Ok(()) => return,
        Err(RecvTimeoutError::Timeout) => {
            warn!("audio: cj-audio did not stop in time; giving the input back from here")
        }
        Err(RecvTimeoutError::Disconnected) => {
            error!("audio: cj-audio is gone; giving the input back from here")
        }
    }
    recover();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cj_core::audio::{AudioStatus, DeviceState};
    use cj_core::audio_state::{NOTE_NO_PC_MIC, NOTE_ROUTED};
    use std::cell::Cell;

    fn plan(tap: bool, mic_default: bool, sound: bool, mic: bool) -> AudioPlan {
        AudioPlan {
            tap,
            mic_default,
            want: AudioWant { sound, mic },
            note: None,
            recheck_at: None,
        }
    }

    #[test]
    fn plan_changes_send_apply_and_want_only_on_change() {
        let idle = plan(false, false, false, false);
        assert_eq!(audio_updates(&idle, &idle), (false, false));
        let tap = plan(true, false, true, false);
        assert_eq!(audio_updates(&idle, &tap), (true, true));
        assert_eq!(audio_updates(&tap, &tap), (false, false));
        let both = plan(true, true, true, false);
        assert_eq!(
            audio_updates(&tap, &both),
            (true, false),
            "the mic routed, but no app listens yet"
        );
        let listening = plan(true, true, true, true);
        assert_eq!(
            audio_updates(&both, &listening),
            (false, true),
            "an app listens: only the want changes"
        );
        // In the grace: the devices stay, and nothing is asked of the PC.
        let grace = AudioPlan {
            recheck_at: Some(std::time::Instant::now()),
            ..plan(true, true, false, false)
        };
        assert_eq!(audio_updates(&listening, &grace), (false, true));
        let noted = AudioPlan {
            note: Some(NOTE_ROUTED),
            ..listening.clone()
        };
        assert_eq!(
            audio_updates(&listening, &noted),
            (false, false),
            "the note alone changes nothing on the audio thread"
        );
        let other_note = AudioPlan {
            note: Some(NOTE_NO_PC_MIC),
            ..noted.clone()
        };
        assert_eq!(audio_updates(&noted, &other_note), (false, false));
    }

    #[test]
    fn every_new_session_hears_the_want_even_unchanged() {
        let (cmds, cmds_rx) = crossbeam_channel::unbounded();
        let mut a = MacSharing::new(true, MacAudioIo::detached(cmds));
        let t = Instant::now();
        a.event(MacAudioEv::Driver(true));
        a.link(true, t);
        assert_eq!(a.update(t), Some(AudioWant::default()), "a new session");
        assert_eq!(a.update(t), None, "nothing changed");
        a.pc_status(AudioStatus {
            output: DeviceState::Ok,
            mic: DeviceState::Ok,
        });
        let want = AudioWant {
            sound: true,
            mic: false,
        };
        assert_eq!(a.update(t), Some(want));
        assert!(matches!(
            cmds_rx.try_recv(),
            Ok(AudioCmd::Apply(AudioPlan {
                tap: true,
                mic_default: true,
                ..
            }))
        ));
        // The session is replaced within the grace: the plan stands.
        a.link(false, t);
        a.link(true, t);
        assert_eq!(a.update(t), Some(want), "the new session hears it too");
        assert!(cmds_rx.try_recv().is_err(), "cj-audio has the plan already");
        assert_eq!(a.update(t), None);
    }

    #[test]
    fn quit_gives_the_input_back_itself_without_an_answer() {
        let recovered = Cell::new(0);
        let recover = || recovered.set(recovered.get() + 1);
        let short = Duration::from_millis(10);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(()).unwrap();
        await_quit(&rx, short, recover);
        assert_eq!(recovered.get(), 0, "cj-audio answered");

        let (_tx, rx) = std::sync::mpsc::channel();
        await_quit(&rx, short, recover);
        assert_eq!(recovered.get(), 1, "cj-audio wedged");

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        drop(tx);
        await_quit(&rx, short, recover);
        assert_eq!(recovered.get(), 2, "cj-audio gone");
    }
}
