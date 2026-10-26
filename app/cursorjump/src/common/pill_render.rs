//! Draws one "dark glass" progress pill into a premultiplied RGBA pixmap.
//! The hosts put it in a layered (Windows) or transparent (Mac) window.
//!
//! - The body, its border and its shadow don't depend on the pill, so they
//!   are drawn once per (scale, backdrop) and cached; a frame copies that
//!   base and draws the icon, text and bar over it.
//! - The shadow is a Gaussian blur (three box passes) of the body's
//!   coverage, cut away under the body as a CSS box-shadow is. `SHADOW_PAD`
//!   is less than the blur's reach, so the shadow is eased to nothing over
//!   the pad rather than ending in a hard edge.
//! - Text is laid out glyph by glyph with Inter's advances (no shaping) and
//!   ellipsized at a char boundary. The ✓, ✕ and icon glyphs are paths.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont, point};
use cj_core::pill::{PillIcon, PillView};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Mask, Paint, Path,
    PathBuilder, Pixmap, Point, Rect, Shader, SpreadMode, Stroke, Transform,
};
use unicode_normalization::UnicodeNormalization;

/// Body size, gap between stacked pills and gap below the bottom one, in
/// logical px.
pub const PILL_W: f32 = 360.0;
pub const PILL_H: f32 = 52.0;
pub const PILL_GAP: f32 = 8.0;
pub const BOTTOM_MARGIN: f32 = 24.0;
/// Room for the shadow on every side of the body, in logical px.
pub const SHADOW_PAD: f32 = 12.0;

/// What is behind the pill's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// A blur view (the Mac).
    #[cfg_attr(
        all(windows, not(test)),
        expect(dead_code, reason = "only the Mac's pills are blurred")
    )]
    Blurred,
    /// Nothing: a layered window can't blur what's behind it (Windows).
    #[cfg_attr(
        all(not(windows), not(test)),
        expect(dead_code, reason = "only the PC's pills are opaque")
    )]
    Opaque,
}

// The layout, in logical px from the body's top-left corner.
const PAD_X: f32 = 14.0;
const ICON: f32 = 26.0;
const ICON_Y: f32 = (PILL_H - ICON) / 2.0;
const ICON_RADIUS: f32 = 7.0;
/// Where the text and bar start and end.
const META_X: f32 = PAD_X + ICON + 10.0;
const META_RIGHT: f32 = PILL_W - PAD_X;
const NAME_SIZE: f32 = 12.0;
const NAME_BASELINE: f32 = 17.5;
const COUNT_SIZE: f32 = 13.0;
const BAR_Y: f32 = 25.0;
const BAR_H: f32 = 3.0;
/// The indeterminate track's height, centred on the bar's.
const THIN_H: f32 = 1.0;
/// One tile of the bar's sheen.
const SHEEN_W: f32 = 160.0;
const SUB_SIZE: f32 = 10.5;
const SUB_BASELINE: f32 = 41.0;
/// Between the name and whatever is right of it.
const RIGHT_GAP: f32 = 8.0;
/// The centre line of the ✓ and ✕, level with the name's lowercase.
const MARK_Y: f32 = NAME_BASELINE - 4.5;
const CHECK_W: f32 = 10.0;
const CLOSE_W: f32 = 8.0;
/// The ✕'s hit area, a square around it.
const CLOSE_HIT: f32 = 20.0;

const BORDER: f32 = 1.0;
const SHADOW_BLUR: f32 = 24.0;
const SHADOW_Y: f32 = 6.0;
const SHADOW_ALPHA: f32 = 0.45;

/// Inter's cap height, which its digits share, per em.
const CAP_HEIGHT: f32 = 0.727;
const INK: [u8; 3] = [0xF5, 0xF5, 0xF5];

/// Logical px to pixmap px at one scale.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// The body's top-left corner, on both axes.
    origin: f32,
    scale: f32,
}

impl Layout {
    fn new(scale: f32) -> Layout {
        let scale = sane(scale);
        Layout {
            origin: (SHADOW_PAD * scale).round(),
            scale,
        }
    }

    fn len(self, v: f32) -> f32 {
        v * self.scale
    }

    /// A body-relative logical coordinate, on either axis.
    fn at(self, v: f32) -> f32 {
        self.origin + v * self.scale
    }
}

/// The scale to draw at: a broken one draws at 1.
fn sane(scale: f32) -> f32 {
    if scale.is_finite() && (0.25..=8.0).contains(&scale) {
        scale
    } else {
        1.0
    }
}

struct Fonts {
    regular: FontRef<'static>,
    semibold: FontRef<'static>,
}

/// Inter 4.1 (OFL), from the official release.
static FONTS: LazyLock<Fonts> = LazyLock::new(|| Fonts {
    regular: FontRef::try_from_slice(include_bytes!("../../assets/fonts/Inter-Regular.ttf"))
        .expect("embedded Inter Regular parses"),
    semibold: FontRef::try_from_slice(include_bytes!("../../assets/fonts/Inter-SemiBold.ttf"))
        .expect("embedded Inter SemiBold parses"),
});

/// The widest "dd%" at an em of 1 px. Inter's digits aren't tabular here,
/// so the percent's slot keeps this width (times the size) and the name's
/// cut doesn't hop as the digits change.
static WIDEST_PERCENT: LazyLock<f32> = LazyLock::new(|| {
    ('0'..='9')
        .map(|d| text_width(&FONTS.regular, 1.0, &format!("{d}{d}%")))
        .fold(0.0, f32::max)
});

/// Drawn for each run of chars the fonts have no glyph for.
const MISSING: char = '□';

/// The body's size in pixmap px. The pixmap adds `SHADOW_PAD` on each side.
pub fn pill_px(scale: f32) -> (u32, u32) {
    let scale = sane(scale);
    (
        (PILL_W * scale).round() as u32,
        (PILL_H * scale).round() as u32,
    )
}

