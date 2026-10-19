//! The wire protocol: newline-terminated ASCII commands, one reply line
//! (`ok` or `err <reason>`) each. Who may connect is decided here too.

/// The longest line accepted, without its newline.
pub const MAX_LINE: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Hold awdl0 down for as long as this connection is open.
    AwdlDown,
    /// Release this connection's hold.
    AwdlUp,
}

/// What the front of a connection's input holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Command(Command),
    /// No complete line yet.
    Incomplete,
    /// An unknown command or an over-long line: close the connection.
    Invalid,
}

/// Takes the next complete line off the front of `buf`.
pub fn next_command(buf: &mut Vec<u8>) -> Next {
    let Some(end) = buf.iter().position(|&b| b == b'\n') else {
        return if buf.len() > MAX_LINE {
            Next::Invalid
        } else {
            Next::Incomplete
        };
    };
    let next = match &buf[..end] {
        b"awdl-down" => Next::Command(Command::AwdlDown),
        b"awdl-up" => Next::Command(Command::AwdlUp),
        // Lines longer than MAX_LINE included.
        _ => Next::Invalid,
    };
    buf.drain(..=end);
    next
}

/// The reply line for a command's result.
pub fn reply(result: &Result<(), String>) -> String {
    match result {
        Ok(()) => "ok\n".into(),
        Err(reason) => format!("err {}\n", reason.replace(['\r', '\n'], " ")),
    }
}

/// Only the user the daemon was installed for, and root, may connect.
pub fn peer_allowed(peer_uid: u32, allowed_uid: u32) -> bool {
    peer_uid == allowed_uid || peer_uid == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn the_two_commands_parse() {
        let mut b = buf("awdl-down\nawdl-up\n");
        assert_eq!(next_command(&mut b), Next::Command(Command::AwdlDown));
        assert_eq!(next_command(&mut b), Next::Command(Command::AwdlUp));
        assert_eq!(next_command(&mut b), Next::Incomplete);
        assert!(b.is_empty(), "both lines consumed");
    }

    #[test]
    fn a_partial_line_waits_for_the_rest() {
        let mut b = buf("awdl-d");
        assert_eq!(next_command(&mut b), Next::Incomplete);
        assert_eq!(b, buf("awdl-d"), "nothing consumed");
        b.extend_from_slice(b"own\n");
        assert_eq!(next_command(&mut b), Next::Command(Command::AwdlDown));
    }

    #[test]
    fn anything_else_is_invalid() {
        for line in [
            "awdl-down \n",
            " awdl-down\n",
            "AWDL-DOWN\n",
            "awdl-down\r\n",
            "awdl\n",
            "\n",
            "ifconfig en0 down\n",
            "awdl-down\0\n",
        ] {
            assert_eq!(next_command(&mut buf(line)), Next::Invalid, "{line:?}");
        }
    }

    #[test]
    fn an_over_long_line_is_invalid_even_before_its_newline() {
        let at_limit = "x".repeat(MAX_LINE);
        assert_eq!(next_command(&mut buf(&at_limit)), Next::Incomplete);
        let over = "x".repeat(MAX_LINE + 1);
        assert_eq!(next_command(&mut buf(&over)), Next::Invalid);
        assert_eq!(next_command(&mut buf(&format!("{over}\n"))), Next::Invalid);
    }

    #[test]
    fn replies_are_single_lines() {
        assert_eq!(reply(&Ok(())), "ok\n");
        assert_eq!(
            reply(&Err("Operation not permitted".into())),
            "err Operation not permitted\n"
        );
        assert_eq!(
            reply(&Err("two\nlines".into())),
            "err two lines\n",
            "a reason never breaks the line framing"
        );
    }

    #[test]
    fn only_the_installing_user_and_root_may_connect() {
        assert!(peer_allowed(501, 501));
        assert!(peer_allowed(0, 501));
        assert!(!peer_allowed(502, 501));
        assert!(!peer_allowed(u32::MAX, 501));
    }
}
