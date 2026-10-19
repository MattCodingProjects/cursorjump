//! The settings window (eframe), run as its own process: `cursorjump --settings`.
//!
//! Every edit is written to the config file, which the running app polls and
//! hot-reloads. Edits are merged into the file as it is on disk at save time,
//! so a change the app made meanwhile (pause, pairing) is not overwritten.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use cj_core::config::Config;
use cj_core::proto::Side;
use eframe::egui;
use tracing::warn;

use crate::ui::dialogs;

/// How often the window re-reads the file for changes made elsewhere.
const DISK_POLL: Duration = Duration::from_millis(500);

pub fn run_settings(cfg_path: &Path, is_mac: bool) -> anyhow::Result<()> {
    let app = SettingsApp::new(cfg_path.to_path_buf(), is_mac);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CursorJump Settings")
            .with_inner_size([460.0, 380.0])
            .with_maximize_button(false)
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "CursorJump Settings",
        options,
        Box::new(|_| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("settings window: {e}"))
}

/// `disk` with every field the user changed (from `before` to `after`) taken from `after`.
fn merge_edits(before: &Config, after: &Config, mut disk: Config) -> Config {
    macro_rules! take_changed {
        ($($field:ident),* $(,)?) => {
            // Fails to compile when Config gains a field not listed here.
            let Config { $($field: _),* } = after;
            $(
                if before.$field != after.$field {
                    disk.$field = after.$field.clone();
                }
            )*
        };
    }
    take_changed!(
        mac_side,
        edge_offset,
        pointer_speed,
        scroll_speed,
        reverse_scroll,
        swap_ctrl_cmd,
        paused,
        autostart,
        reduce_wifi_hiccups,
        clipboard_max_bytes,
        paired_peer,
        manual_peer_addr,
    );
    disk
}

/// A setting the window can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    MacSide,
    EdgeOffset,
    PointerSpeed,
    ScrollSpeed,
    ReverseScroll,
    SwapCtrlCmd,
    Autostart,
    ReduceWifiHiccups,
}

impl Setting {
    /// Shown as a labelled row of the grid, rather than as a checkbox below it.
    fn in_grid(self) -> bool {
        use Setting::*;
        matches!(self, MacSide | EdgeOffset | PointerSpeed | ScrollSpeed)
    }
}

/// The settings shown on this computer, in order. The PC owns the pointer,
/// scroll and key settings; start at login on the Mac is set by its installer.
fn settings_shown(is_mac: bool) -> &'static [Setting] {
    use Setting::*;
    if is_mac {
        &[ReduceWifiHiccups]
    } else {
        &[
            MacSide,
            EdgeOffset,
            PointerSpeed,
            ScrollSpeed,
            ReverseScroll,
            SwapCtrlCmd,
            Autostart,
        ]
    }
}

/// A line saying where the settings this computer does not show are set.
fn pc_only_note(is_mac: bool) -> Option<&'static str> {
    is_mac.then_some("Pointer, scroll and key settings are set on the PC.")
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

struct SettingsApp {
    path: PathBuf,
    is_mac: bool,
    /// The config as last loaded or saved.
    saved: Config,
    /// The config as shown and edited.
    edit: Config,
    stamp: Option<SystemTime>,
    next_poll: Instant,
    confirm_forget: bool,
    error: Option<String>,
}

