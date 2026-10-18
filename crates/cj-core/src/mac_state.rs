//! Mac-side orchestration: a pure reducer deciding when the Mac is controlled
//! by the PC and what the platform layer must do about it.

use crate::proto::Side;

#[derive(Debug, Clone, PartialEq)]
pub enum MacEvent {
    Enter { edge: Side, frac: f32 },
    PushOut { frac: f32, buttons_held: bool },
    ReleaseAll,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MacAction {
    WakeDisplay,
    WarpTo { edge: Side, frac: f32 },
    SendLeave { frac: f32 },
    ReleaseAllInjected,
    StartControl,
    StopControl,
}

#[derive(Debug, Default)]
pub struct MacMachine {
    controlled: bool,
    entry: Option<Side>,
}

impl MacMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(&mut self, ev: MacEvent) -> Vec<MacAction> {
        match ev {
            MacEvent::Enter { edge, frac } => {
                self.controlled = true;
                self.entry = Some(edge);
                vec![
                    MacAction::WakeDisplay,
                    MacAction::WarpTo { edge, frac },
                    MacAction::StartControl,
                ]
            }
            MacEvent::PushOut { frac, buttons_held } => {
                if !self.controlled || buttons_held {
                    return vec![];
                }
                self.controlled = false;
                self.entry = None;
                vec![
                    MacAction::ReleaseAllInjected,
                    MacAction::SendLeave { frac },
                    MacAction::StopControl,
                ]
            }
            MacEvent::ReleaseAll => vec![MacAction::ReleaseAllInjected],
            MacEvent::Disconnected => {
                if !self.controlled {
                    return vec![];
                }
                self.controlled = false;
                self.entry = None;
                vec![MacAction::ReleaseAllInjected, MacAction::StopControl]
            }
        }
    }

    pub fn entry_edge(&self) -> Option<Side> {
        self.entry
    }

    pub fn controlled(&self) -> bool {
        self.controlled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entered() -> MacMachine {
        let mut m = MacMachine::new();
        m.on(MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
        });
        m
    }

    #[test]
    fn mac_enter_wakes_and_warps() {
        let mut m = MacMachine::new();
        let a = m.on(MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
        });
        assert_eq!(
            a,
            vec![
                MacAction::WakeDisplay,
                MacAction::WarpTo {
                    edge: Side::Left,
                    frac: 0.4
                },
                MacAction::StartControl
            ]
        );
        assert!(m.controlled());
        assert_eq!(m.entry_edge(), Some(Side::Left));
    }

    #[test]
    fn mac_push_out_with_button_held_is_wall() {
        let mut m = entered();
        let a = m.on(MacEvent::PushOut {
            frac: 0.5,
            buttons_held: true,
        });
        assert_eq!(a, vec![]);
        assert!(m.controlled());
    }

    #[test]
    fn mac_push_out_releases_and_leaves() {
        let mut m = entered();
        let a = m.on(MacEvent::PushOut {
            frac: 0.5,
            buttons_held: false,
        });
        assert_eq!(
            a,
            vec![
                MacAction::ReleaseAllInjected,
                MacAction::SendLeave { frac: 0.5 },
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled());
        assert_eq!(m.entry_edge(), None);
    }

    #[test]
    fn mac_push_out_ignored_when_not_controlled() {
        let mut m = MacMachine::new();
        let a = m.on(MacEvent::PushOut {
            frac: 0.5,
            buttons_held: false,
        });
        assert_eq!(a, vec![]);
    }

    #[test]
    fn mac_release_all_keeps_state() {
        let mut m = entered();
        assert_eq!(
            m.on(MacEvent::ReleaseAll),
            vec![MacAction::ReleaseAllInjected]
        );
        assert!(m.controlled());
        assert_eq!(m.entry_edge(), Some(Side::Left));
    }

    #[test]
    fn mac_disconnect_releases_all() {
        let mut m = entered();
        let a = m.on(MacEvent::Disconnected);
        assert_eq!(
            a,
            vec![MacAction::ReleaseAllInjected, MacAction::StopControl]
        );
        assert!(!m.controlled());
        assert_eq!(m.entry_edge(), None);
        assert_eq!(m.on(MacEvent::Disconnected), vec![]);
    }
}
