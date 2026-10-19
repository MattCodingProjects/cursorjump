//! Tray icon (Windows) / menu-bar icon (macOS), shared by both apps.
//!
//! The event loop runs on the main thread. The app pushes `TrayModel`s from
//! its own thread and receives menu clicks as `TrayCmd`s. When the app drops
//! its model sender, the icon is removed and the process exits.

use std::sync::Mutex;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use cj_core::latency::{LatencyStats, WifiVerdict};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tracing::{error, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// What the tray shows.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayModel {
    pub status: String,
    /// One or two lines (the second is a hint, see `latency_text`).
    pub latency: String,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    TogglePause,
    OpenSettings,
    ForgetPairing,
    Quit,
}

pub const SPIKY_HINT: &str = "Wi-Fi is spiky — an Ethernet adapter would help";

/// After Quit, the app gets this long to close the model channel before the
/// tray exits the process anyway.
const QUIT_GRACE: Duration = Duration::from_secs(3);

const ICON_SIZE: u32 = 32;

/// The latency readout: the rolling summary, plus the Ethernet hint on a
/// second line when the link is spiky.
pub fn latency_text(stats: &LatencyStats) -> String {
    match stats.verdict() {
        WifiVerdict::Good => stats.summary(),
        WifiVerdict::Spiky => format!("{}\n{SPIKY_HINT}", stats.summary()),
    }
}

/// The icon drawn in code: a rounded square (blue, grey while paused) with a
/// white arrow pointing right. RGBA, `ICON_SIZE` square.
fn icon_rgba(paused: bool) -> Vec<u8> {
    let n = ICON_SIZE as i32;
    let bg: [u8; 4] = if paused {
        [0x8a, 0x8a, 0x8a, 0xff]
    } else {
        [0x2d, 0x7f, 0xf9, 0xff]
    };
    let white = [0xff; 4];
    let radius = 6;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            // Distance into the corner region, for the rounded corners.
            let cx = (radius - x).max(x - (n - 1 - radius)).max(0);
            let cy = (radius - y).max(y - (n - 1 - radius)).max(0);
            let inside = cx * cx + cy * cy <= radius * radius;
            let shaft = (8..20).contains(&x) && (14..18).contains(&y);
            // Arrow head: a triangle from x = 18 (half-height 8) to x = 26 (point).
            let head = (18..26).contains(&x) && (y - 16).abs() < 26 - x;
            px.extend_from_slice(match (inside, shaft || head) {
                (false, _) => &[0, 0, 0, 0],
                (true, true) => &white,
                (true, false) => &bg,
            });
        }
    }
    px
}

fn icon(paused: bool) -> Option<Icon> {
    Icon::from_rgba(icon_rgba(paused), ICON_SIZE, ICON_SIZE)
        .inspect_err(|e| error!("tray icon: {e}"))
        .ok()
}

enum TrayEvent {
    Model(TrayModel),
    Menu(MenuEvent),
    /// The app dropped its model sender.
    AppGone,
}

struct Items {
    menu: Menu,
    status: MenuItem,
    latency: MenuItem,
    hint: MenuItem,
    hint_shown: bool,
    pause: CheckMenuItem,
    settings: MenuItem,
    forget: MenuItem,
    quit: MenuItem,
}

impl Items {
    fn new() -> Self {
        let items = Items {
            menu: Menu::new(),
            status: MenuItem::new("Starting…", false, None),
            latency: MenuItem::new("", false, None),
            hint: MenuItem::new("", false, None),
            hint_shown: false,
            pause: CheckMenuItem::new("Pause crossing", true, false, None),
            settings: MenuItem::new("Settings…", true, None),
            forget: MenuItem::new("Forget paired computer…", true, None),
            quit: MenuItem::new("Quit CursorJump", true, None),
        };
        let appended = items.menu.append_items(&[
            &items.status,
            &items.latency,
            &PredefinedMenuItem::separator(),
            &items.pause,
            &items.settings,
            &items.forget,
            &PredefinedMenuItem::separator(),
            &items.quit,
        ]);
        if let Err(e) = appended {
            error!("tray menu: {e}");
        }
        items
    }