impl SettingsApp {
    fn new(path: PathBuf, is_mac: bool) -> Self {
        let (cfg, error) = match load(&path) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(unreadable(&e))),
        };
        SettingsApp {
            stamp: modified(&path),
            path,
            is_mac,
            saved: cfg.clone(),
            edit: cfg,
            next_poll: Instant::now() + DISK_POLL,
            confirm_forget: false,
            error,
        }
    }

    /// Writes pending edits once no pointer button is down (so a slider drag
    /// saves once, on release). A failure is shown in the window and in a box.
    fn save_if_edited(&mut self, ctx: &egui::Context) {
        if self.edit == self.saved || ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        if let Err(msg) = self.save_edits() {
            self.error = Some(msg.clone());
            dialogs::show_error(&msg);
        }
    }

    /// Merges the edits onto the file as it is on disk and saves it. A file
    /// that cannot be parsed is never written over: the edits are dropped.
    fn save_edits(&mut self) -> Result<(), String> {
        let disk = match load(&self.path) {
            Ok(disk) => disk,
            Err(e) => {
                warn!("not saving settings: {e:#}");
                // Do not retry every frame: go back to what was shown before.
                self.edit = self.saved.clone();
                return Err(unreadable(&e));
            }
        };
        let mut result = Ok(());
        if self.edit.autostart != self.saved.autostart
            && let Err(e) = apply_autostart(self.edit.autostart)
        {
            result = Err(format!("Could not change start at login: {e:#}"));
            self.edit.autostart = self.saved.autostart;
        }
        let merged = merge_edits(&self.saved, &self.edit, disk).clamped();
        match merged.save(&self.path) {
            Ok(()) => {
                self.stamp = modified(&self.path);
                self.saved = merged.clone();
                self.edit = merged;
                result
            }
            Err(e) => {
                warn!("saving settings: {e}");
                // Do not retry every frame; show what was there before.
                self.edit = self.saved.clone();
                Err(format!("Could not save settings: {e}"))
            }
        }
    }

    /// Picks up changes made by the app (e.g. pause from the tray) while no edit is pending.
    fn poll_disk(&mut self) {
        if Instant::now() < self.next_poll {
            return;
        }
        self.next_poll = Instant::now() + DISK_POLL;
        let stamp = modified(&self.path);
        if stamp != self.stamp && self.edit == self.saved {
            self.stamp = stamp;
            match load(&self.path) {
                Ok(cfg) => {
                    self.saved = cfg;
                    self.edit = self.saved.clone();
                    self.error = None;
                }
                Err(e) => self.error = Some(unreadable(&e)),
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let e = &mut self.edit;
        if let Some(note) = pc_only_note(self.is_mac) {
            ui.label(note);
            ui.add_space(8.0);
        }
        let shown = settings_shown(self.is_mac);
        if shown.iter().any(|s| s.in_grid()) {
            egui::Grid::new("settings")
                .num_columns(2)
                .spacing([16.0, 12.0])
                .show(ui, |ui| {
                    for &setting in shown.iter().filter(|s| s.in_grid()) {
                        grid_row(ui, e, setting);
                        ui.end_row();
                    }
                });
            ui.add_space(8.0);
        }
        for &setting in shown.iter().filter(|s| !s.in_grid()) {
            checkbox(ui, e, setting);
        }
    }

    fn pairing(&mut self, ui: &mut egui::Ui) {
        let Some(fp) = self.edit.paired_peer.clone() else {
            self.confirm_forget = false;
            ui.label("Not paired. Pairing starts by itself when both computers find each other.");
            return;
        };
        ui.label(format!(
            "Paired (key {}…)",
            fp.get(..12).unwrap_or(fp.as_str())
        ));
        if !self.confirm_forget {
            if ui.button("Forget paired computer").clicked() {
                self.confirm_forget = true;
            }
            return;
        }
        ui.label("Forget it? Both computers will have to confirm a code again.");
        ui.horizontal(|ui| {
            if ui.button("Forget").clicked() {
                self.edit.paired_peer = None;
                self.confirm_forget = false;
            }
            if ui.button("Cancel").clicked() {
                self.confirm_forget = false;
            }
        });
    }
}

fn grid_row(ui: &mut egui::Ui, e: &mut Config, setting: Setting) {
    match setting {
        Setting::MacSide => {
            ui.label("Mac is on the");
            ui.horizontal(|ui| {
                for (side, name) in [
                    (Side::Left, "Left"),
                    (Side::Right, "Right"),
                    (Side::Top, "Top"),
                    (Side::Bottom, "Bottom"),
                ] {
                    ui.radio_value(&mut e.mac_side, side, name);
                }
            });
        }
        Setting::EdgeOffset => {
            ui.label("Edge offset");
            let mut percent = e.edge_offset * 100.0;
            let slider = egui::Slider::new(&mut percent, -50.0..=50.0)
                .suffix(" %")
                .fixed_decimals(0);
            if ui.add(slider).changed() {
                e.edge_offset = percent / 100.0;
            }
        }
        Setting::PointerSpeed => {
            ui.label("Pointer speed");
            ui.add(speed_slider(&mut e.pointer_speed));
        }
        Setting::ScrollSpeed => {
            ui.label("Scroll speed");
            ui.add(speed_slider(&mut e.scroll_speed));
        }
        _ => unreachable!("{setting:?} is a checkbox"),
    }
}

fn checkbox(ui: &mut egui::Ui, e: &mut Config, setting: Setting) {
    let (value, text) = match setting {
        Setting::ReverseScroll => (&mut e.reverse_scroll, "Reverse scroll direction"),
        Setting::SwapCtrlCmd => (
            &mut e.swap_ctrl_cmd,
            "Swap Ctrl and Cmd (Ctrl+C copies on the Mac)",
        ),
        Setting::Autostart => (&mut e.autostart, "Start at login"),
        Setting::ReduceWifiHiccups => (
            &mut e.reduce_wifi_hiccups,
            "Reduce Wi-Fi hiccups (turns off AirDrop while connected)",
        ),
        _ => unreachable!("{setting:?} is a grid row"),
    };
    ui.checkbox(value, text);
}

fn speed_slider(v: &mut f32) -> egui::Slider<'_> {
    egui::Slider::new(v, 0.25..=4.0)
        .logarithmic(true)
        .suffix("×")
        .fixed_decimals(2)
}

