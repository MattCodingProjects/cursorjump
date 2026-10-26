//! PC-side orchestration: a pure reducer deciding when the cursor crosses to
//! the Mac, when it returns, and how pairing completes.

use crate::config::Config;
use crate::dnd::DragId;
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

/// A file drag that rides across the link with the cursor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Riding {
    /// The PC's drag, on the Mac.
    Outgoing(DragId),
    /// The Mac's drag, on the PC.
    Incoming(DragId),
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
        /// The catcher holds a file drag and only the left button is held.
        offer: Option<DragId>,
    },
    MacLeave {
        frac: f32,
        drag: Option<DragId>,
    },
    /// The Mac: our outgoing drag was released on a target.
    DragDropped {
        drag: DragId,
    },
    /// Either drag ended without needing more from the machine
    /// (not dropped, finished, or `DoDragDrop` returned).
    DragGone {
        drag: DragId,
    },
    ReturnHotkey,
    HookLost,
    ConfigChanged(Config),
    /// The Mac said whether it can take the cursor (`Control::MacReady`).
    MacReady(bool),
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
        drag: Option<DragId>,
    },
    /// Input thread: remove hooks, restore the cursor at `pc_frac` on the PC edge.
    ReturnToPc {
        pc_frac: f32,
    },
    SendReleaseAll,
    Reconnect,
    /// Catcher: reject the outgoing drag until the user's real release.
    /// Net: `DragCancel { Cancelled }`.
    CancelSourceDrag {
        drag: DragId,
    },
    /// Catcher back to the edge strip; the OS drag continues on the PC.
    SourceDragHome {
        drag: DragId,
    },
    /// `DoDragDrop` with the Mac's offer.
    StartIncomingDrag {
        drag: DragId,
    },
    /// Cancel the incoming `DoDragDrop` (from `QueryContinueDrag`).
    CancelIncomingDrag {
        drag: DragId,
    },
}

#[derive(Debug)]
pub struct PcMachine {
    link: Link,
    where_: Where,
    cfg: Config,
    /// PC-side frac at the last entry.
    last_frac: f32,
    /// `edge_offset` in force when the cursor entered the Mac.
    entry_offset: f32,
    local_ans: Option<bool>,
    remote_ans: Option<bool>,
    /// Whether the Mac can take the cursor. True until it says otherwise:
    /// a crossing before its first `MacReady` arrives is bounced back by the Mac.
    mac_ready: bool,
    riding: Option<Riding>,
}

impl PcMachine {
    pub fn new(cfg: Config) -> Self {
        PcMachine {
            link: Link::Searching,
            where_: Where::Pc,
            cfg,
            last_frac: 0.5,
            entry_offset: 0.0,
            local_ans: None,
            remote_ans: None,
            mac_ready: true,
            riding: None,
        }
    }