    fn command(&self, ev: &MenuEvent) -> Option<TrayCmd> {
        let id = ev.id();
        if id == self.pause.id() {
            Some(TrayCmd::TogglePause)
        } else if id == self.settings.id() {
            Some(TrayCmd::OpenSettings)
        } else if id == self.forget.id() {
            Some(TrayCmd::ForgetPairing)
        } else if id == self.quit.id() {
            Some(TrayCmd::Quit)
        } else {
            None
        }
    }

    fn show(&mut self, m: &TrayModel) {
        self.status.set_text(&m.status);
        let mut lines = m.latency.lines();
        self.latency.set_text(lines.next().unwrap_or(""));
        let hint = lines.next();
        if let Some(h) = hint {
            self.hint.set_text(h);
        }
        // Menus cannot hide an item, so the hint line is inserted and removed.
        let result = match (hint.is_some(), self.hint_shown) {
            (true, false) => self.menu.insert(&self.hint, 2),
            (false, true) => self.menu.remove(&self.hint),
            _ => Ok(()),
        };
        match result {
            Ok(()) => self.hint_shown = hint.is_some(),
            Err(e) => warn!("tray menu: {e}"),
        }
        self.pause.set_checked(m.paused);
    }
}

fn tooltip(m: &TrayModel) -> String {
    format!("CursorJump — {}", m.status)
}

/// The tray's event loop, created before anything else touches Raw Input.
pub struct TrayLoop {
    event_loop: EventLoop<TrayEvent>,
}

impl TrayLoop {
    /// Must run on the main thread. On Windows, create it before the input
    /// thread: Raw Input registration is per process, and tao registers the
    /// mouse for its own window when the loop is built. This removes that
    /// registration so the input thread's (made later) is the only one.
    pub fn new() -> Self {
        #[allow(unused_mut, reason = "mutated on macOS only")]
        let mut event_loop = EventLoopBuilder::<TrayEvent>::with_user_event().build();
        event_loop.set_device_event_filter(tao::event_loop::DeviceEventFilter::Always);
        #[cfg(target_os = "macos")]
        {
            use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
            event_loop.set_activation_policy(ActivationPolicy::Accessory);
        }
        TrayLoop { event_loop }
    }

