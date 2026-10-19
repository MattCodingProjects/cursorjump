//! Clipboard model shared by the Windows and macOS clipboard modules.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::io::Cursor;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ClipboardData {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<Vec<u8>>,
    pub png: Option<Vec<u8>>,
}

impl ClipboardData {
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.rtf.is_none() && self.png.is_none()
    }

    pub fn byte_len(&self) -> usize {
        self.text.as_ref().map_or(0, |s| s.len())
            + self.html.as_ref().map_or(0, |s| s.len())
            + self.rtf.as_ref().map_or(0, |b| b.len())
            + self.png.as_ref().map_or(0, |b| b.len())
    }

    /// Stable within a build: `DefaultHasher::new()` uses fixed keys.
    pub fn content_hash(&self) -> u64 {
        let mut h = std::hash::DefaultHasher::new();
        self.text.hash(&mut h);
        self.html.hash(&mut h);
        self.rtf.hash(&mut h);
        self.png.hash(&mut h);
        h.finish()
    }
}

pub fn within_limit(d: &ClipboardData, max_bytes: u64) -> bool {
    d.byte_len() as u64 <= max_bytes
}

// ---------------------------------------------------------------- CF_HTML

const START_MARK: &str = "<!--StartFragment-->";
const END_MARK: &str = "<!--EndFragment-->";

/// Wrap an HTML fragment in the Windows "HTML Format" envelope (UTF-8 byte offsets).
pub fn cf_html_wrap(fragment: &str) -> String {
    let header = |sh: usize, eh: usize, sf: usize, ef: usize| {
        format!(
            "Version:0.9\r\nStartHTML:{sh:010}\r\nEndHTML:{eh:010}\r\nStartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\n"
        )
    };
    let prefix = format!("<html><body>\r\n{START_MARK}");
    let suffix = format!("{END_MARK}\r\n</body></html>");
    let hl = header(0, 0, 0, 0).len();
    let start_html = hl;
    let start_frag = hl + prefix.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + suffix.len();
    format!(
        "{}{prefix}{fragment}{suffix}",
        header(start_html, end_html, start_frag, end_frag)
    )
}

