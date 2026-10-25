//! File drag-and-drop model shared by the PC and the Mac client.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use unicode_normalization::UnicodeNormalization;

/// Random, chosen by the origin; never 0.
pub type DragId = u64;

/// Deepest relative path a received item may contain.
pub const MAX_PATH_DEPTH: usize = 64;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Dir,
}

/// One top-level item of a drag, as offered before any bytes move.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DragItem {
    /// "" when not known until fetched.
    pub name: String,
    pub kind: ItemKind,
    pub size: Option<u64>,
}

/// Starts the transfer of one item.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FilesHeader {
    pub drag: DragId,
    pub item: u32,
    pub name: String,
    pub kind: ItemKind,
    pub total_bytes: u64,
    pub entries: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Relative to the item root; [] is the root itself.
    pub path: Vec<String>,
    pub kind: ItemKind,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum FailReason {
    Disconnected,
    Sleep,
    NoSpace,
    Unreadable { name: String },
    Unwritable,
    Changed { name: String },
    Cancelled,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum DragOutcome {
    Done,
    NotDropped,
    Failed(FailReason),
}

const MAX_NAME_LEN: usize = 255;
const MAX_EXT_BYTES: usize = 16;

/// A file or folder name from the peer, made safe to create on either OS:
/// NFC, no characters Windows forbids, no trailing dots or spaces, no
/// device names, never empty, `.` or `..`, and at most 255 UTF-8 bytes and
/// 255 UTF-16 units (cut before the extension).
pub fn sanitize_component(name: &str) -> String {
    let mut s: String = name
        .nfc()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'..='\u{1f}' => '_',
            c => c,
        })
        .collect();
    s.truncate(s.trim_end_matches(['.', ' ']).len());
    if is_reserved(&s) {
        s.insert(0, '_');
    }
    if s.is_empty() || s == "." || s == ".." {
        s = "_".into();
    }
    truncate_keeping_extension(s)
}

/// `CON`, `com1.txt`, `Lpt9.tar.gz` and the like: Windows matches device
/// names on the part before the first dot, ignoring trailing spaces.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    matches!(
        stem.as_bytes(),
        b"CON"
            | b"PRN"
            | b"AUX"
            | b"NUL"
            | [b'C', b'O', b'M', b'1'..=b'9']
            | [b'L', b'P', b'T', b'1'..=b'9']
    )
}

fn truncate_keeping_extension(s: String) -> String {
    if s.len() <= MAX_NAME_LEN && s.encode_utf16().count() <= MAX_NAME_LEN {
        return s;
    }
    let ext = match s.rfind('.') {
        Some(i) if i > 0 && s.len() - i <= MAX_EXT_BYTES => &s[i..],
        _ => "",
    };
    let (mut bytes, mut units) = (ext.len(), ext.encode_utf16().count());
    let mut out = String::with_capacity(MAX_NAME_LEN);
    for c in s[..s.len() - ext.len()].chars() {
        bytes += c.len_utf8();
        units += c.len_utf16();
        if bytes > MAX_NAME_LEN || units > MAX_NAME_LEN {
            break;
        }
        out.push(c);
    }
    // The cut may leave a trailing dot or space where the name now ends.
    if ext.is_empty() {
        out.truncate(out.trim_end_matches(['.', ' ']).len());
    }
    if out.is_empty() {
        out.push('_');
    }
    out.push_str(ext);
    out
}

/// A relative path from the peer, each component sanitized, for joining
/// under a destination folder. `None` if it is deeper than
/// [`MAX_PATH_DEPTH`] or has a `.`, `..` or empty component.
pub fn safe_rel_path(path: &[String]) -> Option<PathBuf> {
    if path.len() > MAX_PATH_DEPTH || path.iter().any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(path.iter().map(|c| sanitize_component(c)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_component_cases() {
        for (input, want) in [
            ("a:b.txt", "a_b.txt"),
            ("CON.txt", "_CON.txt"),
            ("lpt3", "_lpt3"),
            ("trailing. ", "trailing"),
            ("e\u{301}", "\u{e9}"),
            ("", "_"),
            ("..", "_"),
            ("emoji🍎.png", "emoji🍎.png"),
            ("Lpt9.tar.gz", "_Lpt9.tar.gz"),
            ("COM10", "COM10"),
            ("console", "console"),
            ("a\u{0}b\u{1f}c", "a_b_c"),
            ("...", "_"),
        ] {
            assert_eq!(sanitize_component(input), want, "input {input:?}");
        }

        let long = format!("{}.mp4", "x".repeat(296));
        assert_eq!(long.chars().count(), 300);
        let got = sanitize_component(&long);
        assert!(got.ends_with(".mp4"), "{got}");
        assert!(got.len() <= 255, "{} bytes", got.len());
        assert!(got.encode_utf16().count() <= 255);
    }

    #[test]
    fn safe_rel_path_rejects_escape() {
        let parts = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(safe_rel_path(&parts(&["..", "x"])), None);
        assert_eq!(safe_rel_path(&parts(&["a", ""])), None);
        assert_eq!(safe_rel_path(&parts(&["."])), None);
        assert_eq!(
            safe_rel_path(&vec!["d".to_string(); MAX_PATH_DEPTH + 1]),
            None
        );

        let p = safe_rel_path(&parts(&["Photos", "a:b.jpg"])).unwrap();
        assert!(p.is_relative());
        assert_eq!(p, std::path::Path::new("Photos").join("a_b.jpg"));

        for abs in [
            &["/etc", "passwd"][..],
            &["C:", "x"],
            &["\\\\server", "share"],
        ] {
            assert!(safe_rel_path(&parts(abs)).unwrap().is_relative(), "{abs:?}");
        }
        assert_eq!(
            safe_rel_path(&vec!["d".to_string(); MAX_PATH_DEPTH]).map(|p| p.components().count()),
            Some(MAX_PATH_DEPTH)
        );
    }

    #[test]
    fn long_name_without_extension_is_cut_on_a_char_boundary() {
        let got = sanitize_component(&"東".repeat(200));
        assert_eq!(got, "東".repeat(85));
        let got = sanitize_component(&format!("{}{}", "a".repeat(254), "🍎🍎"));
        assert!(got.len() <= 255 && got.encode_utf16().count() <= 255);
        assert_eq!(got, "a".repeat(254));
    }
}