    /// Runs the tray until the app drops `model_rx`'s sender, then exits the process.
    pub fn run(self, model_rx: Receiver<TrayModel>, on_menu: impl Fn(TrayCmd) + 'static) -> ! {
        let proxy = self.event_loop.create_proxy();
        let menu_proxy = Mutex::new(proxy.clone());
        MenuEvent::set_event_handler(Some(move |e| {
            if let Ok(p) = menu_proxy.lock() {
                let _ = p.send_event(TrayEvent::Menu(e));
            }
        }));
        let forwarder = std::thread::Builder::new()
            .name("cj-tray-model".into())
            .spawn(move || {
                for m in model_rx {
                    if proxy.send_event(TrayEvent::Model(m)).is_err() {
                        return;
                    }
                }
                let _ = proxy.send_event(TrayEvent::AppGone);
            });
        if let Err(e) = forwarder {
            error!("tray model thread failed to start: {e}");
        }

        let mut items = Items::new();
        let mut tray: Option<TrayIcon> = None;
        let mut model: Option<TrayModel> = None;
        let mut quit_by: Option<Instant> = None;
        self.event_loop.run(move |event, _, control_flow| {
            *control_flow = quit_by.map_or(ControlFlow::Wait, ControlFlow::WaitUntil);
            match event {
                // macOS requires the icon to be created once the loop runs.
                Event::NewEvents(StartCause::Init) => {
                    let built = TrayIconBuilder::new()
                        .with_menu(Box::new(items.menu.clone()))
                        .with_tooltip("CursorJump")
                        .with_icon(icon(false).expect("built-in icon is valid"))
                        .build();
                    match built {
                        Ok(t) => tray = Some(t),
                        Err(e) => error!("tray icon failed: {e}"),
                    }
                    // A model that arrived before the icon existed.
                    if let (Some(t), Some(m)) = (&tray, &model) {
                        apply(t, m);
                    }
                }
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                    if quit_by.is_some_and(|t| Instant::now() >= t) {
                        warn!("app did not stop in time; exiting anyway");
                        // The input thread may be stuck with the cursor hidden.
                        #[cfg(windows)]
                        crate::win::cursor::restore_system_cursors();
                        tray.take();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                Event::UserEvent(TrayEvent::Model(m)) => {
                    let paused_changed = model.as_ref().is_none_or(|old| old.paused != m.paused);
                    items.show(&m);
                    if let Some(t) = &tray {
                        let _ = t.set_tooltip(Some(tooltip(&m)));
                        if paused_changed {
                            let _ = t.set_icon(icon(m.paused));
                        }
                    }
                    model = Some(m);
                }
                Event::UserEvent(TrayEvent::Menu(e)) => {
                    let Some(cmd) = items.command(&e) else {
                        return;
                    };
                    // The check mark follows the app's state, not the click.
                    items
                        .pause
                        .set_checked(model.as_ref().is_some_and(|m| m.paused));
                    if cmd == TrayCmd::Quit {
                        quit_by.get_or_insert(Instant::now() + QUIT_GRACE);
                        *control_flow = quit_by.map_or(ControlFlow::Wait, ControlFlow::WaitUntil);
                    }
                    on_menu(cmd);
                }
                Event::UserEvent(TrayEvent::AppGone) => {
                    // Remove the icon now: process exit skips destructors.
                    tray.take();
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        })
    }
}

fn apply(t: &TrayIcon, m: &TrayModel) {
    let _ = t.set_tooltip(Some(tooltip(m)));
    let _ = t.set_icon(icon(m.paused));
}

/// Runs the tray on the main thread until the app drops `model_rx`'s sender.
#[allow(
    dead_code,
    reason = "the macOS app's entry point; the PC uses TrayLoop"
)]
pub fn run_tray(model_rx: Receiver<TrayModel>, on_menu: impl Fn(TrayCmd) + 'static) -> ! {
    TrayLoop::new().run(model_rx, on_menu)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(rtts_ms: &[u64]) -> LatencyStats {
        let base = Instant::now();
        let mut s = LatencyStats::new();
        for (i, ms) in rtts_ms.iter().enumerate() {
            s.record(
                base + Duration::from_secs(i as u64),
                Duration::from_millis(*ms),
            );
        }
        s
    }

    #[test]
    fn latency_text_without_samples() {
        assert_eq!(latency_text(&LatencyStats::new()), "Latency – ms");
    }

    #[test]
    fn latency_text_good_link_is_one_line() {
        assert_eq!(
            latency_text(&stats(&[3, 4, 50, 4])),
            "Latency 4 ms · 1 spikes/min"
        );
    }

    #[test]
    fn latency_text_spiky_link_adds_ethernet_hint() {
        assert_eq!(
            latency_text(&stats(&[3, 60, 70, 80, 4])),
            "Latency 60 ms · 3 spikes/min\nWi-Fi is spiky — an Ethernet adapter would help"
        );
    }

    #[test]
    fn icon_is_square_rgba_with_clear_corners_and_white_arrow() {
        for paused in [false, true] {
            let px = icon_rgba(paused);
            assert_eq!(px.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            let at = |x: u32, y: u32| {
                let i = ((y * ICON_SIZE + x) * 4) as usize;
                [px[i], px[i + 1], px[i + 2], px[i + 3]]
            };
            assert_eq!(at(0, 0)[3], 0, "corner is transparent");
            assert_eq!(at(12, 15), [0xff; 4], "arrow shaft is white");
            assert_eq!(at(16, 4)[3], 0xff, "background is opaque");
            assert_ne!(at(16, 4), [0xff; 4]);
        }
        assert_ne!(icon_rgba(false), icon_rgba(true));
    }
}