fn header_offset(cf: &str, key: &str) -> Option<usize> {
    // Only look in the header region, before the first tag.
    let head = &cf[..cf.find('<').unwrap_or(cf.len())];
    let rest = &head[head.find(key)? + key.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Extract the fragment from a CF_HTML string.
pub fn cf_html_extract(cf: &str) -> Option<String> {
    if let (Some(s), Some(e)) = (
        header_offset(cf, "StartFragment:"),
        header_offset(cf, "EndFragment:"),
    ) && s <= e
        && let Some(frag) = cf
            .as_bytes()
            .get(s..e)
            .and_then(|b| std::str::from_utf8(b).ok())
    {
        return Some(frag.to_string());
    }
    let s = cf.find(START_MARK)? + START_MARK.len();
    let e = s + cf[s..].find(END_MARK)?;
    Some(cf[s..e].to_string())
}

// ------------------------------------------------------------- DIB <-> PNG

fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    let s = b.get(o..o + 2).ok_or_else(|| anyhow!("DIB truncated"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    let s = b.get(o..o + 4).ok_or_else(|| anyhow!("DIB truncated"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn channel(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let max = (mask >> mask.trailing_zeros()) as u64;
    let v = ((px & mask) >> mask.trailing_zeros()) as u64;
    (v * 255 / max) as u8
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// Convert a packed CF_DIB / CF_DIBV5 (24 or 32 bpp) to PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    let hs = u32_at(dib, 0)? as usize;
    ensure!(hs >= 40 && hs <= dib.len(), "bad DIB header size {hs}");
    let width = u32_at(dib, 4)? as i32;
    let height = u32_at(dib, 8)? as i32;
    let bpp = u16_at(dib, 14)?;
    let comp = u32_at(dib, 16)?;
    let clr_used = u32_at(dib, 32)? as usize;
    ensure!(width > 0 && height != 0, "bad DIB size {width}x{height}");
    ensure!(
        comp == BI_RGB || comp == BI_BITFIELDS,
        "unsupported DIB compression {comp}"
    );
    ensure!(
        bpp == 32 || (bpp == 24 && comp == BI_RGB),
        "unsupported DIB bit depth {bpp}"
    );

    // Channel masks (32 bpp). Defaults are BGRA byte order.
    let (mut rm, mut gm, mut bm, mut am) = (0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000);
    let mut offset = hs;
    if comp == BI_BITFIELDS {
        if hs >= 108 {
            rm = u32_at(dib, 40)?;
            gm = u32_at(dib, 44)?;
            bm = u32_at(dib, 48)?;
            am = u32_at(dib, 52)?;
        } else {
            rm = u32_at(dib, 40)?;
            gm = u32_at(dib, 44)?;
            bm = u32_at(dib, 48)?;
            am = 0;
            offset += 12;
        }
    }
    offset = offset
        .checked_add(clr_used.checked_mul(4).context("palette size overflow")?)
        .context("DIB offset overflow")?;

    let w = width as usize;
    let h = height.unsigned_abs() as usize;
    let bottom_up = height > 0;
    let stride = (w
        .checked_mul(bpp as usize)
        .context("DIB too wide")?
        .div_ceil(32))
        * 4;
    let needed = stride.checked_mul(h).context("DIB too large")?;
    let end = offset.checked_add(needed).context("DIB too large")?;
    ensure!(end <= dib.len(), "DIB pixel data truncated");
    let pixels = &dib[offset..end];

    let mut rgba = vec![0u8; w * h * 4];
    for file_row in 0..h {
        let y = if bottom_up {
            h - 1 - file_row
        } else {
            file_row
        };
        let src = &pixels[file_row * stride..file_row * stride + stride];
        for x in 0..w {
            let out = (y * w + x) * 4;
            let px = if bpp == 24 {
                let s = &src[x * 3..x * 3 + 3];
                [s[2], s[1], s[0], 255]
            } else {
                let p = u32::from_le_bytes([
                    src[x * 4],
                    src[x * 4 + 1],
                    src[x * 4 + 2],
                    src[x * 4 + 3],
                ]);
                [
                    channel(p, rm),
                    channel(p, gm),
                    channel(p, bm),
                    channel(p, am),
                ]
            };
            rgba[out..out + 4].copy_from_slice(&px);
        }
    }
    // Many Windows apps leave the alpha channel zeroed: treat as opaque.
    if bpp == 32 && rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        rgba.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p[3] = 255);
    }

    let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| anyhow!("bad pixel buffer"))?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

/// Convert PNG to a packed 32 bpp BITMAPV5HEADER DIB (bottom-up, with alpha).
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image");
    }
    let size_image = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("image too large")?;
    let mut out = Vec::with_capacity(124 + size_image);
    let mut p32 = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    p32(124); // bV5Size
    p32(w);
    p32(h); // positive = bottom-up
    // planes (1) + bit count (32)
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    let mut p32 = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    p32(BI_BITFIELDS);
    p32(size_image as u32);
    p32(0); // x ppm
    p32(0); // y ppm
    p32(0); // clr used
    p32(0); // clr important
    p32(0x00FF_0000); // R
    p32(0x0000_FF00); // G
    p32(0x0000_00FF); // B
    p32(0xFF00_0000); // A
    p32(0x7352_4742); // LCS_sRGB
    out.extend_from_slice(&[0u8; 36]); // endpoints
    out.extend_from_slice(&[0u8; 12]); // gamma
    let mut p32 = |v: u32| out.extend_from_slice(&v.to_le_bytes());
    p32(4); // LCS_GM_IMAGES
    p32(0); // profile data
    p32(0); // profile size
    p32(0); // reserved
    debug_assert_eq!(out.len(), 124);
    for y in (0..h).rev() {
        for x in 0..w {
            let [r, g, b, a] = img.get_pixel(x, y).0;
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(out)
}

// -------------------------------------------------------------- EchoGuard

/// Stops a clipboard we just applied from being sent straight back.
#[derive(Debug, Clone, Default)]
pub struct EchoGuard {
    applied: Option<u64>,
    sent: Option<u64>,
}

impl EchoGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// We just wrote remote data locally. That replaces whatever we sent last,
    /// so sending that item again is a real change.
    pub fn mark_applied(&mut self, hash: u64) {
        self.applied = Some(hash);
        self.sent = None;
    }

    /// False if `hash` equals the last applied or last sent hash.
    pub fn should_send(&mut self, hash: u64) -> bool {
        if self.applied == Some(hash) || self.sent == Some(hash) {
            return false;
        }
        self.sent = Some(hash);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str, key: &str) -> usize {
        let i = s.find(key).unwrap() + key.len();
        s[i..i + 10].parse().unwrap()
    }

    #[test]
    fn cf_html_multibyte_offsets() {
        let frag = "<b>café ☕</b>";
        let s = cf_html_wrap(frag);
        let sf = parse(&s, "StartFragment:");
        let ef = parse(&s, "EndFragment:");
        assert_eq!(&s.as_bytes()[sf..ef], frag.as_bytes());
        let sh = parse(&s, "StartHTML:");
        let eh = parse(&s, "EndHTML:");
        assert!(s[sh..].starts_with("<html>"));
        assert_eq!(eh, s.len());
        assert_eq!(cf_html_extract(&s).as_deref(), Some(frag));
    }

    #[test]
    fn cf_html_extract_real_world() {
        let expected = "<span style=\"color: red;\">Hello</span>";
        let cf = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000215\r\nStartFragment:0000000141\r\nEndFragment:0000000179\r\n<html>\r\n<body>\r\n<!--StartFragment--><span style=\"color: red;\">Hello</span><!--EndFragment-->\r\n</body>\r\n</html>";
        assert_eq!(cf_html_extract(cf).as_deref(), Some(expected));
        // Bad offsets fall back to the markers.
        let bad = cf.replace("StartFragment:0000000141", "StartFragment:0000009999");
        assert_eq!(cf_html_extract(&bad).as_deref(), Some(expected));
        // Missing offsets fall back to the markers.
        let (_, body) = cf.split_once("<html>").unwrap();
        assert_eq!(
            cf_html_extract(&format!("<html>{body}")).as_deref(),
            Some(expected)
        );
        assert_eq!(cf_html_extract("plain text"), None);
    }

    #[test]
    fn clipboard_over_limit_skipped() {
        let d = ClipboardData {
            png: Some(vec![0; 40 * 1024 * 1024]),
            ..Default::default()
        };
        assert!(!within_limit(&d, 32 * 1024 * 1024));
        assert!(within_limit(&d, 64 * 1024 * 1024));
        assert!(!d.is_empty());
        assert!(ClipboardData::default().is_empty());
    }

    #[test]
    fn content_hash_differs_by_content() {
        let a = ClipboardData {
            text: Some("a".into()),
            ..Default::default()
        };
        let b = ClipboardData {
            text: Some("b".into()),
            ..Default::default()
        };
        assert_eq!(a.content_hash(), a.clone().content_hash());
        assert_ne!(a.content_hash(), b.content_hash());
    }

    // 2x2 image: top row red, green; bottom row blue, white(ish) with alpha.
    const TOP: [[u8; 4]; 2] = [[255, 0, 0, 255], [0, 255, 0, 128]];
    const BOTTOM: [[u8; 4]; 2] = [[0, 0, 255, 0], [255, 255, 255, 255]];

    fn header40(w: i32, h: i32, bpp: u16, comp: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&comp.to_le_bytes());
        v.extend_from_slice(&[0u8; 20]);
        v
    }

    fn decode(png: &[u8]) -> image::RgbaImage {
        image::load_from_memory(png).unwrap().to_rgba8()
    }

    #[test]
    fn dib_png_roundtrip_32bpp() {
        let img = image::RgbaImage::from_fn(2, 2, |x, y| {
            image::Rgba(if y == 0 {
                TOP[x as usize]
            } else {
                BOTTOM[x as usize]
            })
        });
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(dib.len(), 124 + 16);
        // Bottom-up: first stored row is the image's bottom row, BGRA.
        assert_eq!(&dib[124..128], &[255, 0, 0, 0]);
        let back = decode(&dib_to_png(&dib).unwrap());
        assert_eq!(back, img);
    }

    #[test]
    fn dib_bottom_up_24bpp_to_png() {
        let mut dib = header40(2, 2, 24, BI_RGB);
        // Bottom row first (BGR), padded to 8 bytes.
        dib.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]); // blue, white
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // red, green
        let img = decode(&dib_to_png(&dib).unwrap());
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [255, 255, 255, 255]);
    }

    #[test]
    fn dib_top_down_32bpp_zero_alpha_is_opaque() {
        let mut dib = header40(2, -1, 32, BI_RGB);
        dib.extend_from_slice(&[0, 0, 255, 0, 0, 255, 0, 0]); // red, green; alpha 0
        let img = decode(&dib_to_png(&dib).unwrap());
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255]);
    }

    #[test]
    fn dib_bitfields_40_byte_header() {
        let mut dib = header40(1, 1, 32, BI_BITFIELDS);
        for m in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF] {
            dib.extend_from_slice(&m.to_le_bytes());
        }
        dib.extend_from_slice(&[10, 20, 30, 0]); // B G R
        let img = decode(&dib_to_png(&dib).unwrap());
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10, 255]);
    }

    #[test]
    fn dib_rejects_garbage() {
        assert!(dib_to_png(&[]).is_err());
        assert!(dib_to_png(&header40(2, 2, 24, BI_RGB)).is_err()); // truncated pixels
        assert!(dib_to_png(&header40(2, 2, 8, BI_RGB)).is_err());
        assert!(png_to_dib(b"not a png").is_err());
    }

    #[test]
    fn echo_guard_blocks_reflection() {
        let mut g = EchoGuard::new();
        g.mark_applied(1);
        assert!(!g.should_send(1));
        assert!(g.should_send(2));
        assert!(!g.should_send(2));
    }

    #[test]
    fn echo_guard_resends_after_peer_item() {
        let mut g = EchoGuard::new();
        assert!(g.should_send(1)); // copy X here
        assert!(!g.should_send(1)); // Windows' duplicate events for X
        g.mark_applied(2); // Y arrives from the peer
        assert!(!g.should_send(2)); // the echo of writing Y
        assert!(!g.should_send(2)); // and its duplicates
        assert!(g.should_send(1)); // copy X again: a real change
        assert!(!g.should_send(1));
    }
}
