//! Hiding and restoring the system cursors while the pointer is on the Mac.
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, DestroyCursor, OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_HELP, OCR_IBEAM, OCR_NO,
    OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW, OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT,
    SPI_SETCURSORS, SYSTEM_CURSOR_ID, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor,
    SystemParametersInfoW,
};

const SYSTEM_CURSORS: [SYSTEM_CURSOR_ID; 14] = [
    OCR_NORMAL,
    OCR_IBEAM,
    OCR_WAIT,
    OCR_CROSS,
    OCR_UP,
    OCR_SIZENWSE,
    OCR_SIZENESW,
    OCR_SIZEWE,
    OCR_SIZENS,
    OCR_SIZEALL,
    OCR_NO,
    OCR_HAND,
    OCR_APPSTARTING,
    OCR_HELP,
];

/// 32x32 monochrome mask: 32 rows of 4 bytes.
const MASK_BYTES: usize = 32 * 32 / 8;

/// Reload the user's cursor scheme, undoing `hide_system_cursors`.
pub fn restore_system_cursors() {
    // SAFETY: SPI_SETCURSORS takes no pointer; fWinIni 0 means no broadcast or persist.
    if let Err(e) = unsafe {
        SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    } {
        tracing::warn!("restore_system_cursors failed: {e}");
    }
}

/// Replace every standard system cursor with a fully transparent one.
pub fn hide_system_cursors() {
    let and_mask = [0xFFu8; MASK_BYTES];
    let xor_mask = [0x00u8; MASK_BYTES];
    // SAFETY: GetModuleHandleW(None) returns this executable's handle.
    let hinst = unsafe { GetModuleHandleW(None) }.ok().map(Into::into);
    for id in SYSTEM_CURSORS {
        // SAFETY: both masks are 32x32 1-bpp buffers that outlive the call.
        let cursor = match unsafe {
            CreateCursor(
                hinst,
                0,
                0,
                32,
                32,
                and_mask.as_ptr().cast(),
                xor_mask.as_ptr().cast(),
            )
        } {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("CreateCursor failed: {e}");
                continue;
            }
        };
        // SetSystemCursor takes ownership on success; on failure we still own it.
        if let Err(e) = unsafe { SetSystemCursor(cursor, id) } {
            tracing::warn!("SetSystemCursor({}) failed: {e}", id.0);
            let _ = unsafe { DestroyCursor(cursor) };
        }
    }
}
