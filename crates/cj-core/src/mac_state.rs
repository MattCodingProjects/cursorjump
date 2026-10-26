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
}

#[derive(Debug, Default)]
pub struct MacMachine {
    controlled: bool,
    entry: Option<Side>,
    /// The PC's drag riding on the Mac.
    incoming: Option<DragId>,
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
            MacEvent::DragEnded { drag } => {
                if self.incoming == Some(drag) {
                    self.incoming = None;
                }
                vec![]
            }
            // The PC took the cursor back (hotkey, hook lost, pause, quit).
            MacEvent::ReleaseAll => {
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
}
