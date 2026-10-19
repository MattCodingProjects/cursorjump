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

/// First-contact pairing: true only if the user confirms that `code` matches
/// the code shown on the other computer.
pub async fn ask_pairing(code: &str, peer_name: &str) -> bool {
    ask(
        MessageLevel::Info,
        "CursorJump — pair this computer?",
        format!(
            "Pair with “{peer_name}”?\n\n\
             The other screen should show the same code:\n\n\
             {code}\n\n\
             Click Yes only if both codes match."
        ),
    )
    .await
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