/// The pixmap's size: the body plus `SHADOW_PAD` on each side, in pixmap px.
pub fn pixmap_px(scale: f32) -> (u32, u32) {
    let (w, h) = pill_px(scale);
    let pad = 2 * Layout::new(scale).origin as u32;
    (w + pad, h + pad)
}

/// The ✕'s hit area, in pixmap px.
pub fn close_rect(scale: f32) -> Rect {
    let l = Layout::new(scale);
    let half = CLOSE_HIT / 2.0;
    let (cx, cy) = (META_RIGHT - CLOSE_W / 2.0, MARK_Y);
    Rect::from_ltrb(
        l.at(cx - half),
        l.at(cy - half),
        l.at(cx + half),
        l.at(cy + half),
    )
    .expect("a non-empty square")
}

/// Whether the pill shows its ✕, which a click there acts on: a running
/// pill's on hover (it cancels) and a failed one's always (it dismisses).
pub fn close_shown(view: &PillView, hover: bool) -> bool {
    view.failed || (hover && view.running)
}

/// What is drawn right of the name.
enum Right<'a> {
    Close,
    Check(f32),
    Percent(&'a str),
    Nothing,
}

impl Right<'_> {
    /// The ✕, when shown, replaces the percent.
    fn of(view: &PillView, hover: bool) -> Right<'_> {
        if close_shown(view, hover) {
            Right::Close
        } else if view.check_scale > 0.0 {
            Right::Check(view.check_scale)
        } else if !view.right.is_empty() {
            Right::Percent(&view.right)
        } else {
            Right::Nothing
        }
    }

    /// The room it takes, in pixmap px. The ✓ keeps its full width as it
    /// pops, and a percent at least `WIDEST_PERCENT`'s.
    fn width(&self, l: Layout) -> f32 {
        let size = l.len(NAME_SIZE);
        match self {
            Right::Close => l.len(CLOSE_W),
            Right::Check(_) => l.len(CHECK_W),
            Right::Percent(text) => {
                text_width(&FONTS.regular, size, text).max(*WIDEST_PERCENT * size)
            }
            Right::Nothing => 0.0,
        }
    }
}

/// Draws `view`. The pixmap is `pill_px(scale)` plus `SHADOW_PAD` on each
/// side, premultiplied, and all of it is multiplied by `view.opacity`.
pub fn render(view: &PillView, scale: f32, backdrop: Backdrop, hover: bool) -> Pixmap {
    let l = Layout::new(scale);
    let mut pm = Pixmap::clone(&base(l, backdrop));
    draw_icon(&mut pm, l, &view.icon);

    let right = Right::of(view, hover);
    let (name_x, name_right) = name_box(view, l.scale, hover);
    let size = l.len(NAME_SIZE);
    let baseline = l.at(NAME_BASELINE);
    let name = displayable(&FONTS.semibold, &view.name);
    let name = ellipsize(&FONTS.semibold, size, &name, name_right - name_x);
    draw_text(&mut pm, &FONTS.semibold, size, &name, name_x, baseline, 1.0);
    let (mark_x, mark_y) = (l.at(META_RIGHT), l.at(MARK_Y));
    match right {
        Right::Close => draw_close(&mut pm, l, mark_x - l.len(CLOSE_W / 2.0), mark_y),
        Right::Check(k) => draw_check(&mut pm, l, mark_x - l.len(CHECK_W / 2.0), mark_y, k),
        Right::Percent(text) => {
            let x = mark_x - text_width(&FONTS.regular, size, text);
            draw_text(&mut pm, &FONTS.regular, size, text, x, baseline, 0.6);
        }
        Right::Nothing => {}
    }

    draw_bar(&mut pm, l, view);
    let size = l.len(SUB_SIZE);
    let sub = displayable(&FONTS.regular, &view.sub);
    let sub = ellipsize(&FONTS.regular, size, &sub, l.len(META_RIGHT - META_X));
    let (x, y) = (l.at(META_X), l.at(SUB_BASELINE));
    draw_text(&mut pm, &FONTS.regular, size, &sub, x, y, 0.55);

    if view.opacity < 1.0 {
        let k = view.opacity.clamp(0.0, 1.0);
        for c in pm.data_mut() {
            *c = (f32::from(*c) * k).round() as u8;
        }
    }
    pm
}

/// Where the name may go, from its left to its right, in pixmap px.
fn name_box(view: &PillView, scale: f32, hover: bool) -> (f32, f32) {
    let l = Layout::new(scale);
    let right = Right::of(view, hover).width(l);
    let gap = if right > 0.0 { l.len(RIGHT_GAP) } else { 0.0 };
    (l.at(META_X), l.at(META_RIGHT) - right - gap)
}

/// The shadow, body and border at this scale and backdrop, cached.
fn base(l: Layout, backdrop: Backdrop) -> Arc<Pixmap> {
    /// Few (scale, backdrop) pairs are ever in use; this bounds the rest.
    const KEEP: usize = 8;
    type Cache = Vec<((u32, Backdrop), Arc<Pixmap>)>;
    static CACHE: Mutex<Cache> = Mutex::new(Vec::new());

    let key = (l.scale.to_bits(), backdrop);
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, pm)) = cache.iter().find(|(k, _)| *k == key) {
        return pm.clone();
    }
    let pm = Arc::new(draw_base(l, backdrop));
    if cache.len() >= KEEP {
        cache.remove(0);
    }
    cache.push((key, pm.clone()));
    pm
}