/// The file's config (defaults if there is no file), with "start at login"
/// read from where it really lives. Errs if the file cannot be parsed.
fn load(path: &Path) -> anyhow::Result<Config> {
    #[allow(unused_mut, reason = "only Windows reads the real autostart state")]
    let mut cfg = Config::try_load(path)?.unwrap_or_default();
    #[cfg(windows)]
    {
        cfg.autostart = crate::win::autostart::autostart_enabled();
    }
    Ok(cfg)
}

fn unreadable(e: &anyhow::Error) -> String {
    format!(
        "The settings file can't be read, so changes are not saved:\n{e:#}\n\n\
         Fix or delete the file, then reopen this window."
    )
}

fn apply_autostart(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    crate::win::autostart::set_autostart(enabled)?;
    // The Mac's window never shows start at login: its installer registers
    // the LaunchAgent (`--enable-autostart`), and the app ignores `autostart`.
    #[cfg(not(windows))]
    let _ = enabled;
    Ok(())
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_disk();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("CursorJump");
            ui.add_space(8.0);
            self.controls(ui);
            ui.separator();
            self.pairing(ui);
            if let Some(err) = &self.error {
                ui.separator();
                ui.colored_label(egui::Color32::from_rgb(0xd0, 0x3a, 0x2f), err);
            }
        });
        let ctx = ui.ctx().clone();
        self.save_if_edited(&ctx);
        ctx.request_repaint_after(DISK_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_changes_made_on_disk_meanwhile() {
        let before = Config::default();
        let after = Config {
            pointer_speed: 2.0,
            ..before.clone()
        };
        let disk = Config {
            paused: true,
            paired_peer: Some("ab".into()),
            ..before.clone()
        };
        let merged = merge_edits(&before, &after, disk);
        assert_eq!(merged.pointer_speed, 2.0);
        assert!(merged.paused);
        assert_eq!(merged.paired_peer.as_deref(), Some("ab"));
    }

    #[test]
    fn merge_applies_forget() {
        let before = Config {
            paired_peer: Some("ab".into()),
            ..Config::default()
        };
        let after = Config {
            paired_peer: None,
            ..before.clone()
        };
        let merged = merge_edits(&before, &after, before.clone());
        assert_eq!(merged.paired_peer, None);
    }

    #[test]
    fn save_never_overwrites_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let broken = "paired_peer = \"ab\npaused = ";
        std::fs::write(&path, broken).unwrap();
        let mut app = SettingsApp::new(path.clone(), false);
        assert!(
            app.error.is_some(),
            "the window says the file is unreadable"
        );
        app.edit.pointer_speed = 2.0;
        assert!(app.save_edits().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert_eq!(
            app.edit, app.saved,
            "edits dropped, not retried every frame"
        );
    }

    #[test]
    fn save_merges_edits_onto_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "pointer_speed = 1.5\n").unwrap();
        let mut app = SettingsApp::new(path.clone(), false);
        assert_eq!(app.error, None);
        // Meanwhile the app pauses crossing.
        std::fs::write(&path, "pointer_speed = 1.5\npaused = true\n").unwrap();
        app.edit.scroll_speed = 2.0;
        app.save_edits().unwrap();
        let on_disk = Config::try_load(&path).unwrap().unwrap();
        assert_eq!(on_disk.scroll_speed, 2.0);
        assert_eq!(on_disk.pointer_speed, 1.5);
        assert!(on_disk.paused);
    }

    #[test]
    fn the_mac_shows_only_what_the_mac_uses() {
        assert_eq!(settings_shown(true), [Setting::ReduceWifiHiccups]);
        assert!(pc_only_note(true).is_some());
    }

    #[test]
    fn the_pc_shows_every_pc_setting_and_no_mac_one() {
        assert_eq!(
            settings_shown(false),
            [
                Setting::MacSide,
                Setting::EdgeOffset,
                Setting::PointerSpeed,
                Setting::ScrollSpeed,
                Setting::ReverseScroll,
                Setting::SwapCtrlCmd,
                Setting::Autostart,
            ]
        );
        assert_eq!(pc_only_note(false), None);
    }

    #[test]
    fn merge_without_edits_is_disk() {
        let c = Config::default();
        let disk = Config {
            mac_side: Side::Left,
            ..Config::default()
        };
        assert_eq!(merge_edits(&c, &c, disk.clone()), disk);
    }
}
