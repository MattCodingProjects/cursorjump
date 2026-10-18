//! PC-side orchestration: a pure reducer deciding when the cursor crosses to
//! the Mac, when it returns, and how pairing completes.

use crate::config::Config;
use crate::gaming::{GameProbe, game_locked};
use crate::layout::{apply_offset, opposite, remove_offset};
use crate::proto::Side;

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Searching,
    Pairing { code: String },
    Connected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Where {
    Pc,
    Mac,
}

#[derive(Debug, Clone)]
pub enum PcEvent {
    Connected {
        paired: bool,
        code: String,
    },
    PairAnswered {
        local: bool,
        remote: Option<bool>,
    },
    Disconnected,
    /// The input thread saw a push past the configured edge.
    EdgePush {
        frac: f32,
        game: GameProbe,
        buttons_held: bool,
    },
    MacLeave {
        frac: f32,
    },
    ReturnHotkey,
    HookLost,
    ConfigChanged(Config),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PcAction {
    ShowPairDialog {
        code: String,
    },
    PersistPairing,
    /// Input thread: install hooks, park and hide the cursor. Net: send Enter.
    EnterMac {
        mac_edge: Side,
        frac: f32,
    },
    /// Input thread: remove hooks, restore the cursor at `pc_frac` on the PC edge.
    ReturnToPc {
        pc_frac: f32,
    },
    SendReleaseAll,
    Reconnect,
}

#[derive(Debug)]
pub struct PcMachine {
    link: Link,
    where_: Where,
    cfg: Config,
    /// PC-side frac at the last entry.
    last_frac: f32,
    local_ans: Option<bool>,
    remote_ans: Option<bool>,
}

impl PcMachine {
    pub fn new(cfg: Config) -> Self {
        PcMachine {
            link: Link::Searching,
            where_: Where::Pc,
            cfg,
            last_frac: 0.5,
            local_ans: None,
            remote_ans: None,
        }
    }

    pub fn on(&mut self, ev: PcEvent) -> Vec<PcAction> {
        match ev {
            PcEvent::Connected { paired: true, .. } => {
                self.link = Link::Connected;
                vec![]
            }
            PcEvent::Connected {
                paired: false,
                code,
            } => {
                self.link = Link::Pairing { code: code.clone() };
                self.clear_answers();
                vec![PcAction::ShowPairDialog { code }]
            }
            PcEvent::PairAnswered { local, remote } => {
                if !matches!(self.link, Link::Pairing { .. }) {
                    return vec![];
                }
                self.local_ans = Some(local);
                if remote.is_some() {
                    self.remote_ans = remote;
                }
                if self.local_ans == Some(false) || self.remote_ans == Some(false) {
                    self.link = Link::Searching;
                    self.clear_answers();
                    vec![PcAction::Reconnect]
                } else if self.local_ans == Some(true) && self.remote_ans == Some(true) {
                    self.link = Link::Connected;
                    vec![PcAction::PersistPairing]
                } else {
                    vec![]
                }
            }
            PcEvent::EdgePush {
                frac,
                game,
                buttons_held,
            } => {
                if self.link != Link::Connected
                    || self.where_ != Where::Pc
                    || self.cfg.paused
                    || buttons_held
                    || game_locked(&game)
                {
                    return vec![];
                }
                self.last_frac = frac;
                self.where_ = Where::Mac;
                vec![PcAction::EnterMac {
                    mac_edge: opposite(self.cfg.mac_side),
                    frac: apply_offset(frac, self.cfg.edge_offset),
                }]
            }
            PcEvent::MacLeave { frac } => {
                if self.where_ != Where::Mac {
                    return vec![];
                }
                self.where_ = Where::Pc;
                vec![PcAction::ReturnToPc {
                    pc_frac: remove_offset(frac, self.cfg.edge_offset),
                }]
            }
            PcEvent::Disconnected => {
                let mut actions = Vec::new();
                if self.where_ == Where::Mac {
                    actions.push(PcAction::ReturnToPc {
                        pc_frac: self.last_frac,
                    });
                }
                actions.push(PcAction::Reconnect);
                self.where_ = Where::Pc;
                self.link = Link::Searching;
                self.clear_answers();
                actions
            }
            PcEvent::ReturnHotkey | PcEvent::HookLost => self.force_return(),
            PcEvent::ConfigChanged(cfg) => {
                self.cfg = cfg;
                if self.cfg.paused {
                    self.force_return()
                } else {
                    vec![]
                }
            }
        }
    }