fn draw_base(l: Layout, backdrop: Backdrop) -> Pixmap {
    let (w, h) = pixmap_px(l.scale);
    let mut pm = Pixmap::new(w, h).expect("a non-empty pixmap");
    let (bw, bh) = pill_px(l.scale);
    let body_rect = Rect::from_xywh(l.origin, l.origin, bw as f32, bh as f32).expect("a body");
    let body = rounded_rect(body_rect, bh as f32 / 2.0);

    // Shadow: the body's coverage, moved down, blurred, minus the body.
    let mut shape = Mask::new(w, h).expect("a non-empty mask");
    let down = Transform::from_translate(0.0, l.len(SHADOW_Y));
    shape.fill_path(&body, FillRule::Winding, true, down);
    let mut under = Mask::new(w, h).expect("a non-empty mask");
    under.fill_path(&body, FillRule::Winding, true, Transform::identity());
    let sigma = l.len(SHADOW_BLUR / 2.0);
    let blurred = gaussian_blur(shape.data(), w as usize, h as usize, sigma);
    // Eased to 0 at the pixmap's edges, over the pad.
    let pad = l.origin.max(1.0);
    let ease = |i: usize, n: u32| {
        let d = (i as f32 + 0.5).min(n as f32 - i as f32 - 0.5);
        let t = (d / pad).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let row = w as usize;
    let data = pm.data_mut();
    for (i, (&s, &u)) in blurred.iter().zip(under.data()).enumerate() {
        let outside = 1.0 - f32::from(u) / 255.0;
        let a = SHADOW_ALPHA * s * outside * ease(i % row, w) * ease(i / row, h);
        data[i * 4 + 3] = (a * 255.0).round() as u8;
    }

    let alpha = match backdrop {
        Backdrop::Blurred => 0.62,
        Backdrop::Opaque => 0.80,
    };
    let fill = solid([18, 18, 18], alpha);
    pm.fill_path(&body, &fill, FillRule::Winding, Transform::identity(), None);
    // A 1 px border just inside the body's edge.
    let half = l.len(BORDER) / 2.0;
    let inset = Rect::from_ltrb(
        body_rect.left() + half,
        body_rect.top() + half,
        body_rect.right() - half,
        body_rect.bottom() - half,
    )
    .expect("an inset body");
    let stroke = Stroke {
        width: l.len(BORDER),
        ..Stroke::default()
    };
    let border = rounded_rect(inset, inset.height() / 2.0);
    let line = solid([255, 255, 255], 0.14);
    pm.stroke_path(&border, &line, &stroke, Transform::identity(), None);
    pm
}

/// `coverage` (0–255) blurred with a Gaussian of `sigma` px, as 0–1. Three
/// box blurs each way approximate it to within a few percent.
fn gaussian_blur(coverage: &[u8], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let mut a: Vec<f32> = coverage.iter().map(|&c| f32::from(c) / 255.0).collect();
    let mut b = vec![0.0; a.len()];
    for r in box_radii(sigma) {
        box_blur(&a, &mut b, h, w, w, 1, r);
        box_blur(&b, &mut a, w, h, 1, w, r);
    }
    a
}

/// Radii of three box blurs that together approximate a Gaussian of
/// `sigma` (W. Jarosz, "Fast Image Convolutions", 2001).
fn box_radii(sigma: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut lo = ideal.floor() as usize;
    if lo.is_multiple_of(2) {
        lo = lo.saturating_sub(1);
    }
    let lo = lo.max(1);
    let wl = lo as f32;
    let m =
        ((12.0 * sigma * sigma - n * wl * wl - 4.0 * n * wl - 3.0 * n) / (-4.0 * wl - 4.0)).round();
    let r = |i: f32| (if i < m { lo } else { lo + 2 } - 1) / 2;
    [r(0.0), r(1.0), r(2.0)]
}

/// One box blur of radius `r` along `lines` lines of `len` samples `step`
/// apart, line k starting at k × `next`. Outside the pixmap counts as 0.
fn box_blur(
    src: &[f32],
    dst: &mut [f32],
    lines: usize,
    len: usize,
    next: usize,
    step: usize,
    r: usize,
) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for k in 0..lines {
        let at = |i: usize| k * next + i * step;
        let mut sum: f32 = (0..r.min(len)).map(|i| src[at(i)]).sum();
        for i in 0..len {
            if i + r < len {
                sum += src[at(i + r)];
            }
            dst[at(i)] = sum * norm;
            if i >= r {
                sum -= src[at(i - r)];
            }
        }
    }
}

