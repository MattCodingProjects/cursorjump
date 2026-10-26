//! Mac-side orchestration: a pure reducer deciding when the Mac is controlled
//! by the PC and what the platform layer must do about it.

use crate::dnd::DragId;
use crate::proto::Side;

#[derive(Debug, Clone, PartialEq)]
pub enum MacEvent {
    /// `drag`: a PC file drag rides along with the cursor.
    Enter {
        edge: Side,
        frac: f32,
        drag: Option<DragId>,
    },
    /// `offer`: the Mac catcher holds a file drag.
    PushOut {
        frac: f32,
        buttons_held: bool,
        offer: Option<DragId>,
    },
    /// A push-out with only the left button held and no catcher offer, on
    /// a Mac with a catcher (R48): a drag that starts late (Chrome,
    /// WhatsApp) may yet drop on the catcher at the release, so the
    /// release goes out now and the leave waits for `LateDrop`.
    PushOutLate {
        frac: f32,
    },
    /// The wait after `PushOutLate` is over: `drag` is the drag that
    /// dropped on the catcher meanwhile, if any.
    LateDrop {
        drag: Option<DragId>,
    },
    /// The incoming drag ended on the Mac (dropped or cancelled).
    DragEnded {
        drag: DragId,
    },
    ReleaseAll,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MacAction {
    WakeDisplay,
    WarpTo {
        edge: Side,
        frac: f32,
    },
    SendLeave {
        frac: f32,
        drag: Option<DragId>,
    },
    ReleaseAllInjected,
    StartControl,
    StopControl,
    /// Start the Mac-side drag of the PC's offer, under the cursor.
    BeginDrag {
        drag: DragId,
    },
    /// Post Esc to cancel the incoming drag; always before `ReleaseAllInjected`.
    CancelDrag {
        drag: DragId,
    },
    /// Start the wait for a late drop (R48), just before the release goes
    /// out; `LateDrop` ends it.
    AwaitLateDrop,
}

#[derive(Debug, Default)]
pub struct MacMachine {
    controlled: bool,
    entry: Option<Side>,
    /// The PC's drag riding on the Mac.
    incoming: Option<DragId>,
    /// Where the cursor pushed out, while the leave waits for a late drop.
    leaving: Option<f32>,
}

impl MacMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(&mut self, ev: MacEvent) -> Vec<MacAction> {
        match ev {
            MacEvent::Enter { edge, frac, drag } => {
                let mut actions = if self.incoming != drag {
                    // A stale drag from an earlier crossing must not linger.
                    self.cancel_incoming()
                } else {
                    Vec::new()
                };
                self.controlled = true;
                self.entry = Some(edge);
                self.incoming = drag;
                self.leaving = None;
                actions.extend([
                    MacAction::WakeDisplay,
                    MacAction::WarpTo { edge, frac },
                    MacAction::StartControl,
                ]);
                actions.extend(drag.map(|drag| MacAction::BeginDrag { drag }));
                actions
            }
            MacEvent::PushOut {
                frac,
                buttons_held,
                offer,
            } => {
                if !self.controlled {
                    return vec![];
                }
                let mut actions = Vec::new();
                let drag = if let Some(drag) = self.incoming.take() {
                    // The PC's drag goes home with the cursor.
                    actions.push(MacAction::CancelDrag { drag });
                    Some(drag)
                } else if offer.is_some() {
                    // Releasing drops the drag onto the catcher.
                    offer
                } else if buttons_held {
                    return vec![];
                } else {
                    None
                };
                self.controlled = false;
                self.entry = None;
                actions.extend([
                    MacAction::ReleaseAllInjected,
                    MacAction::SendLeave { frac, drag },
                    MacAction::StopControl,
                ]);
                actions
            }
            MacEvent::PushOutLate { frac } => {
                if self.incoming.is_some() {
                    // The PC's drag goes home at once; nothing waits.
                    return self.on(MacEvent::PushOut {
                        frac,
                        buttons_held: true,
                        offer: None,
                    });
                }
                if !self.controlled {
                    return vec![];
                }
                self.controlled = false;
                self.entry = None;
                self.leaving = Some(frac);
                // The wait (and the catcher's late window) opens before the
                // release goes out, so a drop at the release is taken.
                vec![
                    MacAction::AwaitLateDrop,
                    MacAction::ReleaseAllInjected,
                    MacAction::StopControl,
                ]
            }
            MacEvent::LateDrop { drag } => match self.leaving.take() {
                Some(frac) => vec![MacAction::SendLeave { frac, drag }],
                None => vec![],
            },
            MacEvent::DragEnded { drag } => {
                if self.incoming == Some(drag) {
                    self.incoming = None;
                }
                vec![]
            }
            // The PC took the cursor back (hotkey, hook lost, pause, quit).
            MacEvent::ReleaseAll => {
                self.leaving = None;
                let mut actions = self.cancel_incoming();
                if !self.controlled {
                    actions.push(MacAction::ReleaseAllInjected);
                    return actions;
                }
                self.controlled = false;
                self.entry = None;
                actions.extend([MacAction::ReleaseAllInjected, MacAction::StopControl]);
                actions
            }
            MacEvent::Disconnected => {
                self.leaving = None;
                let mut actions = self.cancel_incoming();
                if !self.controlled {
                    return actions;
                }
                self.controlled = false;
                self.entry = None;
                actions.extend([MacAction::ReleaseAllInjected, MacAction::StopControl]);
                actions
            }
        }
    }