    fn clear_answers(&mut self) {
        self.local_ans = None;
        self.remote_ans = None;
    }

    /// Leave the Mac (if on it), releasing anything held there.
    fn force_return(&mut self) -> Vec<PcAction> {
        if self.where_ != Where::Mac {
            return vec![];
        }
        self.where_ = Where::Pc;
        vec![
            PcAction::SendReleaseAll,
            PcAction::ReturnToPc {
                pc_frac: self.last_frac,
            },
        ]
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn where_(&self) -> &Where {
        &self.where_
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Rect;

    const SCREEN: Rect = Rect {
        left: 0,
        top: 0,
        right: 2560,
        bottom: 1440,
    };

    fn free() -> GameProbe {
        GameProbe {
            clip: SCREEN,
            virtual_screen: SCREEN,
            cursor_showing: true,
            d3d_fullscreen: false,
        }
    }

    fn locked() -> GameProbe {
        GameProbe {
            cursor_showing: false,
            ..free()
        }
    }

    fn connected(cfg: Config) -> PcMachine {
        let mut m = PcMachine::new(cfg);
        m.on(PcEvent::Connected {
            paired: true,
            code: String::new(),
        });
        m
    }

    fn push(frac: f32, game: GameProbe, buttons_held: bool) -> PcEvent {
        PcEvent::EdgePush {
            frac,
            game,
            buttons_held,
        }
    }

    fn assert_enter(a: &[PcAction], edge: Side, want: f32) {
        match a {
            [PcAction::EnterMac { mac_edge, frac }] => {
                assert_eq!(*mac_edge, edge);
                assert!((frac - want).abs() < 1e-5, "frac {frac} != {want}");
            }
            other => panic!("unexpected actions {other:?}"),
        }
    }

    fn pairing() -> PcMachine {
        let mut m = PcMachine::new(Config::default());
        m.on(PcEvent::Connected {
            paired: false,
            code: "1".into(),
        });
        m
    }

    #[test]
    fn pc_edge_push_enters_when_clear() {
        let mut m = connected(Config::default()); // mac_side Right
        let a = m.on(push(0.3, free(), false));
        assert_enter(&a, Side::Left, 0.3);
        assert_eq!(m.where_(), &Where::Mac);
    }

    #[test]
    fn pc_edge_push_blocked_in_game() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(push(0.3, locked(), false)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_edge_push_blocked_when_paused() {
        let mut m = connected(Config {
            paused: true,
            ..Config::default()
        });
        assert_eq!(m.on(push(0.3, free(), false)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_edge_push_blocked_with_button_held() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(push(0.3, free(), true)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_edge_push_ignored_while_pairing() {
        let mut m = PcMachine::new(Config::default());
        m.on(PcEvent::Connected {
            paired: false,
            code: "123456".into(),
        });
        assert_eq!(m.on(push(0.3, free(), false)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_edge_push_ignored_while_on_mac() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        assert_eq!(m.on(push(0.7, free(), false)), vec![]);
    }

    #[test]
    fn pc_offset_applied_and_removed() {
        let mut m = connected(Config {
            edge_offset: 0.1,
            ..Config::default()
        });
        let a = m.on(push(0.5, free(), false));
        assert_enter(&a, Side::Left, 0.6);
        let a = m.on(PcEvent::MacLeave { frac: 0.6 });
        match a.as_slice() {
            [PcAction::ReturnToPc { pc_frac }] => assert!((pc_frac - 0.5).abs() < 1e-5),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_mac_leave_ignored_on_pc() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(PcEvent::MacLeave { frac: 0.6 }), vec![]);
    }

    #[test]
    fn pc_disconnect_while_on_mac_returns() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        let a = m.on(PcEvent::Disconnected);
        assert_eq!(
            a,
            vec![PcAction::ReturnToPc { pc_frac: 0.3 }, PcAction::Reconnect]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.link(), &Link::Searching);
    }

    #[test]
    fn pc_disconnect_on_pc_only_reconnects() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(PcEvent::Disconnected), vec![PcAction::Reconnect]);
        assert_eq!(m.link(), &Link::Searching);
    }

    #[test]
    fn pc_hotkey_returns_and_releases() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        let a = m.on(PcEvent::ReturnHotkey);
        assert_eq!(
            a,
            vec![
                PcAction::SendReleaseAll,
                PcAction::ReturnToPc { pc_frac: 0.3 }
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.on(PcEvent::ReturnHotkey), vec![]);
    }

    #[test]
    fn pc_hook_lost_returns() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        let a = m.on(PcEvent::HookLost);
        assert_eq!(
            a,
            vec![
                PcAction::SendReleaseAll,
                PcAction::ReturnToPc { pc_frac: 0.3 }
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.on(PcEvent::HookLost), vec![]);
    }

    #[test]
    fn pc_config_paused_while_on_mac_returns() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        let a = m.on(PcEvent::ConfigChanged(Config {
            paused: true,
            ..Config::default()
        }));
        assert_eq!(
            a,
            vec![
                PcAction::SendReleaseAll,
                PcAction::ReturnToPc { pc_frac: 0.3 }
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_config_change_otherwise_silent() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        let cfg = Config {
            edge_offset: 0.2,
            ..Config::default()
        };
        assert_eq!(m.on(PcEvent::ConfigChanged(cfg)), vec![]);
        assert_eq!(m.where_(), &Where::Mac);
    }

    #[test]
    fn pc_unpaired_connect_shows_dialog() {
        let mut m = PcMachine::new(Config::default());
        let a = m.on(PcEvent::Connected {
            paired: false,
            code: "424242".into(),
        });
        assert_eq!(
            a,
            vec![PcAction::ShowPairDialog {
                code: "424242".into()
            }]
        );
        assert_eq!(
            m.link(),
            &Link::Pairing {
                code: "424242".into()
            }
        );
    }

    #[test]
    fn pc_pairing_needs_both() {
        let mut m = pairing();
        let a = m.on(PcEvent::PairAnswered {
            local: true,
            remote: Some(false),
        });
        assert_eq!(a, vec![PcAction::Reconnect]);
        assert!(!a.contains(&PcAction::PersistPairing));
        assert_eq!(m.link(), &Link::Searching);
    }

    #[test]
    fn pc_pairing_completes_when_both_accept() {
        let mut m = pairing();
        assert_eq!(
            m.on(PcEvent::PairAnswered {
                local: true,
                remote: None
            }),
            vec![]
        );
        assert!(matches!(m.link(), Link::Pairing { .. }));
        let a = m.on(PcEvent::PairAnswered {
            local: true,
            remote: Some(true),
        });
        assert_eq!(a, vec![PcAction::PersistPairing]);
        assert_eq!(m.link(), &Link::Connected);
    }

    #[test]
    fn pc_pairing_answers_ignored_once_connected() {
        let mut m = pairing();
        m.on(PcEvent::PairAnswered {
            local: true,
            remote: Some(true),
        });
        assert_eq!(
            m.on(PcEvent::PairAnswered {
                local: false,
                remote: None
            }),
            vec![]
        );
        assert_eq!(m.link(), &Link::Connected);
    }

    #[test]
    fn pc_pairing_local_reject_reconnects() {
        let mut m = pairing();
        let a = m.on(PcEvent::PairAnswered {
            local: false,
            remote: None,
        });
        assert_eq!(a, vec![PcAction::Reconnect]);
        assert_eq!(m.link(), &Link::Searching);
    }
}