fn draw_icon(pm: &mut Pixmap, l: Layout, icon: &PillIcon) {
    let square = Rect::from_xywh(l.at(PAD_X), l.at(ICON_Y), l.len(ICON), l.len(ICON))
        .expect("an icon square");
    let back = solid([255, 255, 255], 0.12);
    let id = Transform::identity();
    let rounded = rounded_rect(square, l.len(ICON_RADIUS));
    pm.fill_path(&rounded, &back, FillRule::Winding, id, None);
    // The glyphs are drawn in the square's own 26 × 26 units.
    let t = Transform::from_row(l.scale, 0.0, 0.0, l.scale, square.x(), square.y());
    let ink = solid(INK, 1.0);
    let line = Stroke {
        width: 1.4,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    let fill = |pm: &mut Pixmap, path: Path| {
        pm.fill_path(&path, &ink, FillRule::Winding, t, None);
    };
    match icon {
        PillIcon::Video => {
            // Nudged right of centre, where a triangle looks centred.
            let mut p = PathBuilder::new();
            p.move_to(10.0, 8.5);
            p.line_to(18.0, 13.0);
            p.line_to(10.0, 17.5);
            p.close();
            fill(pm, p.finish().expect("a triangle"));
        }
        PillIcon::Image => {
            let frame = Rect::from_xywh(6.5, 8.0, 13.0, 10.0).expect("a frame");
            pm.stroke_path(&rounded_rect(frame, 1.5), &ink, &line, t, None);
            fill(
                pm,
                PathBuilder::from_circle(10.0, 11.3, 1.3).expect("a sun"),
            );
            let mut p = PathBuilder::new();
            p.move_to(7.6, 17.2);
            p.line_to(11.6, 13.0);
            p.line_to(14.0, 15.4);
            p.line_to(15.7, 13.8);
            p.line_to(18.4, 17.2);
            p.close();
            fill(pm, p.finish().expect("hills"));
        }
        PillIcon::Folder => {
            let mut p = PathBuilder::new();
            p.move_to(6.5, 10.0);
            p.quad_to(6.5, 8.5, 8.0, 8.5);
            p.line_to(11.0, 8.5);
            p.line_to(12.5, 10.0);
            p.line_to(18.0, 10.0);
            p.quad_to(19.5, 10.0, 19.5, 11.5);
            p.line_to(19.5, 16.5);
            p.quad_to(19.5, 18.0, 18.0, 18.0);
            p.line_to(8.0, 18.0);
            p.quad_to(6.5, 18.0, 6.5, 16.5);
            p.close();
            fill(pm, p.finish().expect("a folder"));
        }
        PillIcon::Document => {
            let mut p = PathBuilder::new();
            p.move_to(9.0, 6.5);
            p.line_to(14.5, 6.5);
            p.line_to(18.0, 10.0);
            p.line_to(18.0, 19.5);
            p.line_to(9.0, 19.5);
            p.close();
            // The folded corner.
            p.move_to(14.5, 6.5);
            p.line_to(14.5, 10.0);
            p.line_to(18.0, 10.0);
            let page = p.finish().expect("a page");
            pm.stroke_path(&page, &ink, &line, t, None);
        }
        PillIcon::Count(n) => {
            // Shrunk to fit if it has many digits.
            let text = n.to_string();
            let room = l.len(ICON - 6.0);
            let mut size = l.len(COUNT_SIZE);
            let width = text_width(&FONTS.semibold, size, &text);
            if width > room {
                size *= room / width;
            }
            let width = text_width(&FONTS.semibold, size, &text);
            let x = square.x() + (square.width() - width) / 2.0;
            let baseline = square.y() + (square.height() + CAP_HEIGHT * size) / 2.0;
            draw_text(pm, &FONTS.semibold, size, &text, x, baseline, 1.0);
        }
    }
}

/// The bar for a known fraction, a thin track while running without one,
/// or nothing (cancelled or failed).
fn draw_bar(pm: &mut Pixmap, l: Layout, view: &PillView) {
    let (x, y) = (l.at(META_X), l.at(BAR_Y));
    let (w, h) = (l.len(META_RIGHT - META_X), l.len(BAR_H));
    let track = solid([255, 255, 255], 0.14);
    let id = Transform::identity();
    match view.fraction {
        Some(f) => {
            let rect = Rect::from_xywh(x, y, w, h).expect("a bar");
            let path = rounded_rect(rect, h / 2.0);
            pm.fill_path(&path, &track, FillRule::Winding, id, None);
            let fw = w * f.clamp(0.0, 1.0);
            let Some(rect) = Rect::from_xywh(x, y, fw, h) else {
                return;
            };
            // The sheen's tiles slide two widths per period, as in the mockup.
            let phase = if view.sheen_phase.is_finite() {
                view.sheen_phase.rem_euclid(1.0)
            } else {
                0.0
            };
            let tile = l.len(SHEEN_W);
            let from = x + tile * (2.0 * phase - 1.0);
            let grey = Color::from_rgba8(0xD9, 0xD9, 0xD9, 0xFF);
            let stops = vec![
                GradientStop::new(0.0, grey),
                GradientStop::new(0.5, Color::WHITE),
                GradientStop::new(1.0, grey),
            ];
            let shader = LinearGradient::new(
                Point::from_xy(from, 0.0),
                Point::from_xy(from + tile, 0.0),
                stops,
                SpreadMode::Repeat,
                id,
            )
            .unwrap_or(Shader::SolidColor(Color::WHITE));
            let paint = Paint {
                shader,
                anti_alias: true,
                ..Paint::default()
            };
            let path = rounded_rect(rect, h / 2.0);
            pm.fill_path(&path, &paint, FillRule::Winding, id, None);
        }
        None if view.running => {
            let th = l.len(THIN_H);
            let rect = Rect::from_xywh(x, y + (h - th) / 2.0, w, th).expect("a track");
            let path = rounded_rect(rect, th / 2.0);
            pm.fill_path(&path, &track, FillRule::Winding, id, None);
        }
        None => {}
    }
}

/// A ✓ centred on (cx, cy), `k` of its full size.
fn draw_check(pm: &mut Pixmap, l: Layout, cx: f32, cy: f32, k: f32) {
    let mut p = PathBuilder::new();
    p.move_to(-4.0, 0.3);
    p.line_to(-1.3, 3.0);
    p.line_to(4.0, -3.0);
    let s = l.scale * k;
    let t = Transform::from_row(s, 0.0, 0.0, s, cx, cy);
    let stroke = Stroke {
        width: 1.8,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    let check = p.finish().expect("a check");
    pm.stroke_path(&check, &solid(INK, 1.0), &stroke, t, None);
}

/// A ✕ centred on (cx, cy).
fn draw_close(pm: &mut Pixmap, l: Layout, cx: f32, cy: f32) {
    // Round caps reach `width / 2` past the ends, filling CLOSE_W.
    let width = 1.5;
    let r = (CLOSE_W - width) / 2.0;
    let mut p = PathBuilder::new();
    p.move_to(-r, -r);
    p.line_to(r, r);
    p.move_to(r, -r);
    p.line_to(-r, r);
    let t = Transform::from_row(l.scale, 0.0, 0.0, l.scale, cx, cy);
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    let cross = p.finish().expect("a cross");
    pm.stroke_path(&cross, &solid(INK, 1.0), &stroke, t, None);
}

fn solid(rgb: [u8; 3], alpha: f32) -> Paint<'static> {
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    let [r, g, b] = rgb.map(|c| f32::from(c) / 255.0);
    paint.set_color(Color::from_rgba(r, g, b, alpha).expect("a colour in range"));
    paint
}

/// `rect` with corners of radius `r`, at most half its width or height.
fn rounded_rect(rect: Rect, r: f32) -> Path {
    // How far along a cubic's handles sit to draw a quarter circle.
    const K: f32 = 0.552_284_8;
    let r = r.min(rect.width() / 2.0).min(rect.height() / 2.0).max(0.0);
    let (l, t, rt, b) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let c = r * (1.0 - K);
    let mut p = PathBuilder::new();
    p.move_to(l + r, t);
    p.line_to(rt - r, t);
    p.cubic_to(rt - c, t, rt, t + c, rt, t + r);
    p.line_to(rt, b - r);
    p.cubic_to(rt, b - c, rt - c, b, rt - r, b);
    p.line_to(l + r, b);
    p.cubic_to(l + c, b, l, b - c, l, b - r);
    p.line_to(l, t + r);
    p.cubic_to(l, t + c, l + c, t, l + r, t);
    p.close();
    p.finish().expect("a non-empty rect")
}

/// The scale that draws `font` with an em of `px`.
fn em(font: &FontRef, px: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(2048.0);
    PxScale::from(px * font.height_unscaled() / upem)
}

/// Each char's glyph and where it starts along one line, and the line's
/// width. Kerning comes from the font's `kern` table, if it has one.
fn glyph_run(font: &FontRef, px: f32, text: &str) -> (Vec<(GlyphId, f32)>, f32) {
    let sf = font.as_scaled(em(font, px));
    let mut pen = 0.0;
    let mut prev = None;
    let mut run = Vec::with_capacity(text.len());
    for c in text.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            pen += sf.kern(p, id);
        }
        run.push((id, pen));
        pen += sf.h_advance(id);
        prev = Some(id);
    }
    (run, pen)
}