    pub fn on(&mut self, ev: PcEvent) -> Vec<PcAction> {
        match ev {
            PcEvent::Connected { paired: true, .. } => {
                self.link = Link::Connected;
                self.mac_ready = true;
                vec![]
            }
            PcEvent::Connected {
                paired: false,
                code,
            } => {
                self.link = Link::Pairing { code: code.clone() };
                self.mac_ready = true;
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
                offer,
            } => {
                if self.link != Link::Connected
                    || self.where_ != Where::Pc
                    || !self.mac_ready
                    || self.cfg.paused
                    || (buttons_held && offer.is_none())
                    || matches!(self.riding, Some(Riding::Incoming(_)))
                    || game_locked(&game)
                {
                    return vec![];
                }
                self.last_frac = frac;
                self.entry_offset = self.cfg.edge_offset;
                self.where_ = Where::Mac;
                self.riding = offer.map(Riding::Outgoing);
                vec![PcAction::EnterMac {
                    mac_edge: opposite(self.cfg.mac_side),
                    frac: apply_offset(frac, self.cfg.edge_offset),
                    drag: offer,
                }]
            }
            PcEvent::MacLeave { frac, drag } => {
                if self.where_ != Where::Mac {
                    return vec![];
                }
                self.where_ = Where::Pc;
                let mut actions = vec![PcAction::ReturnToPc {
                    pc_frac: remove_offset(frac, self.entry_offset),
                }];
                if let Some(drag) = drag {
                    if self.riding == Some(Riding::Outgoing(drag)) {
                        self.riding = None;
                        actions.push(PcAction::SourceDragHome { drag });
                    } else {
                        self.riding = Some(Riding::Incoming(drag));
                        actions.push(PcAction::StartIncomingDrag { drag });
                    }
                }
                actions
            }
            PcEvent::DragDropped { drag } => {
                // The PC's own OS drag already ended on the catcher, when the
                // user's real release passed through; the cursor stays on the Mac.
                if self.riding == Some(Riding::Outgoing(drag)) {
                    self.riding = None;
                }
                vec![]
            }
            PcEvent::DragGone { drag } => {
                if self.riding == Some(Riding::Outgoing(drag))
                    || self.riding == Some(Riding::Incoming(drag))
                {
                    self.riding = None;
                }
                vec![]
            }
            PcEvent::Disconnected => {
                let mut actions = Vec::new();
                match self.riding.take() {
                    Some(Riding::Outgoing(drag)) => {
                        actions.push(PcAction::CancelSourceDrag { drag })
                    }
                    Some(Riding::Incoming(drag)) => {
                        actions.push(PcAction::CancelIncomingDrag { drag })
                    }
                    None => {}
                }
                if self.where_ == Where::Mac {
                    actions.push(PcAction::ReturnToPc {
                        pc_frac: self.last_frac,
                    });
                }
                actions.push(PcAction::Reconnect);
                self.where_ = Where::Pc;
                self.link = Link::Searching;
                self.mac_ready = true;
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
            PcEvent::MacReady(ready) => {
                self.mac_ready = ready;
                if ready {
                    vec![]
                } else {
                    // The Mac can no longer be controlled: never strand the cursor there.
                    self.force_return()
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
        let mut actions = vec![PcAction::SendReleaseAll];
        if let Some(Riding::Outgoing(drag)) = self.riding {
            self.riding = None;
            actions.push(PcAction::CancelSourceDrag { drag });
        }
        actions.push(PcAction::ReturnToPc {
            pc_frac: self.last_frac,
        });
        actions
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn where_(&self) -> &Where {
        &self.where_
    }

    /// Whether the Mac can take the cursor (it has Accessibility).
    pub fn mac_ready(&self) -> bool {
        self.mac_ready
    }

    /// The file drag riding across the link, if any.
    pub fn riding(&self) -> Option<Riding> {
        self.riding
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
            offer: None,
        }
    }

    /// A push with the left button held on a file drag the catcher holds.
    fn push_offer(frac: f32, game: GameProbe, drag: DragId) -> PcEvent {
        PcEvent::EdgePush {
            frac,
            game,
            buttons_held: true,
            offer: Some(drag),
        }
    }

    fn leave(frac: f32, drag: Option<DragId>) -> PcEvent {
        PcEvent::MacLeave { frac, drag }
    }

    /// Connected, with the PC's drag 7 riding on the Mac.
    fn outgoing() -> PcMachine {
        let mut m = connected(Config::default());
        m.on(push_offer(0.3, free(), 7));
        m
    }

    /// Connected, with the Mac's drag 9 riding on the PC.
    fn incoming() -> PcMachine {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        m.on(leave(0.4, Some(9)));
        m
    }

    fn assert_enter(a: &[PcAction], edge: Side, want: f32) {
        match a {
            [
                PcAction::EnterMac {
                    mac_edge,
                    frac,
                    drag: None,
                },
            ] => {
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
        let a = m.on(leave(0.6, None));
        match a.as_slice() {
            [PcAction::ReturnToPc { pc_frac }] => assert!((pc_frac - 0.5).abs() < 1e-5),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(m.where_(), &Where::Pc);
    }

    #[test]
    fn pc_offset_snapshot_survives_config_change() {
        let mut m = connected(Config::default());
        m.on(push(0.5, free(), false));
        m.on(PcEvent::ConfigChanged(Config {
            edge_offset: 0.2,
            ..Config::default()
        }));
        let a = m.on(leave(0.4, None));
        match a.as_slice() {
            [PcAction::ReturnToPc { pc_frac }] => assert!((pc_frac - 0.4).abs() < 1e-5),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pc_mac_leave_ignored_on_pc() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(leave(0.6, None)), vec![]);
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
    fn pc_mac_ready_starts_true() {
        let m = PcMachine::new(Config::default());
        assert!(m.mac_ready(), "true until the Mac says otherwise");
        let mut m = connected(Config::default());
        assert!(m.mac_ready());
        assert_enter(&m.on(push(0.3, free(), false)), Side::Left, 0.3);
    }

    #[test]
    fn pc_mac_not_ready_is_a_wall() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(PcEvent::MacReady(false)), vec![]);
        assert!(!m.mac_ready());
        assert_eq!(m.on(push(0.3, free(), false)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.on(PcEvent::MacReady(true)), vec![]);
        assert!(m.mac_ready());
        assert_enter(&m.on(push(0.3, free(), false)), Side::Left, 0.3);
    }

    #[test]
    fn pc_mac_not_ready_while_on_mac_returns() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        assert_eq!(
            m.on(PcEvent::MacReady(false)),
            vec![
                PcAction::SendReleaseAll,
                PcAction::ReturnToPc { pc_frac: 0.3 }
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.on(PcEvent::MacReady(false)), vec![], "already back");
    }

    #[test]
    fn pc_mac_ready_while_on_mac_changes_nothing() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        assert_eq!(m.on(PcEvent::MacReady(true)), vec![]);
        assert_eq!(m.where_(), &Where::Mac);
    }

    #[test]
    fn pc_mac_ready_resets_on_disconnect_and_connect() {
        let mut m = connected(Config::default());
        m.on(PcEvent::MacReady(false));
        m.on(PcEvent::Disconnected);
        assert!(m.mac_ready(), "reset on disconnect");

        m.on(PcEvent::MacReady(false));
        m.on(PcEvent::Connected {
            paired: true,
            code: String::new(),
        });
        assert!(m.mac_ready(), "reset on a paired connect");

        m.on(PcEvent::MacReady(false));
        m.on(PcEvent::Connected {
            paired: false,
            code: "1".into(),
        });
        assert!(m.mac_ready(), "reset on a pairing connect");
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

    #[test]
    fn file_drag_crosses_with_left_held() {
        let mut m = connected(Config::default());
        assert_eq!(
            m.on(push_offer(0.3, free(), 7)),
            vec![PcAction::EnterMac {
                mac_edge: Side::Left,
                frac: 0.3,
                drag: Some(7)
            }]
        );
        assert_eq!(m.where_(), &Where::Mac);
        assert_eq!(m.riding(), Some(Riding::Outgoing(7)));
    }

    #[test]
    fn buttons_held_without_offer_is_still_a_wall() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(push(0.3, free(), true)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn file_drag_still_blocked_by_game_lock() {
        let mut m = connected(Config::default());
        assert_eq!(m.on(push_offer(0.3, locked(), 7)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn file_drag_still_blocked_when_paused() {
        let mut m = connected(Config {
            paused: true,
            ..Config::default()
        });
        assert_eq!(m.on(push_offer(0.3, free(), 7)), vec![]);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn outgoing_drag_coming_home_continues_on_pc() {
        let mut m = outgoing();
        match m.on(leave(0.6, Some(7))).as_slice() {
            [
                PcAction::ReturnToPc { pc_frac },
                PcAction::SourceDragHome { drag: 7 },
            ] => assert!((pc_frac - 0.6).abs() < 1e-5),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn mac_leave_with_foreign_drag_starts_incoming() {
        let mut m = connected(Config::default());
        m.on(push(0.3, free(), false));
        match m.on(leave(0.6, Some(9))).as_slice() {
            [
                PcAction::ReturnToPc { pc_frac },
                PcAction::StartIncomingDrag { drag: 9 },
            ] => assert!((pc_frac - 0.6).abs() < 1e-5),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), Some(Riding::Incoming(9)));
    }

    #[test]
    fn incoming_drag_makes_the_edge_a_wall() {
        let mut m = incoming();
        assert_eq!(m.on(push(0.3, free(), false)), vec![]);
        assert_eq!(m.on(push_offer(0.3, free(), 7)), vec![]);
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), Some(Riding::Incoming(9)));
        // Once the drag is gone the edge opens again.
        assert_eq!(m.on(PcEvent::DragGone { drag: 9 }), vec![]);
        assert_eq!(m.riding(), None);
        assert_enter(&m.on(push(0.3, free(), false)), Side::Left, 0.3);
    }

    #[test]
    fn dropped_outgoing_clears_riding_and_stays_on_mac() {
        let mut m = outgoing();
        assert_eq!(m.on(PcEvent::DragDropped { drag: 7 }), vec![]);
        assert_eq!(m.riding(), None);
        assert_eq!(m.where_(), &Where::Mac);
    }

    #[test]
    fn outgoing_drag_gone_clears_riding() {
        let mut m = outgoing();
        assert_eq!(m.on(PcEvent::DragGone { drag: 7 }), vec![]);
        assert_eq!(m.riding(), None);
        assert_eq!(m.where_(), &Where::Mac);
    }

    #[test]
    fn drag_events_for_other_drags_are_ignored() {
        let mut m = outgoing();
        assert_eq!(m.on(PcEvent::DragDropped { drag: 8 }), vec![]);
        assert_eq!(m.on(PcEvent::DragGone { drag: 8 }), vec![]);
        assert_eq!(m.riding(), Some(Riding::Outgoing(7)));
        let mut m = incoming();
        assert_eq!(m.on(PcEvent::DragDropped { drag: 9 }), vec![]);
        assert_eq!(m.riding(), Some(Riding::Incoming(9)), "only outgoing drops");
    }

    #[test]
    fn hotkey_during_outgoing_drag_cancels_before_return() {
        let mut m = outgoing();
        assert_eq!(
            m.on(PcEvent::ReturnHotkey),
            vec![
                PcAction::SendReleaseAll,
                PcAction::CancelSourceDrag { drag: 7 },
                PcAction::ReturnToPc { pc_frac: 0.3 }
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn other_forced_returns_during_outgoing_drag_cancel_before_return() {
        let want = vec![
            PcAction::SendReleaseAll,
            PcAction::CancelSourceDrag { drag: 7 },
            PcAction::ReturnToPc { pc_frac: 0.3 },
        ];
        let paused = PcEvent::ConfigChanged(Config {
            paused: true,
            ..Config::default()
        });
        for ev in [PcEvent::HookLost, paused, PcEvent::MacReady(false)] {
            let mut m = outgoing();
            assert_eq!(m.on(ev.clone()), want, "{ev:?}");
            assert_eq!(m.where_(), &Where::Pc, "{ev:?}");
            assert_eq!(m.riding(), None, "{ev:?}");
        }
    }

    #[test]
    fn disconnect_during_outgoing_drag_cancels_before_return() {
        let mut m = outgoing();
        assert_eq!(
            m.on(PcEvent::Disconnected),
            vec![
                PcAction::CancelSourceDrag { drag: 7 },
                PcAction::ReturnToPc { pc_frac: 0.3 },
                PcAction::Reconnect
            ]
        );
        assert_eq!(m.where_(), &Where::Pc);
        assert_eq!(m.riding(), None);
    }

    #[test]
    fn disconnect_during_incoming_drag_cancels_it() {
        let mut m = incoming();
        assert_eq!(
            m.on(PcEvent::Disconnected),
            vec![
                PcAction::CancelIncomingDrag { drag: 9 },
                PcAction::Reconnect
            ]
        );
        assert_eq!(m.riding(), None);
        assert_eq!(m.link(), &Link::Searching);
    }
}
