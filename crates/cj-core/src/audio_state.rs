//! Mac audio routing rules: pure and clock-injected, like `mac_state`.
//! Decides when Mac sound goes to the PC and when the PC's mic is the Mac's
//! default input, with a grace period so a Wi-Fi blip changes nothing.

use crate::audio::{AudioStatus, AudioWant, DeviceState};
use std::time::{Duration, Instant};

/// How long a dropped link still counts as alive.
pub const GRACE: Duration = Duration::from_secs(5);

pub const NOTE_CAPTURE_DENIED: &str = "Mac sound stays on the Mac: allow CursorJump in System Settings › Privacy & Security › Screen & System Audio Recording";
pub const NOTE_NO_PC_OUTPUT: &str = "Mac sound stays on the Mac: the PC has no sound output";
pub const NOTE_NO_DRIVER: &str =
    "Mic stays on the Mac: the CursorJump audio driver isn't installed";
pub const NOTE_PC_MIC_BLOCKED: &str = "Mic stays on the Mac: on the PC, turn on Settings › Privacy & security › Microphone › Let desktop apps access your microphone";
pub const NOTE_NO_PC_MIC: &str = "Mic stays on the Mac: the PC has no microphone";
pub const NOTE_ROUTED: &str = "Mac sound and mic go through the PC";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    Unknown,
    Allowed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioPlan {
    /// Muted tap running (sound leaves the Mac).
    pub tap: bool,
    /// CursorJump Microphone is the default input.
    pub mic_default: bool,
    /// What to ask the PC for.
    pub want: AudioWant,
    /// The menu line.
    pub note: Option<&'static str>,
    /// When the grace ends, if a drop is pending.
    pub recheck_at: Option<Instant>,
}

#[derive(Debug, Clone)]
pub struct MacAudio {
    sharing: bool,
    capture: Capture,
    driver: bool,
    mic_listening: bool,
    connected: bool,
    dropped_at: Option<Instant>,
    pc: Option<AudioStatus>,
}

impl MacAudio {
    pub fn new(sharing: bool) -> Self {
        MacAudio {
            sharing,
            capture: Capture::Unknown,
            driver: false,
            mic_listening: false,
            connected: false,
            dropped_at: None,
            pc: None,
        }
    }

    /// Forgets the PC status unless still within `GRACE` of the drop.
    pub fn connected(&mut self, now: Instant) {
        let within = self
            .dropped_at
            .is_some_and(|t| now.saturating_duration_since(t) < GRACE);
        if !within {
            self.pc = None;
        }
        self.connected = true;
        self.dropped_at = None;
    }

    pub fn disconnected(&mut self, now: Instant) {
        if self.connected {
            self.dropped_at = Some(now);
        }
        self.connected = false;
    }

    pub fn set_sharing(&mut self, on: bool) {
        self.sharing = on;
    }

    pub fn set_capture(&mut self, c: Capture) {
        self.capture = c;
    }

    pub fn set_driver(&mut self, present: bool) {
        self.driver = present;
    }

    pub fn set_pc(&mut self, s: AudioStatus) {
        self.pc = Some(s);
    }

    pub fn set_mic_listening(&mut self, on: bool) {
        self.mic_listening = on;
    }

