//! Dragged web links: Discord and many web pages drag an image out as an
//! InternetShortcut (`image.png.url`, `[InternetShortcut] URL=…`), not its
//! bytes. The sending side downloads the link's target and offers that
//! instead (ruling R10); a failed download fails the item `Unreadable`
//! (R10a).
//!
//! Plain GETs over http or https only (redirects too, at most 5), with no
//! cookies or credentials and `User-Agent: CursorJump`. TLS is rustls on
//! ring; certificates are checked against the OS trust store
//! (rustls-platform-verifier: CryptoAPI on Windows, Security.framework on
//! macOS), so a root that antivirus HTTPS scanning or a company installs is
//! trusted as the browser trusts it (R41). Only a URL's host is ever
//! logged: Discord's CDN links carry signed tokens in their query (main.rs
//! also keeps ureq's own logging below trace, where it would print paths).
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use tracing::debug;
use ureq::http::Uri;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use super::transfer::{PartGuard, part_name};

/// The most a link's target may be.
pub const LINK_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub enum LinkError {
    /// Not an http or https URL (or one with credentials in it).
    BadUrl,
    /// The server answered with this status.
    Http(u16),
    /// More than the limit, said up front or found out while reading.
    TooLarge,
    /// Writing the file.
    Io(io::Error),
    /// Connecting, a stall, too many redirects, a bad response…
    Network(String),
    Cancelled,
}

impl LinkError {
    /// What went wrong, for the log (never the URL).
    pub fn kind(&self) -> String {
        match self {
            LinkError::BadUrl => "bad url".into(),
            LinkError::Http(code) => format!("http {code}"),
            LinkError::TooLarge => "too large".into(),
            LinkError::Io(e) => format!("io: {}", e.kind()),
            LinkError::Network(what) => format!("network: {what}"),
            LinkError::Cancelled => "cancelled".into(),
        }
    }
}

struct Limits {
    connect: Duration,
    /// Longest wait for the next bytes; there is no limit overall.
    stall: Duration,
    redirects: u32,
}

const LIMITS: Limits = Limits {
    connect: Duration::from_secs(10),
    stall: Duration::from_secs(30),
    redirects: 5,
};

/// Bytes read per chunk; `cancelled` is asked between chunks.
const CHUNK: usize = 64 * 1024;

/// Download `url` to `dest` (blocking), at most `max_bytes` of it. It is
/// written to a `.cjpart` sibling and renamed into place, and the part is
/// removed on any failure, cancel or panic. Returns the bytes written.
pub fn fetch_link(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, LinkError> {
    fetch_with(url, dest, max_bytes, cancelled, &LIMITS)
}

fn fetch_with(
    url: &str,
    dest: &Path,
    max_bytes: u64,
    cancelled: &dyn Fn() -> bool,
    limits: &Limits,
) -> Result<u64, LinkError> {
    let uri = checked_uri(url).ok_or(LinkError::BadUrl)?;
    let host = url_host(url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(limits.redirects)
        .http_status_as_error(false)
        .save_redirect_history(false)
        .timeout_connect(Some(limits.connect))
        .timeout_per_read(Some(limits.stall))
        .user_agent("CursorJump")
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::Rustls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    // `cancelled` is first asked once the headers are in: until then the
    // call is bounded by the connect timeout and by `stall` per read (per
    // redirect), so a server trickling its headers can hold it longer.
    let response = agent.get(uri).call().map_err(network)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        debug!(%host, status, "link: refused");
        return Err(LinkError::Http(status));
    }
    if response
        .body()
        .content_length()
        .is_some_and(|n| n > max_bytes)
    {
        return Err(LinkError::TooLarge);
    }
    let mut body = response.into_body().into_reader();
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let part = dest.with_file_name(part_name(&name));
    let mut guard = PartGuard::new(part.clone());
    let mut out = File::create(&part).map_err(LinkError::Io)?;
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        if cancelled() {
            return Err(LinkError::Cancelled);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(LinkError::Network(format!("reading: {}", e.kind()))),
        };
        total += n as u64;
        if total > max_bytes {
            return Err(LinkError::TooLarge);
        }
        out.write_all(&buf[..n]).map_err(LinkError::Io)?;
    }
    out.flush().map_err(LinkError::Io)?;
    drop(out);
    std::fs::rename(&part, dest).map_err(LinkError::Io)?;
    guard.disarm();
    debug!(%host, bytes = total, "link: fetched");
    Ok(total)
}