    /// Cancels the incoming drag, if any: always before releasing the buttons,
    /// so the release does not drop it.
    fn cancel_incoming(&mut self) -> Vec<MacAction> {
        self.incoming
            .take()
            .map(|drag| MacAction::CancelDrag { drag })
            .into_iter()
            .collect()
    }

    pub fn entry_edge(&self) -> Option<Side> {
        self.entry
    }

    pub fn controlled(&self) -> bool {
        self.controlled
    }

    /// The PC's drag riding on the Mac, if any.
    pub fn incoming(&self) -> Option<DragId> {
        self.incoming
    }

    /// Whether a leave waits for a late drop (between `PushOutLate` and
    /// `LateDrop`).
    pub fn awaiting_late_drop(&self) -> bool {
        self.leaving.is_some()
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
            drag: None,
        });
        m
    }

    /// Entered with the PC's drag 7 riding along.
    fn entered_with_drag() -> MacMachine {
        let mut m = MacMachine::new();
        m.on(MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
            drag: Some(7),
        });
        m
    }

    fn push_out(buttons_held: bool, offer: Option<DragId>) -> MacEvent {
        MacEvent::PushOut {
            frac: 0.5,
            buttons_held,
            offer,
        }
    }

    #[test]
    fn mac_enter_wakes_and_warps() {
        let mut m = MacMachine::new();
        let a = m.on(MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
            drag: None,
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
            offer: None,
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
            offer: None,
        });
        assert_eq!(
            a,
            vec![
                MacAction::ReleaseAllInjected,
                MacAction::SendLeave {
                    frac: 0.5,
                    drag: None
                },
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
            offer: None,
        });
        assert_eq!(a, vec![]);
    }

    #[test]
    fn mac_release_all_while_controlled_stops_control() {
        let mut m = entered();
        assert_eq!(
            m.on(MacEvent::ReleaseAll),
            vec![MacAction::ReleaseAllInjected, MacAction::StopControl]
        );
        assert!(!m.controlled());
        assert_eq!(m.entry_edge(), None);
    }

    #[test]
    fn mac_release_all_when_not_controlled_only_releases() {
        let mut m = MacMachine::new();
        assert_eq!(
            m.on(MacEvent::ReleaseAll),
            vec![MacAction::ReleaseAllInjected]
        );
        assert!(!m.controlled());
        assert_eq!(m.entry_edge(), None);
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

    #[test]
    fn enter_with_drag_begins_drag_after_start_control() {
        let mut m = MacMachine::new();
        let a = m.on(MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
            drag: Some(7),
        });
        assert_eq!(
            a,
            vec![
                MacAction::WakeDisplay,
                MacAction::WarpTo {
                    edge: Side::Left,
                    frac: 0.4
                },
                MacAction::StartControl,
                MacAction::BeginDrag { drag: 7 }
            ]
        );
        assert_eq!(m.incoming(), Some(7));
        assert_eq!(entered().incoming(), None);
    }

    #[test]
    fn incoming_drag_push_out_cancels_before_leaving() {
        let mut m = entered_with_drag();
        // The injected left button is down for the drag; that is no wall.
        assert_eq!(
            m.on(push_out(true, None)),
            vec![
                MacAction::CancelDrag { drag: 7 },
                MacAction::ReleaseAllInjected,
                MacAction::SendLeave {
                    frac: 0.5,
                    drag: Some(7)
                },
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled());
        assert_eq!(m.incoming(), None);
    }

    #[test]
    fn push_out_with_offer_drops_on_catcher_then_leaves() {
        let mut m = entered();
        assert_eq!(
            m.on(push_out(true, Some(9))),
            vec![
                MacAction::ReleaseAllInjected,
                MacAction::SendLeave {
                    frac: 0.5,
                    drag: Some(9)
                },
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled());
        assert_eq!(m.incoming(), None);
    }

    #[test]
    fn push_out_with_buttons_and_no_drag_is_a_wall() {
        let mut m = entered();
        assert_eq!(m.on(push_out(true, None)), vec![]);
        assert!(m.controlled());
    }

    #[test]
    fn push_out_ignored_when_not_controlled_even_with_offer() {
        let mut m = MacMachine::new();
        assert_eq!(m.on(push_out(true, Some(9))), vec![]);
    }

    #[test]
    fn release_all_during_incoming_cancels_first() {
        let mut m = entered_with_drag();
        assert_eq!(
            m.on(MacEvent::ReleaseAll),
            vec![
                MacAction::CancelDrag { drag: 7 },
                MacAction::ReleaseAllInjected,
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled());
        assert_eq!(m.incoming(), None);
    }

    #[test]
    fn disconnect_during_incoming_cancels_first() {
        let mut m = entered_with_drag();
        assert_eq!(
            m.on(MacEvent::Disconnected),
            vec![
                MacAction::CancelDrag { drag: 7 },
                MacAction::ReleaseAllInjected,
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled());
        assert_eq!(m.incoming(), None);
    }

    #[test]
    fn drag_ended_clears_incoming() {
        let mut m = entered_with_drag();
        assert_eq!(m.on(MacEvent::DragEnded { drag: 8 }), vec![]);
        assert_eq!(m.incoming(), Some(7), "another drag");
        assert_eq!(m.on(MacEvent::DragEnded { drag: 7 }), vec![]);
        assert_eq!(m.incoming(), None);
        assert!(m.controlled(), "the cursor stays on the Mac");
        // With the drag dropped, the next push out is a plain leave.
        assert_eq!(
            m.on(push_out(false, None)),
            vec![
                MacAction::ReleaseAllInjected,
                MacAction::SendLeave {
                    frac: 0.5,
                    drag: None
                },
                MacAction::StopControl
            ]
        );
    }

    #[test]
    fn enter_over_a_stale_incoming_drag_cancels_it_first() {
        let warp = MacAction::WarpTo {
            edge: Side::Left,
            frac: 0.4,
        };
        let enter = |drag| MacEvent::Enter {
            edge: Side::Left,
            frac: 0.4,
            drag,
        };
        let mut m = entered_with_drag();
        assert_eq!(
            m.on(enter(Some(8))),
            vec![
                MacAction::CancelDrag { drag: 7 },
                MacAction::WakeDisplay,
                warp.clone(),
                MacAction::StartControl,
                MacAction::BeginDrag { drag: 8 }
            ]
        );
        assert_eq!(m.incoming(), Some(8));
        assert_eq!(
            m.on(enter(None)),
            vec![
                MacAction::CancelDrag { drag: 8 },
                MacAction::WakeDisplay,
                warp.clone(),
                MacAction::StartControl
            ]
        );
        assert_eq!(m.incoming(), None);
        // The same drag again is not cancelled.
        let mut m = entered_with_drag();
        assert_eq!(
            m.on(enter(Some(7))),
            vec![
                MacAction::WakeDisplay,
                warp,
                MacAction::StartControl,
                MacAction::BeginDrag { drag: 7 }
            ]
        );
    }

    // ------------------------------------------------ late drags (R48)

    fn push_out_late() -> MacEvent {
        MacEvent::PushOutLate { frac: 0.5 }
    }

    #[test]
    fn left_held_push_out_waits_then_releases_before_leaving() {
        let mut m = entered();
        assert_eq!(
            m.on(push_out_late()),
            vec![
                MacAction::AwaitLateDrop,
                MacAction::ReleaseAllInjected,
                MacAction::StopControl
            ]
        );
        assert!(!m.controlled(), "input is ignored during the wait");
        assert!(m.awaiting_late_drop());
        assert_eq!(m.entry_edge(), None);
    }

    #[test]
    fn late_drop_leave_carries_the_caught_drag_or_none() {
        let mut m = entered();
        m.on(push_out_late());
        assert_eq!(
            m.on(MacEvent::LateDrop { drag: Some(11) }),
            vec![MacAction::SendLeave {
                frac: 0.5,
                drag: Some(11)
            }]
        );
        assert!(!m.awaiting_late_drop());
        assert_eq!(m.on(MacEvent::LateDrop { drag: None }), vec![], "once");
        let mut m = entered();
        m.on(push_out_late());
        assert_eq!(
            m.on(MacEvent::LateDrop { drag: None }),
            vec![MacAction::SendLeave {
                frac: 0.5,
                drag: None
            }]
        );
    }

    #[test]
    fn late_drop_without_a_wait_does_nothing() {
        let mut m = entered();
        assert_eq!(m.on(MacEvent::LateDrop { drag: Some(11) }), vec![]);
        assert!(m.controlled());
        let mut m = MacMachine::new();
        assert_eq!(m.on(push_out_late()), vec![], "not controlled");
        assert!(!m.awaiting_late_drop());
    }

    #[test]
    fn taking_the_cursor_back_during_the_wait_sends_no_leave() {
        for ev in [MacEvent::ReleaseAll, MacEvent::Disconnected] {
            let mut m = entered();
            m.on(push_out_late());
            m.on(ev.clone());
            assert!(!m.awaiting_late_drop(), "{ev:?}");
            assert_eq!(
                m.on(MacEvent::LateDrop { drag: Some(11) }),
                vec![],
                "{ev:?}"
            );
        }
    }

    #[test]
    fn late_push_out_over_an_incoming_drag_sends_it_home_at_once() {
        let mut m = entered_with_drag();
        assert_eq!(
            m.on(push_out_late()),
            going_home_actions(),
            "the PC's drag goes home; nothing waits"
        );
        assert!(!m.awaiting_late_drop());
    }

    fn going_home_actions() -> Vec<MacAction> {
        vec![
            MacAction::CancelDrag { drag: 7 },
            MacAction::ReleaseAllInjected,
            MacAction::SendLeave {
                frac: 0.5,
                drag: Some(7),
            },
            MacAction::StopControl,
        ]
    }
}