    pub fn plan(&self, now: Instant) -> AudioPlan {
        let pending = match self.dropped_at {
            Some(t) if !self.connected && now.saturating_duration_since(t) < GRACE => {
                Some(t + GRACE)
            }
            _ => None,
        };
        let alive = self.connected || pending.is_some();
        let out = self.pc.map_or(DeviceState::Missing, |s| s.output);
        let mic = self.pc.map_or(DeviceState::Missing, |s| s.mic);
        let tap =
            self.sharing && self.capture != Capture::Denied && alive && out == DeviceState::Ok;
        let mic_default = self.sharing && self.driver && alive && mic == DeviceState::Ok;
        let want = AudioWant {
            sound: tap && self.connected,
            mic: mic_default && self.mic_listening && self.connected,
        };
        let note = if self.sharing && self.connected {
            Some(if self.capture == Capture::Denied {
                NOTE_CAPTURE_DENIED
            } else if out != DeviceState::Ok {
                NOTE_NO_PC_OUTPUT
            } else if !self.driver {
                NOTE_NO_DRIVER
            } else if mic == DeviceState::Blocked {
                NOTE_PC_MIC_BLOCKED
            } else if mic != DeviceState::Ok {
                NOTE_NO_PC_MIC
            } else {
                NOTE_ROUTED
            })
        } else {
            None
        };
        AudioPlan {
            tap,
            mic_default,
            want,
            note,
            recheck_at: pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: AudioStatus = AudioStatus {
        output: DeviceState::Ok,
        mic: DeviceState::Ok,
    };

    fn up(t: Instant) -> MacAudio {
        let mut a = MacAudio::new(true);
        a.set_capture(Capture::Allowed);
        a.set_driver(true);
        a.connected(t);
        a.set_pc(OK);
        a
    }

    #[test]
    fn routes_both_ways_once_the_pc_reports_ok() {
        let t = Instant::now();
        let mut a = MacAudio::new(true);
        a.set_capture(Capture::Allowed);
        a.set_driver(true);
        a.connected(t);
        let p = a.plan(t);
        assert!(!p.tap && !p.mic_default, "no status yet");
        a.set_pc(OK);
        a.set_mic_listening(true);
        let p = a.plan(t);
        assert!(p.tap && p.mic_default);
        assert_eq!(
            p.want,
            AudioWant {
                sound: true,
                mic: true
            }
        );
        assert_eq!(p.note, Some(NOTE_ROUTED));
        assert_eq!(p.recheck_at, None);
    }

    #[test]
    fn a_blip_within_grace_changes_nothing() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_mic_listening(true);
        a.disconnected(t);
        let p = a.plan(t + Duration::from_millis(4900));
        assert!(p.tap && p.mic_default);
        assert_eq!(p.want, AudioWant::default());
        assert_eq!(p.recheck_at, Some(t + GRACE));
        assert_eq!(p.note, None);
    }

    #[test]
    fn a_drop_beyond_grace_falls_back() {
        let t = Instant::now();
        let mut a = up(t);
        a.disconnected(t);
        let p = a.plan(t + GRACE);
        assert!(!p.tap && !p.mic_default);
        assert_eq!(p.recheck_at, None);
    }

    #[test]
    fn reconnecting_within_grace_keeps_the_last_status() {
        let t = Instant::now();
        let mut a = up(t);
        a.disconnected(t);
        a.connected(t + Duration::from_secs(2));
        let p = a.plan(t + Duration::from_secs(2));
        assert!(p.tap && p.mic_default);
        assert_eq!(p.recheck_at, None);
    }

    #[test]
    fn reconnecting_after_a_fallback_waits_for_the_pc_status() {
        let t = Instant::now();
        let mut a = up(t);
        a.disconnected(t);
        let t2 = t + Duration::from_secs(9);
        a.connected(t2);
        assert!(!a.plan(t2).tap);
        a.set_pc(OK);
        assert!(a.plan(t2).tap);
    }

    #[test]
    fn sharing_off_falls_back_at_once() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_sharing(false);
        let p = a.plan(t);
        assert!(!p.tap && !p.mic_default);
        assert_eq!(p.want, AudioWant::default());
        assert_eq!(p.note, None);
    }

    #[test]
    fn missing_pc_mic_falls_back_for_the_mic_only() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_pc(AudioStatus {
            output: DeviceState::Ok,
            mic: DeviceState::Missing,
        });
        let p = a.plan(t);
        assert!(p.tap && !p.mic_default);
        assert_eq!(p.note, Some(NOTE_NO_PC_MIC));
    }

    #[test]
    fn blocked_mic_says_how_to_fix_it() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_pc(AudioStatus {
            output: DeviceState::Ok,
            mic: DeviceState::Blocked,
        });
        let p = a.plan(t);
        assert!(!p.mic_default);
        assert_eq!(p.note, Some(NOTE_PC_MIC_BLOCKED));
    }

    #[test]
    fn denied_capture_keeps_sound_local_but_routes_the_mic() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_capture(Capture::Denied);
        let p = a.plan(t);
        assert!(!p.tap && p.mic_default);
        assert_eq!(p.note, Some(NOTE_CAPTURE_DENIED));
    }

    #[test]
    fn the_pc_mic_is_asked_for_only_while_an_app_listens() {
        let t = Instant::now();
        let mut a = up(t);
        assert!(a.plan(t).mic_default && !a.plan(t).want.mic);
        a.set_mic_listening(true);
        assert!(a.plan(t).want.mic);
        a.set_mic_listening(false);
        assert!(!a.plan(t).want.mic);
    }

    #[test]
    fn no_driver_keeps_the_mic_local() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_driver(false);
        let p = a.plan(t);
        assert!(p.tap && !p.mic_default);
        assert_eq!(p.note, Some(NOTE_NO_DRIVER));
    }

    #[test]
    fn no_pc_output_keeps_sound_local() {
        let t = Instant::now();
        let mut a = up(t);
        a.set_pc(AudioStatus {
            output: DeviceState::Missing,
            mic: DeviceState::Ok,
        });
        let p = a.plan(t);
        assert!(!p.tap && p.mic_default);
        assert_eq!(p.note, Some(NOTE_NO_PC_OUTPUT));
    }
}