/// `url` if it is http or https with a host and no credentials, in the
/// ASCII form a request needs: an IDN host in punycode, a Unicode path or
/// query percent-encoded, no fragment.
fn checked_uri(url: &str) -> Option<Uri> {
    let mut parsed = url::Url::parse(url.trim()).ok()?;
    // The url crate lowercases the scheme.
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host_str().is_none_or(str::is_empty)
    {
        return None;
    }
    parsed.set_fragment(None);
    parsed.as_str().parse().ok()
}

/// A request that failed, in words that never hold the URL.
fn network(e: ureq::Error) -> LinkError {
    let what = match e {
        ureq::Error::Timeout(t) => format!("timed out ({t})"),
        ureq::Error::Io(e) => format!("{}", e.kind()),
        ureq::Error::HostNotFound => "host not found".into(),
        ureq::Error::ConnectionFailed => "connection failed".into(),
        ureq::Error::TooManyRedirects => "too many redirects".into(),
        ureq::Error::RedirectFailed => "redirect failed".into(),
        ureq::Error::BadUri(_) => "bad redirect".into(),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => "tls".into(),
        ureq::Error::Protocol(_) => "protocol".into(),
        _ => "request failed".into(),
    };
    LinkError::Network(what)
}

/// The target of an InternetShortcut file (`[InternetShortcut]`, `URL=`),
/// as UTF-8 (BOM or not) or UTF-16LE with a BOM. Trailing NULs, as an
/// HGLOBAL rounded up leaves, are ignored.
pub fn shortcut_url(contents: &[u8]) -> Option<String> {
    let text = match contents {
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(contents).into_owned(),
    };
    let mut in_section = false;
    for line in text
        .lines()
        .map(|l| l.trim_matches(|c: char| c.is_whitespace() || c == '\0'))
    {
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[InternetShortcut]");
        } else if in_section
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("URL")
        {
            let value = value.trim();
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

/// A URL's host, for the log ("?" if there is none).
pub fn url_host(url: &str) -> String {
    url::Url::parse(url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// One connection's answer: each segment is written after its delay,
    /// then the connection is closed.
    type Reply = Vec<(Duration, Vec<u8>)>;

    fn now(bytes: impl Into<Vec<u8>>) -> Reply {
        vec![(Duration::ZERO, bytes.into())]
    }

    fn head(status: &str, extra: &str) -> String {
        format!("HTTP/1.1 {status}\r\nConnection: close\r\n{extra}\r\n")
    }

    fn ok_body(body: &[u8]) -> Reply {
        let mut v = head("200 OK", &format!("Content-Length: {}\r\n", body.len())).into_bytes();
        v.extend_from_slice(body);
        now(v)
    }

    /// A local HTTP/1.1 server answering one connection per reply, in
    /// order. `replies` gets the server's base URL. The handle yields each
    /// request's head.
    fn serve(replies: impl FnOnce(&str) -> Vec<Reply>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let replies = replies(&base);
        let handle = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for reply in replies {
                let Ok((mut conn, _)) = listener.accept() else {
                    break;
                };
                heads.push(read_head(&mut conn));
                for (delay, bytes) in reply {
                    std::thread::sleep(delay);
                    if conn.write_all(&bytes).and_then(|()| conn.flush()).is_err() {
                        break;
                    }
                }
            }
            heads
        });
        (base, handle)
    }

    fn read_head(conn: &mut TcpStream) -> String {
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let mut byte = [0u8];
        while !got.ends_with(b"\r\n\r\n") {
            match conn.read(&mut byte) {
                Ok(1) => got.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    }

    fn never() -> bool {
        false
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn fetches_a_file_and_renames_it_into_place() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
        let reply = ok_body(&body);
        let (base, server) = serve(|_| vec![reply]);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("image.png");
        let got = fetch_link(
            &format!("{base}/a/image.png?ex=1"),
            &dest,
            LINK_MAX_BYTES,
            &never,
        );
        assert_eq!(got.unwrap(), body.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(names(dir.path()), ["image.png"], "no part left");
        let heads = server.join().unwrap();
        let head = heads[0].to_ascii_lowercase();
        assert!(
            head.starts_with("get /a/image.png?ex=1 http/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nuser-agent: cursorjump\r\n"), "{head}");
        assert!(
            !head.contains("cookie:") && !head.contains("authorization:"),
            "{head}"
        );
    }

    #[test]
    fn http_error_is_reported_and_nothing_is_left() {
        let reply = now(head("404 Not Found", "Content-Length: 9\r\n") + "not found");
        let (base, _server) = serve(|_| vec![reply]);
        let dir = tempfile::tempdir().unwrap();
        let got = fetch_link(
            &format!("{base}/gone"),
            &dir.path().join("x"),
            LINK_MAX_BYTES,
            &never,
        );
        assert!(matches!(got, Err(LinkError::Http(404))), "{got:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn too_large_aborts_and_cleans_up() {
        // Said up front by Content-Length...
        let (base, _server) = serve(|_| vec![ok_body(&[1; 100])]);
        let dir = tempfile::tempdir().unwrap();
        let got = fetch_link(&format!("{base}/big"), &dir.path().join("big"), 99, &never);
        assert!(matches!(got, Err(LinkError::TooLarge)), "{got:?}");
        assert!(names(dir.path()).is_empty());
        // ...or found out while streaming a body of unknown length.
        let mut unsized_reply = head("200 OK", "").into_bytes();
        unsized_reply.extend_from_slice(&[2; 300_000]);
        let (base, _server) = serve(|_| vec![now(unsized_reply)]);
        let got = fetch_link(
            &format!("{base}/big"),
            &dir.path().join("big"),
            100_000,
            &never,
        );
        assert!(matches!(got, Err(LinkError::TooLarge)), "{got:?}");
        assert!(names(dir.path()).is_empty());
        // Exactly at the limit is fine.
        let (base, _server) = serve(|_| vec![ok_body(&[3; 100])]);
        let got = fetch_link(&format!("{base}/ok"), &dir.path().join("ok"), 100, &never);
        assert_eq!(got.unwrap(), 100);
    }

    #[test]
    fn only_http_and_https_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("x");
        for url in [
            "file:///C:/Windows/win.ini",
            "ftp://example.com/a.png",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "not a url",
            "",
            "http://",
            "https://user:secret@example.com/a.png",
        ] {
            let got = fetch_link(url, &dest, LINK_MAX_BYTES, &never);
            assert!(matches!(got, Err(LinkError::BadUrl)), "{url:?}: {got:?}");
        }
        assert!(names(dir.path()).is_empty());
        // A redirect to another scheme is not followed.
        let (base, _server) = serve(|_| {
            vec![now(head(
                "302 Found",
                "Location: file:///C:/Windows/win.ini\r\nContent-Length: 0\r\n",
            ))]
        });
        let got = fetch_link(&format!("{base}/r"), &dest, LINK_MAX_BYTES, &never);
        assert!(got.is_err(), "{got:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn follows_a_redirect() {
        let (base, server) = serve(|base| {
            let to = format!("Location: {base}/real.png\r\nContent-Length: 0\r\n");
            vec![now(head("302 Found", &to)), ok_body(b"png!")]
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("image.png");
        assert_eq!(
            fetch_link(&format!("{base}/short"), &dest, LINK_MAX_BYTES, &never).unwrap(),
            4
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"png!");
        let heads = server.join().unwrap();
        assert!(heads[1].starts_with("GET /real.png "), "{}", heads[1]);
        // Exactly 5 redirects are followed...
        let hops = |n: usize| {
            move |base: &str| {
                let to = format!("Location: {base}/again\r\nContent-Length: 0\r\n");
                let mut replies: Vec<Reply> = (0..n).map(|_| now(head("302 Found", &to))).collect();
                replies.push(ok_body(b"end"));
                replies
            }
        };
        let (base, _server) = serve(hops(5));
        let five = dest.with_file_name("five");
        assert_eq!(
            fetch_link(&format!("{base}/again"), &five, LINK_MAX_BYTES, &never).unwrap(),
            3
        );
        std::fs::remove_file(&five).unwrap();
        // ...and a 6th fails.
        let (base, _server) = serve(hops(6));
        let got = fetch_link(
            &format!("{base}/again"),
            &dest.with_file_name("y"),
            LINK_MAX_BYTES,
            &never,
        );
        assert!(matches!(got, Err(LinkError::Network(_))), "{got:?}");
        assert_eq!(names(dir.path()), ["image.png"]);
    }

    #[test]
    fn cancel_between_chunks_cleans_up() {
        let (base, _server) = serve(|_| {
            vec![vec![
                (
                    Duration::ZERO,
                    [
                        head("200 OK", "Content-Length: 300000\r\n").into_bytes(),
                        vec![4; 100_000],
                    ]
                    .concat(),
                ),
                (Duration::from_millis(200), vec![4; 100_000]),
                (Duration::from_millis(200), vec![4; 100_000]),
            ]]
        });
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        // Cancelled once the first bytes are in.
        let cancelled = move || counted.fetch_add(1, Ordering::SeqCst) >= 2;
        let got = fetch_link(
            &format!("{base}/slow"),
            &dir.path().join("slow"),
            LINK_MAX_BYTES,
            &cancelled,
        );
        assert!(matches!(got, Err(LinkError::Cancelled)), "{got:?}");
        assert!(names(dir.path()).is_empty());
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn a_stall_between_chunks_fails() {
        let (base, _server) = serve(|_| {
            vec![vec![
                (
                    Duration::ZERO,
                    head("200 OK", "Content-Length: 10\r\n").into_bytes(),
                ),
                (Duration::from_millis(1500), vec![5; 10]),
            ]]
        });
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            stall: Duration::from_millis(300),
            ..LIMITS
        };
        let got = fetch_with(
            &format!("{base}/stall"),
            &dir.path().join("s"),
            LINK_MAX_BYTES,
            &never,
            &limits,
        );
        assert!(matches!(got, Err(LinkError::Network(_))), "{got:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn shortcut_url_parses_utf8_and_utf16() {
        let text = "[{000214A0-0000-0000-C000-000000000046}]\r\nProp3=19,11\r\n\
                    [InternetShortcut]\r\nIDList=\r\nURL=https://cdn.example/image.png?ex=1\r\n";
        let want = Some("https://cdn.example/image.png?ex=1".to_string());
        assert_eq!(shortcut_url(text.as_bytes()), want);
        // With a UTF-8 BOM and trailing NULs (an HGLOBAL rounded up).
        let bom8 = [&[0xEF, 0xBB, 0xBF][..], text.as_bytes(), &[0, 0, 0]].concat();
        assert_eq!(shortcut_url(&bom8), want);
        // UTF-16LE with a BOM.
        let mut utf16 = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            utf16.extend_from_slice(&u.to_le_bytes());
        }
        utf16.extend_from_slice(&[0, 0]);
        assert_eq!(shortcut_url(&utf16), want);
        // Not in the section, or no URL line.
        assert_eq!(shortcut_url(b"URL=https://outside/section"), None);
        assert_eq!(shortcut_url(b"[InternetShortcut]\nIconIndex=0\n"), None);
        assert_eq!(shortcut_url(b""), None);
    }

    #[test]
    fn error_kinds_never_hold_the_url() {
        let secret = "https://cdn.example/a.png?hm=secret";
        // The server reads the request and hangs up without an answer: a
        // network error, in words that carry no URL.
        let (base, _server) = serve(|_| vec![vec![]]);
        let dir = tempfile::tempdir().unwrap();
        let got = fetch_link(
            &format!("{base}/a.png?hm=secret"),
            &dir.path().join("a"),
            1,
            &never,
        );
        let kinds = [
            got.unwrap_err().kind(),
            LinkError::BadUrl.kind(),
            LinkError::Http(403).kind(),
            LinkError::TooLarge.kind(),
            LinkError::Io(io::ErrorKind::PermissionDenied.into()).kind(),
            LinkError::Network("too many redirects".into()).kind(),
            LinkError::Cancelled.kind(),
        ];
        assert!(kinds[0].starts_with("network: "), "{}", kinds[0]);
        assert_eq!(kinds[2], "http 403");
        assert_eq!(kinds[4], "io: permission denied");
        for k in kinds {
            assert!(!k.contains("secret") && !k.contains(secret), "{k}");
        }
    }

    #[test]
    fn a_body_cut_short_fails_and_cleans_up() {
        let (base, _server) =
            serve(|_| vec![now(head("200 OK", "Content-Length: 10\r\n") + "abcd")]);
        let dir = tempfile::tempdir().unwrap();
        let got = fetch_link(
            &format!("{base}/short"),
            &dir.path().join("short"),
            LINK_MAX_BYTES,
            &never,
        );
        assert!(matches!(got, Err(LinkError::Network(_))), "{got:?}");
        assert!(names(dir.path()).is_empty());
    }

    #[test]
    fn non_ascii_urls_are_encoded_for_the_request() {
        // An IDN host in punycode; a Unicode path and query percent-encoded.
        let uri = checked_uri("https://bücher.example/ä.png?q=ü#frag").unwrap();
        assert_eq!(uri.host(), Some("xn--bcher-kva.example"));
        assert_eq!(uri.path(), "/%C3%A4.png");
        assert_eq!(uri.query(), Some("q=%C3%BC"));
        assert_eq!(
            url_host("https://bücher.example/ä"),
            "xn--bcher-kva.example"
        );
        // And they download.
        let (base, server) = serve(|_| vec![ok_body(b"jpg")]);
        let dir = tempfile::tempdir().unwrap();
        let got = fetch_link(
            &format!("{base}/картинка.jpg?q=ü"),
            &dir.path().join("картинка.jpg"),
            LINK_MAX_BYTES,
            &never,
        );
        assert_eq!(got.unwrap(), 3);
        let heads = server.join().unwrap();
        assert!(
            heads[0]
                .starts_with("GET /%D0%BA%D0%B0%D1%80%D1%82%D0%B8%D0%BD%D0%BA%D0%B0.jpg?q=%C3%BC "),
            "{}",
            heads[0]
        );
    }

    /// A real https download, checked against the OS trust store.
    /// `CJ_LINK_URL` picks the link (default: the Rust logo).
    #[test]
    #[ignore = "needs the internet"]
    fn smoke_fetches_over_https_with_the_os_trust_store() {
        let url = std::env::var("CJ_LINK_URL")
            .unwrap_or_else(|_| "https://www.rust-lang.org/static/images/rust-logo-blk.svg".into());
        let dir = tempfile::tempdir().unwrap();
        let bytes = fetch_link(&url, &dir.path().join("fetched"), LINK_MAX_BYTES, &never)
            .unwrap_or_else(|e| panic!("{}: {}", url_host(&url), e.kind()));
        eprintln!("smoke: {} bytes from {}", bytes, url_host(&url));
        assert!(bytes > 0);
        // A certificate no OS trusts is refused.
        let got = fetch_link(
            "https://self-signed.badssl.com/",
            &dir.path().join("bad"),
            LINK_MAX_BYTES,
            &never,
        );
        assert!(matches!(got, Err(LinkError::Network(_))), "{got:?}");
    }

    #[test]
    fn hosts_are_logged_without_paths_or_queries() {
        assert_eq!(
            url_host("https://cdn.discordapp.com/a/b.png?ex=1&hm=secret"),
            "cdn.discordapp.com"
        );
        assert_eq!(url_host("http://127.0.0.1:8080/x"), "127.0.0.1");
        assert_eq!(url_host("https://user:pw@example.com/"), "example.com");
        assert_eq!(url_host("not a url"), "?");
    }
}
