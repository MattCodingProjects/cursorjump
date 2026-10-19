//! Native yes/no dialogs (rfd), shared by both apps.

use rfd::{AsyncMessageDialog, MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};

async fn ask(level: MessageLevel, title: &str, text: String) -> bool {
    AsyncMessageDialog::new()
        .set_level(level)
        .set_title(title)
        .set_description(text)
        .set_buttons(MessageButtons::YesNo)
        .show()
        .await
        == MessageDialogResult::Yes
}

const PAIR_TITLE: &str = "CursorJump — pair this computer?";

fn pair_text(code: &str, peer_name: &str) -> String {
    format!(
        "Pair with “{peer_name}”?\n\n\
         The other screen should show the same code:\n\n\
         {code}\n\n\
         Click Yes only if both codes match."
    )
}

/// First-contact pairing: true only if the user confirms that `code` matches
/// the code shown on the other computer.
///
/// On Windows this is a topmost message box that takes the foreground (rfd's
/// box can open behind other windows). It runs on its own thread, never on
/// an async worker.
#[cfg(windows)]
pub async fn ask_pairing(code: &str, peer_name: &str) -> bool {
    let text = pair_text(code, peer_name);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("cj-pair-dialog".into())
        .spawn(move || {
            let _ = tx.send(windows_yes_no(PAIR_TITLE, &text));
        });
    if let Err(e) = spawned {
        tracing::error!("pairing dialog failed to start: {e}");
        return false;
    }
    rx.await.unwrap_or(false)
}

#[cfg(windows)]
fn windows_yes_no(title: &str, text: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };
    use windows::core::HSTRING;
    // SAFETY: both strings outlive the call; there is no owner window.
    let answer = unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(title),
            MB_YESNO | MB_TOPMOST | MB_SETFOREGROUND | MB_ICONQUESTION,
        )
    };
    answer == IDYES
}

/// First-contact pairing: true only if the user confirms that `code` matches
/// the code shown on the other computer.
#[cfg(not(windows))]
pub async fn ask_pairing(code: &str, peer_name: &str) -> bool {
    ask(MessageLevel::Info, PAIR_TITLE, pair_text(code, peer_name)).await
}

/// Confirms "Forget paired computer" from the tray.
pub async fn confirm_forget() -> bool {
    ask(
        MessageLevel::Warning,
        "CursorJump — forget paired computer?",
        "Forget the paired computer?\n\n\
         The two computers will have to pair again, confirming a code on both screens."
            .to_string(),
    )
    .await
}

/// Blocking error box, for failures the user would otherwise never see
/// (release builds have no console).
pub fn show_error(text: &str) {
    MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title("CursorJump")
        .set_description(text)
        .set_buttons(MessageButtons::Ok)
        .show();
}