fn text_width(font: &FontRef, px: f32, text: &str) -> f32 {
    glyph_run(font, px, text).1
}

/// `text` in NFC, without default-ignorable chars (zero-width ones,
/// variation selectors, tags, skin tones), and with each run of chars
/// `font` has no glyph for as one `MISSING`.
fn displayable<'a>(font: &FontRef, text: &'a str) -> Cow<'a, str> {
    let ignorable = |c: char| {
        matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{E0020}'..='\u{E007F}'
                | '\u{1F3FB}'..='\u{1F3FF}'
        )
    };
    let missing = |c: char| font.glyph_id(c) == GlyphId(0);
    let clean = text.chars().all(|c| !ignorable(c) && !missing(c));
    if clean && unicode_normalization::is_nfc(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.nfc().filter(|&c| !ignorable(c)) {
        if missing(c) {
            if !in_run {
                out.push(MISSING);
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    Cow::Owned(out)
}

/// `text` if it fits in `max` px, else as much of it as fits before an "…".
fn ellipsize<'a>(font: &FontRef, px: f32, text: &'a str, max: f32) -> Cow<'a, str> {
    let (run, width) = glyph_run(font, px, text);
    if width <= max {
        return Cow::Borrowed(text);
    }
    let sf = font.as_scaled(em(font, px));
    let dots = sf.glyph_id('…');
    let dots_w = sf.h_advance(dots);
    // The longest prefix that fits with the "…" after it.
    let mut cut = 0;
    for (i, (byte, _)) in text.char_indices().enumerate().skip(1) {
        let (prev, start) = run[i - 1];
        let end = start + sf.h_advance(prev);
        if end + sf.kern(prev, dots) + dots_w > max {
            break;
        }
        cut = byte;
    }
    Cow::Owned(format!("{}…", text[..cut].trim_end()))
}

/// Draws `text` from `x` with its baseline at `y`, in `INK` at `alpha`.
fn draw_text(pm: &mut Pixmap, font: &FontRef, px: f32, text: &str, x: f32, y: f32, alpha: f32) {
    let scale = em(font, px);
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for (id, pen) in glyph_run(font, px, text).0 {
        let glyph = id.with_scale_and_position(scale, point(x + pen, y));
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let min = outline.px_bounds().min;
        outline.draw(|gx, gy, coverage| {
            let (tx, ty) = (min.x as i32 + gx as i32, min.y as i32 + gy as i32);
            if tx < 0 || ty < 0 || tx >= w || ty >= h {
                return;
            }
            // Source-over, premultiplied.
            let a = (coverage * alpha).clamp(0.0, 1.0);
            let i = (ty * w + tx) as usize * 4;
            for (d, s) in data[i..i + 4].iter_mut().zip([INK[0], INK[1], INK[2], 255]) {
                *d = (f32::from(s) * a + f32::from(*d) * (1.0 - a)).round() as u8;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running() -> PillView {
        PillView {
            drag: 1,
            icon: PillIcon::Video,
            name: "holiday-video.mp4".into(),
            right: "42%".into(),
            sub: "From PC · 2.1 of 5.0 GB · 1 min left".into(),
            fraction: Some(0.42),
            sheen_phase: 0.0,
            check_scale: 0.0,
            opacity: 1.0,
            failed: false,
            running: true,
        }
    }

    /// A running copy with an unknown total: nothing on the right.
    fn folder() -> PillView {
        PillView {
            icon: PillIcon::Folder,
            name: "Photos".into(),
            right: String::new(),
            sub: "From Mac · 7 MB".into(),
            fraction: None,
            ..running()
        }
    }

    fn px(pm: &Pixmap, x: f32, y: f32) -> [u8; 4] {
        let p = pm.pixel(x as u32, y as u32).expect("inside the pixmap");
        [p.red(), p.green(), p.blue(), p.alpha()]
    }

    /// The brightest red channel in the 3 × 3 pixels around (x, y).
    fn brightest(pm: &Pixmap, x: f32, y: f32) -> u8 {
        let (x, y) = (x as i32, y as i32);
        (-1..=1)
            .flat_map(|dy| (-1..=1).map(move |dx| (x + dx, y + dy)))
            .map(|(x, y)| px(pm, x as f32, y as f32)[0])
            .max()
            .unwrap()
    }

    #[test]
    fn body_alpha_matches_backdrop() {
        // The shadow is cut away under the body, as a CSS box-shadow is, so a
        // body pixel is the fill alone: 0.62 × 255 ≈ 158, 0.80 × 255 = 204.
        for scale in [1.0, 2.0] {
            let l = Layout::new(scale);
            for (backdrop, want) in [(Backdrop::Blurred, 158u8), (Backdrop::Opaque, 204)] {
                let pm = render(&running(), scale, backdrop, false);
                let [r, g, b, a] = px(&pm, l.at(PAD_X / 2.0), l.at(PILL_H / 2.0));
                assert!(a.abs_diff(want) <= 3, "{backdrop:?} @{scale}: alpha {a}");
                // rgb(18, 18, 18), premultiplied.
                let want_rgb = (18.0 * f32::from(a) / 255.0).round() as u8;
                for c in [r, g, b] {
                    assert!(c.abs_diff(want_rgb) <= 1, "{backdrop:?} @{scale}: {c}");
                }
            }
        }
    }

    #[test]
    fn bar_fill_tracks_fraction() {
        for scale in [1.0, 2.0] {
            let l = Layout::new(scale);
            let pm = render(&running(), scale, Backdrop::Blurred, false);
            let y = l.at(BAR_Y + BAR_H / 2.0);
            let (x0, x1) = (l.at(META_X), l.at(META_RIGHT));
            let lit = |x: u32| px(&pm, x as f32, y)[0] > 128;
            let end = (x0 as u32..x1 as u32).rfind(|&x| lit(x)).expect("a fill");
            // One white run from the bar's start.
            assert!((x0.ceil() as u32..end).all(lit), "@{scale}");
            let want = x0 + 0.42 * (x1 - x0);
            let got = (end + 1) as f32;
            assert!(
                (got - want).abs() <= 1.0,
                "@{scale}: ends {got}, want {want}"
            );
            // The track beyond is dim.
            assert!(!lit(end + 2) && !lit(x1 as u32 - 3), "@{scale}");
        }
    }

    #[test]
    fn long_name_is_ellipsized() {
        let name: String = "Very long file name ".repeat(10);
        assert_eq!(name.chars().count(), 200);
        let long = PillView { name, ..running() };
        let blank = PillView {
            name: String::new(),
            ..running()
        };
        for scale in [1.0, 2.0] {
            let l = Layout::new(scale);
            let size = l.len(NAME_SIZE);
            let (left, right) = name_box(&long, scale, false);
            let fitted = ellipsize(&FONTS.semibold, size, &long.name, right - left);
            assert!(fitted.ends_with('…'), "{fitted:?}");
            let width = text_width(&FONTS.semibold, size, &fitted);
            assert!(
                width <= right - left,
                "@{scale}: {width} > {}",
                right - left
            );
            // Cut close to the end of the box, not early.
            assert!(width > right - left - 2.0 * size, "@{scale}: {width}");

            // On screen, nothing of the name reaches the gap before the percent.
            let (a, b) = (
                render(&long, scale, Backdrop::Blurred, false),
                render(&blank, scale, Backdrop::Blurred, false),
            );
            let rows = l.at(NAME_BASELINE - NAME_SIZE) as u32..l.at(NAME_BASELINE + 4.0) as u32;
            let cols = right.ceil() as u32 + 1..(right + l.len(RIGHT_GAP)) as u32 - 1;
            assert!(!cols.is_empty());
            for y in rows {
                for x in cols.clone() {
                    let (x, y) = (x as f32, y as f32);
                    assert_eq!(px(&a, x, y), px(&b, x, y), "@{scale} ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn name_box_holds_still_as_the_percent_changes() {
        let with = |right: &str| {
            let v = PillView {
                right: right.into(),
                ..running()
            };
            name_box(&v, 2.0, false)
        };
        let still = with("42%");
        for right in ["0%", "7%", "11%", "40%", "88%", "99%"] {
            assert_eq!(with(right), still, "{right}");
        }
    }

    #[test]
    fn close_only_on_hover_or_failed() {
        let failed = PillView {
            failed: true,
            running: false,
            sub: "Copy failed — Mac disconnected".into(),
            ..folder()
        };
        let done = PillView {
            right: String::new(),
            sub: "From PC · 5.0 GB".into(),
            fraction: Some(1.0),
            check_scale: 1.0,
            running: false,
            ..running()
        };
        let cancelled = PillView {
            right: String::new(),
            sub: "Cancelled".into(),
            fraction: None,
            running: false,
            ..running()
        };
        assert!(!close_shown(&folder(), false) && close_shown(&folder(), true));
        assert!(close_shown(&failed, false) && close_shown(&failed, true));
        for v in [&done, &cancelled] {
            assert!(!close_shown(v, false) && !close_shown(v, true), "{}", v.sub);
        }
        for scale in [1.0, 2.0] {
            let r = close_rect(scale);
            let (w, h) = pixmap_px(scale);
            assert!(r.left() >= 0.0 && r.top() >= 0.0);
            assert!(r.right() <= w as f32 && r.bottom() <= h as f32);
            let (cx, cy) = (r.x() + r.width() / 2.0, r.y() + r.height() / 2.0);
            let lit = |v: &PillView, hover: bool| {
                brightest(&render(v, scale, Backdrop::Blurred, hover), cx, cy) > 150
            };
            assert!(!lit(&folder(), false), "@{scale}");
            assert!(lit(&folder(), true), "@{scale}");
            assert!(lit(&failed, false), "@{scale}");

            // The ✕ takes the percent's place.
            let blank = PillView {
                right: String::new(),
                ..running()
            };
            let with = |v: &PillView, hover: bool| {
                render(v, scale, Backdrop::Opaque, hover).data().to_vec()
            };
            assert!(with(&running(), true) == with(&blank, true), "@{scale}");
            assert!(with(&running(), false) != with(&blank, false), "@{scale}");

            // Hovering a done pill keeps its ✓, and a cancelled one shows
            // nothing more.
            for v in [&done, &cancelled] {
                assert!(with(v, true) == with(v, false), "@{scale} {}", v.sub);
            }
            let check = render(&done, scale, Backdrop::Opaque, false);
            let x = l_at(scale, META_RIGHT - CHECK_W / 2.0);
            assert!(brightest(&check, x, l_at(scale, MARK_Y)) > 150, "@{scale}");
        }
    }

    fn l_at(scale: f32, v: f32) -> f32 {
        Layout::new(scale).at(v)
    }

    #[test]
    fn track_only_while_running() {
        let failed = PillView {
            failed: true,
            running: false,
            sub: "Copy failed — Mac disconnected".into(),
            ..folder()
        };
        let cancelled = PillView {
            running: false,
            sub: "Cancelled".into(),
            ..folder()
        };
        for scale in [1.0, 2.0] {
            let l = Layout::new(scale);
            let y = l.at(BAR_Y + BAR_H / 2.0);
            let (x0, x1) = (l.at(META_X) as u32, l.at(META_RIGHT) as u32);
            let body = |pm: &Pixmap| px(pm, l.at(PAD_X / 2.0), l.at(PILL_H / 2.0));
            // Running with no fraction: a thin track the bar's whole width.
            let pm = render(&folder(), scale, Backdrop::Blurred, false);
            let base = body(&pm)[0];
            for x in x0 + 2..x1 - 2 {
                let r = px(&pm, x as f32, y)[0];
                assert!(r > base + 20, "@{scale} x {x}: {r} vs {base}");
            }
            // Failed or cancelled: the bar row is bare body.
            for v in [&failed, &cancelled] {
                let pm = render(v, scale, Backdrop::Blurred, false);
                for x in x0..x1 {
                    assert_eq!(px(&pm, x as f32, y), body(&pm), "@{scale} {} x {x}", v.sub);
                }
            }
        }
    }

    #[test]
    fn missing_glyphs_become_one_box() {
        for (name, want) in [
            ("🎉 party.jpg", "□ party.jpg"),
            ("👨\u{200D}👩\u{200D}👧 family.png", "□ family.png"),
            ("👍🏽 ok", "□ ok"),
            ("e\u{301}te\u{200B}.txt", "\u{e9}te.txt"),
            ("plain.txt", "plain.txt"),
        ] {
            for font in [&FONTS.semibold, &FONTS.regular] {
                let shown = displayable(font, name);
                assert_eq!(shown, want, "{name:?}");
                assert_eq!(
                    shown.matches(MISSING).count(),
                    usize::from(want.contains(MISSING))
                );
                let (run, _) = glyph_run(font, NAME_SIZE, &shown);
                assert!(run.iter().all(|&(id, _)| id != GlyphId(0)), "{name:?}");
            }
            let v = PillView {
                name: name.into(),
                sub: format!("Copy failed — “{name}” changed while copying"),
                ..running()
            };
            render(&v, 2.0, Backdrop::Blurred, true);
        }
    }

    #[test]
    fn broken_numbers_dont_panic() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -0.25] {
            for v in [
                PillView {
                    sheen_phase: bad,
                    ..running()
                },
                PillView {
                    fraction: Some(bad),
                    ..running()
                },
                PillView {
                    check_scale: bad,
                    right: String::new(),
                    running: false,
                    ..running()
                },
                PillView {
                    opacity: bad,
                    ..running()
                },
            ] {
                for scale in [bad, 1.0, 2.0] {
                    render(&v, scale, Backdrop::Opaque, false);
                }
            }
        }
        // A broken phase draws the bar as phase 0 does.
        let nan = PillView {
            sheen_phase: f32::NAN,
            ..running()
        };
        let zero = render(&running(), 1.0, Backdrop::Blurred, false);
        assert!(render(&nan, 1.0, Backdrop::Blurred, false).data() == zero.data());
    }

    #[test]
    fn opacity_scales_alpha() {
        let full = render(&running(), 2.0, Backdrop::Blurred, false);
        let half = render(
            &PillView {
                opacity: 0.5,
                ..running()
            },
            2.0,
            Backdrop::Blurred,
            false,
        );
        let mut seen = 0;
        for (f, h) in full.pixels().iter().zip(half.pixels()) {
            let want = f32::from(f.alpha()) * 0.5;
            assert!((f32::from(h.alpha()) - want).abs() <= 1.0);
            seen += usize::from(f.alpha() > 100);
        }
        assert!(seen > 1000, "{seen} opaque-ish pixels");
        let gone = render(
            &PillView {
                opacity: 0.0,
                ..running()
            },
            2.0,
            Backdrop::Blurred,
            false,
        );
        assert!(gone.pixels().iter().all(|p| p.alpha() == 0));
    }

    #[test]
    fn scale_2_doubles_pixels() {
        assert_eq!(pill_px(1.0), (360, 52));
        assert_eq!(pill_px(2.0), (720, 104));
        let one = render(&running(), 1.0, Backdrop::Blurred, false);
        let two = render(&running(), 2.0, Backdrop::Blurred, false);
        assert_eq!((one.width(), one.height()), (384, 76));
        assert_eq!((two.width(), two.height()), (768, 152));
        let (a, b) = (close_rect(1.0), close_rect(2.0));
        assert_eq!(
            (b.x(), b.y(), b.width(), b.height()),
            (2.0 * a.x(), 2.0 * a.y(), 2.0 * a.width(), 2.0 * a.height())
        );
        // The same body pixel, twice as far in.
        let (l1, l2) = (Layout::new(1.0), Layout::new(2.0));
        let at = |pm: &Pixmap, l: Layout| px(pm, l.at(PAD_X / 2.0), l.at(PILL_H / 2.0));
        assert_eq!(at(&one, l1), at(&two, l2));
        assert_eq!(l2.at(PAD_X / 2.0), 2.0 * l1.at(PAD_X / 2.0));
    }

    /// Writes example pills at scale 2 over a light and a dark wallpaper:
    /// `cargo test -p cursorjump --release preview_png -- --ignored --nocapture`.
    #[test]
    #[ignore = "writes a PNG for a person to look at"]
    fn preview_png() {
        use tiny_skia::{PixmapPaint, RadialGradient};

        let views = [
            PillView {
                sheen_phase: 0.3,
                ..running()
            },
            PillView {
                icon: PillIcon::Count(3),
                name: "3 files".into(),
                right: String::new(),
                sub: "From PC · 36 MB".into(),
                fraction: Some(1.0),
                check_scale: 1.0,
                running: false,
                ..running()
            },
            PillView {
                sub: "From Mac · 7 MB".into(),
                ..folder()
            },
            PillView {
                icon: PillIcon::Image,
                name: "IMG_2041.HEIC".into(),
                right: String::new(),
                sub: "Copy failed — PC disconnected".into(),
                fraction: None,
                failed: true,
                running: false,
                ..running()
            },
            PillView {
                icon: PillIcon::Document,
                name: "Quarterly report final final (really the last one) v12.pdf".into(),
                right: "7%".into(),
                sub: "From Mac · 3 of 41 MB · 15 s left".into(),
                fraction: Some(0.07),
                ..running()
            },
            PillView {
                icon: PillIcon::Count(128),
                name: "128 items".into(),
                right: "87%".into(),
                sub: "From PC · 1.7 of 2.0 GB · a few seconds left".into(),
                fraction: Some(0.87),
                sheen_phase: 0.7,
                ..running()
            },
            PillView {
                icon: PillIcon::Image,
                name: "🎉 party 👨\u{200D}👩\u{200D}👧.jpg".into(),
                right: "63%".into(),
                sub: "From PC · 12 of 19 MB · 5 s left".into(),
                fraction: Some(0.63),
                ..running()
            },
            PillView {
                icon: PillIcon::Document,
                name: "backup.zip".into(),
                right: String::new(),
                sub: "Cancelled".into(),
                fraction: None,
                running: false,
                opacity: 0.6,
                ..running()
            },
        ];
        let hovered = 4;
        let scale = 2.0;
        let (pw, ph) = pixmap_px(scale);
        let pad = Layout::new(scale).origin;
        let margin = 40.0;
        let step = (PILL_H + PILL_GAP) * scale;
        let col_w = pw as f32 + 2.0 * margin;
        let height = margin + views.len() as f32 * step + BOTTOM_MARGIN * scale;
        let mut out = Pixmap::new(2 * col_w as u32, height as u32).unwrap();
        for (col, (inner, outer, backdrop)) in [
            ((255, 255, 255), (0xCF, 0xCF, 0xCF), Backdrop::Blurred),
            ((0x3B, 0x3B, 0x3B), (0x0D, 0x0D, 0x0D), Backdrop::Opaque),
        ]
        .into_iter()
        .enumerate()
        {
            let x0 = col as f32 * col_w;
            let rgb = |(r, g, b): (u8, u8, u8)| Color::from_rgba8(r, g, b, 255);
            let centre = Point::from_xy(x0 + 0.3 * col_w, 0.2 * height);
            let shader = RadialGradient::new(
                centre,
                0.0,
                centre,
                0.8 * col_w,
                vec![
                    GradientStop::new(0.0, rgb(inner)),
                    GradientStop::new(1.0, rgb(outer)),
                ],
                SpreadMode::Pad,
                Transform::identity(),
            )
            .unwrap();
            let wall = Paint {
                shader,
                ..Paint::default()
            };
            let rect = Rect::from_xywh(x0, 0.0, col_w, height).unwrap();
            out.fill_rect(rect, &wall, Transform::identity(), None);
            for (i, v) in views.iter().enumerate() {
                let pill = render(v, scale, backdrop, i == hovered);
                let y = margin + i as f32 * step - pad;
                let x = x0 + margin;
                let paint = PixmapPaint::default();
                out.draw_pixmap(
                    x as i32,
                    y as i32,
                    pill.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
        }
        let dir = std::env::temp_dir().join("cursorjump-preview");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pill_preview.png");
        out.save_png(&path).unwrap();
        println!("pill preview: {}", path.display());
        assert_eq!((pw, ph), (768, 152));

        // Time a frame once the base is cached.
        for scale in [1.0, 2.0] {
            render(&running(), scale, Backdrop::Blurred, false);
            let n = 200;
            let t = std::time::Instant::now();
            for i in 0..n {
                let v = PillView {
                    sheen_phase: i as f32 / n as f32,
                    ..running()
                };
                std::hint::black_box(render(&v, scale, Backdrop::Blurred, false));
            }
            println!("scale {scale}: {:?} per pill", t.elapsed() / n);
            let t = std::time::Instant::now();
            std::hint::black_box(draw_base(Layout::new(scale), Backdrop::Opaque));
            println!("scale {scale}: {:?} for the uncached base", t.elapsed());
        }
    }
}
