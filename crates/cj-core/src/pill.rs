//! The progress pill shown on the machine receiving a drag: its state,
//! timings and exact text. The hosts draw the [`PillView`]s.
//!
//! Readings of the rules:
//! - Only a drag that finishes or is cancelled within `SHOW_AFTER` is never
//!   shown. A failure is shown at once, however soon it comes.
//! - While held, nothing expires. On release, every finished pill still on
//!   the board replays its ✓, hold and fade from the release, since the user
//!   never saw them; a running pill just carries on.
//! - The rate behind the ETA is the change between the oldest and newest
//!   progress samples of the last 5 s, so there is no ETA once nothing has been
//!   heard for 5 s or nothing moved. ETAs round up, so they err long.
//! - A `done` past the total raises the total, so the text never reads
//!   "12 of 10 MB". A total of 0 has no bar or percentage.
use crate::dnd::{DragId, DragItem, FailReason, ItemKind};
use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// "tiny files": a drag that finishes sooner than this after `start` is never shown.
pub const SHOW_AFTER: Duration = Duration::from_millis(500);
/// How long a finished pill stays fully visible before it fades.
pub const DONE_HOLD: Duration = Duration::from_secs(1);
/// How long a finished pill takes to fade out after `DONE_HOLD`.
pub const FADE: Duration = Duration::from_millis(300);
/// How long the ✓ takes to grow from `CHECK_FROM` to full size.
pub const CHECK_POP: Duration = Duration::from_millis(150);
/// One pass of the sheen along the bar.
pub const SHEEN_PERIOD: Duration = Duration::from_millis(1600);
/// How long after `start` an ETA may first be shown.
pub const ETA_AFTER: Duration = Duration::from_secs(2);

/// Interval `next_frame` asks for while something animates.
const FRAME: Duration = Duration::from_millis(16);
/// Progress samples older than this don't count toward the rate.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// Closest two kept progress samples may be, but for the newest one, which
/// is replaced until it is this far past the one before it.
const SAMPLE_GAP: Duration = Duration::from_millis(100);
/// The ✓'s scale when it first appears.
const CHECK_FROM: f32 = 0.6;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Peer {
    Pc,
    Mac,
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Peer::Pc => "PC",
            Peer::Mac => "Mac",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PillIcon {
    Video,
    Image,
    Folder,
    Document,
    /// Several items; the number is drawn in the square.
    Count(u32),
}

/// The icon for a drag: a count for several items, else by kind and, for a
/// file, by extension (ignoring case).
pub fn icon_for(items: &[DragItem]) -> PillIcon {
    let [item] = items else {
        return match items.len() {
            0 => PillIcon::Document,
            n => PillIcon::Count(u32::try_from(n).unwrap_or(u32::MAX)),
        };
    };
    if item.kind == ItemKind::Dir {
        return PillIcon::Folder;
    }
    // A leading dot starts a hidden name, not an extension.
    let ext = match item.name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => return PillIcon::Document,
    };
    match ext.as_str() {
        "mp4" | "mov" | "mkv" | "avi" | "webm" => PillIcon::Video,
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "webp" | "bmp" | "tiff" => PillIcon::Image,
        _ => PillIcon::Document,
    }
}

/// The pill's name line: the item's name ("file" if not known yet, or for no
/// items), or "n files", or "n items" if any of them is a folder.
pub fn title_for(items: &[DragItem]) -> String {
    match items {
        [] => "file".to_string(),
        [item] if item.name.is_empty() => "file".to_string(),
        [item] => item.name.clone(),
        _ if items.iter().any(|i| i.kind == ItemKind::Dir) => format!("{} items", items.len()),
        _ => format!("{} files", items.len()),
    }
}

/// One pill as it should be drawn right now.
#[derive(Debug, Clone, PartialEq)]
pub struct PillView {
    pub drag: DragId,
    pub icon: PillIcon,
    pub name: String,
    /// "42%" while running with a known total; "" otherwise. The ✓ is drawn
    /// from `check_scale`.
    pub right: String,
    pub sub: String,
    /// How full the bar is. `None` hides the bar: an unknown or zero total,
    /// a cancel or a failure.
    pub fraction: Option<f32>,
    /// Where the sheen is along the bar, in [0, 1). Only meaningful while
    /// running (a bar is shown and `check_scale` is 0); 0 otherwise.
    pub sheen_phase: f32,
    /// 0 until done; pops from 0.6 to 1.0 over `CHECK_POP`.
    pub check_scale: f32,
    pub opacity: f32,
    /// Persists until `dismiss`; drawn with a ✕.
    pub failed: bool,
}

#[derive(Debug, Clone)]
enum Stage {
    Running,
    /// Done, or cancelled. Hold and fade are timed from `at`: the finish, or
    /// the release if it finished or was still showing while held.
    Finished {
        cancelled: bool,
        at: Instant,
    },
    Failed(FailReason),
}

#[derive(Debug, Clone)]
struct Pill {
    drag: DragId,
    icon: PillIcon,
    name: String,
    started: Instant,
    done: u64,
    total: Option<u64>,
    /// `(when, done)` from `progress`, from the last 5 s, at least
    /// `SAMPLE_GAP` apart but for the newest.
    samples: VecDeque<(Instant, u64)>,
    stage: Stage,
}

impl Pill {
    /// The total the bar and percentage are drawn against: a known one above 0.
    fn bar_total(&self) -> Option<u64> {
        self.total.filter(|&t| t > 0)
    }

    /// Time left at the rate of the last 5 s, once `ETA_AFTER` has passed.
    fn eta(&self, now: Instant) -> Option<Duration> {
        let total = self.total?;
        if now.saturating_duration_since(self.started) < ETA_AFTER {
            return None;
        }
        let recent = |&&(t, _): &&(Instant, u64)| now.saturating_duration_since(t) <= RATE_WINDOW;
        let &(t1, d1) = self.samples.back().filter(recent)?;
        let &(t0, d0) = self.samples.iter().find(recent)?;
        let moved = d1.checked_sub(d0).filter(|&m| m > 0)?;
        let span = t1.saturating_duration_since(t0);
        if span.is_zero() {
            return None;
        }
        let left = u128::from(total.saturating_sub(self.done));
        let nanos = left * span.as_nanos() / u128::from(moved);
        Some(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Whether a finished pill has faded out completely.
    fn expired(&self, now: Instant) -> bool {
        match &self.stage {
            Stage::Finished { at, .. } => now.saturating_duration_since(*at) >= DONE_HOLD + FADE,
            _ => false,
        }
    }
}

/// `part` of `whole`, clamped to [0, 1].
fn ratio(part: Duration, whole: Duration) -> f32 {
    (part.as_secs_f64() / whole.as_secs_f64()).min(1.0) as f32
}

/// The pills on one machine, one per incoming drag, stacked in start order.
#[derive(Debug, Clone)]
pub struct PillBoard {
    from: Peer,
    local: Peer,
    /// Bottom-most first.
    pills: Vec<Pill>,
    held: bool,
}

impl PillBoard {
    /// A board for drags coming from `from`, shown on `local`.
    pub fn new(from: Peer, local: Peer) -> PillBoard {
        PillBoard {
            from,
            local,
            pills: Vec::new(),
            held: false,
        }
    }

    /// A new drag, at the drop moment. Shown from `SHOW_AFTER` on. The total
    /// is the items' sizes summed, if all are known. A drag already on the
    /// board is left as it is.
    pub fn start(&mut self, drag: DragId, items: &[DragItem], now: Instant) {
        self.prune(now);
        if self.pills.iter().any(|p| p.drag == drag) {
            return;
        }
        let total = items
            .iter()
            .try_fold(0u64, |sum, i| sum.checked_add(i.size?));
        self.pills.push(Pill {
            drag,
            icon: icon_for(items),
            name: title_for(items),
            started: now,
            done: 0,
            total,
            samples: VecDeque::new(),
            stage: Stage::Running,
        });
    }

    /// Bytes received so far. A `None` total keeps the one already known,
    /// and a total below `done` is raised to it. Ignored unless the drag is
    /// running.
    pub fn progress(&mut self, drag: DragId, done: u64, total: Option<u64>, now: Instant) {
        self.prune(now);
        let Some(p) = self.running(drag) else {
            return;
        };
        p.done = done;
        p.total = total.or(p.total).map(|t| t.max(done));
        // The newest sample slides until it is SAMPLE_GAP past the one before.
        let n = p.samples.len();
        let young = n >= 2
            && p.samples[n - 1]
                .0
                .saturating_duration_since(p.samples[n - 2].0)
                < SAMPLE_GAP;
        if young {
            p.samples.pop_back();
        }
        p.samples.push_back((now, done));
        while let Some(&(t, _)) = p.samples.front() {
            if now.saturating_duration_since(t) > RATE_WINDOW {
                p.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// The drag ended. `Ok` shows the ✓ and fades, `Cancelled` fades the
    /// same way, and any other failure stays until dismissed. A drag that
    /// ends well or is cancelled within `SHOW_AFTER` is dropped unseen.
    /// Ignored unless the drag is running.
    pub fn finish(&mut self, drag: DragId, result: Result<(), FailReason>, now: Instant) {
        self.prune(now);
        let Some(p) = self.running(drag) else {
            return;
        };
        let cancelled = match result {
            Ok(()) => false,
            Err(FailReason::Cancelled) => true,
            Err(reason) => {
                p.stage = Stage::Failed(reason);
                return;
            }
        };
        if now.saturating_duration_since(p.started) < SHOW_AFTER {
            self.pills.retain(|p| p.drag != drag);
            return;
        }
        // While held, `set_held(false, ..)` moves `at` to the release.
        p.stage = Stage::Finished { cancelled, at: now };
    }

    /// Remove a failed pill (its ✕). Other pills are left alone; to stop a
    /// running copy, cancel it.
    pub fn dismiss(&mut self, drag: DragId) {
        self.pills
            .retain(|p| p.drag != drag || !matches!(p.stage, Stage::Failed(_)));
    }

    /// The PC game lock: while held, `views` is empty, `next_frame` is `None`
    /// and nothing expires. Holding first drops the pills already faded out.
    /// On release, the finished pills left replay their ✓, hold and fade from
    /// `now`; the host should call `views` and `next_frame` again then.
    pub fn set_held(&mut self, held: bool, now: Instant) {
        if held && !self.held {
            self.prune(now);
        }
        if !held && self.held {
            for p in &mut self.pills {
                if let Stage::Finished { at, .. } = &mut p.stage {
                    *at = now;
                }
            }
        }
        self.held = held;
    }

    /// The pills to draw, bottom-most first.
    pub fn views(&self, now: Instant) -> Vec<PillView> {
        if self.held {
            return Vec::new();
        }
        self.pills
            .iter()
            .filter_map(|p| self.view(p, now))
            .collect()
    }

    /// When the views next change by themselves: every `FRAME` while the
    /// sheen, ✓ or fade animates, the end of a hold, or when a pill becomes
    /// due. `None` when held or nothing is visible or pending. Calls to
    /// `progress` and `finish` change the views too.
    pub fn next_frame(&self, now: Instant) -> Option<Instant> {
        if self.held {
            return None;
        }
        self.pills
            .iter()
            .filter_map(|p| match &p.stage {
                Stage::Running => {
                    let shown = p.started + SHOW_AFTER;
                    if now < shown {
                        Some(shown)
                    } else {
                        p.bar_total().map(|_| now + FRAME)
                    }
                }
                &Stage::Finished { cancelled, at } => {
                    let (popped, faded) = (at + CHECK_POP, at + DONE_HOLD + FADE);
                    if !cancelled && now < popped {
                        Some((now + FRAME).min(popped))
                    } else if now < at + DONE_HOLD {
                        Some(at + DONE_HOLD)
                    } else if now < faded {
                        Some((now + FRAME).min(faded))
                    } else {
                        None
                    }
                }
                Stage::Failed(_) => None,
            })
            .min()
    }

    fn view(&self, p: &Pill, now: Instant) -> Option<PillView> {
        let mut v = PillView {
            drag: p.drag,
            icon: p.icon.clone(),
            name: p.name.clone(),
            right: String::new(),
            sub: String::new(),
            fraction: None,
            sheen_phase: 0.0,
            check_scale: 0.0,
            opacity: 1.0,
            failed: false,
        };
        match &p.stage {
            Stage::Running => {
                let since = now.saturating_duration_since(p.started);
                if since < SHOW_AFTER {
                    return None;
                }
                v.sub = format_sub(self.from, p.done, p.total, p.eta(now));
                if let Some(total) = p.bar_total() {
                    let done = p.done.min(total);
                    // Floored, and never 100 % until done.
                    let percent = (u128::from(done) * 100 / u128::from(total)).min(99);
                    v.right = format!("{percent}%");
                    v.fraction = Some((done as f64 / total as f64) as f32);
                    let period = SHEEN_PERIOD.as_nanos();
                    v.sheen_phase = ((since.as_nanos() % period) as f64 / period as f64) as f32;
                }
            }
            &Stage::Finished { cancelled, at } => {
                let since = now.saturating_duration_since(at);
                if since >= DONE_HOLD + FADE {
                    return None;
                }
                v.opacity = 1.0 - ratio(since.saturating_sub(DONE_HOLD), FADE);
                if cancelled {
                    v.sub = "Cancelled".to_string();
                } else {
                    v.sub = format_sub(self.from, p.total.unwrap_or(p.done), None, None);
                    v.fraction = Some(1.0);
                    v.check_scale = CHECK_FROM + (1.0 - CHECK_FROM) * ratio(since, CHECK_POP);
                }
            }
            Stage::Failed(reason) => {
                v.sub = fail_text(reason, self.from, self.local);
                v.failed = true;
            }
        }
        Some(v)
    }

    fn running(&mut self, drag: DragId) -> Option<&mut Pill> {
        self.pills
            .iter_mut()
            .find(|p| p.drag == drag && matches!(p.stage, Stage::Running))
    }

    /// Drop the pills that have faded out, unless held.
    fn prune(&mut self, now: Instant) {
        if !self.held {
            self.pills.retain(|p| !p.expired(now));
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Unit {
    Kb,
    Mb,
    Gb,
}

impl Unit {
    fn for_bytes(bytes: u64) -> Unit {
        match bytes {
            ..1_000_000 => Unit::Kb,
            1_000_000..1_000_000_000 => Unit::Mb,
            _ => Unit::Gb,
        }
    }

    /// `bytes` in this unit: KB rounded up, MB rounded, GB to one decimal.
    fn amount(self, bytes: u64) -> String {
        match self {
            Unit::Kb => bytes.div_ceil(1_000).to_string(),
            Unit::Mb => (bytes / 1_000_000 + u64::from(bytes % 1_000_000 >= 500_000)).to_string(),
            Unit::Gb => {
                let tenths = bytes / 100_000_000 + u64::from(bytes % 100_000_000 >= 50_000_000);
                format!("{}.{}", tenths / 10, tenths % 10)
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Unit::Kb => "KB",
            Unit::Mb => "MB",
            Unit::Gb => "GB",
        }
    }
}

/// The dim line of a running pill: "From PC · 2.1 of 5.0 GB · 1 min left".
/// The unit comes from the total, or from `done` if the total is unknown.
pub fn format_sub(from: Peer, done: u64, total: Option<u64>, eta: Option<Duration>) -> String {
    let amount = match total {
        Some(total) => {
            let unit = Unit::for_bytes(total);
            format!(
                "{} of {} {}",
                unit.amount(done),
                unit.amount(total),
                unit.label()
            )
        }
        None => {
            let unit = Unit::for_bytes(done);
            format!("{} {}", unit.amount(done), unit.label())
        }
    };
    match eta {
        Some(eta) => format!("From {from} · {amount} · {}", eta_text(eta)),
        None => format!("From {from} · {amount}"),
    }
}

/// "a few seconds left", "15 s left", "4 min left" or "1 h 2 min left",
/// always rounded up.
fn eta_text(eta: Duration) -> String {
    const SECOND: u128 = 1_000_000_000;
    let nanos = eta.as_nanos();
    if eta < Duration::from_secs(5) {
        return "a few seconds left".to_string();
    }
    if eta < Duration::from_secs(60) {
        return format!("{} s left", nanos.div_ceil(5 * SECOND) * 5);
    }
    let min = nanos.div_ceil(60 * SECOND);
    if eta < Duration::from_secs(3_600) {
        format!("{min} min left")
    } else {
        format!("{} h {} min left", min / 60, min % 60)
    }
}

/// The dim line of a failed pill, in plain words.
pub fn fail_text(reason: &FailReason, from: Peer, local: Peer) -> String {
    match reason {
        FailReason::Disconnected => format!("Copy failed — {from} disconnected"),
        FailReason::Sleep => format!("Copy failed — {from} went to sleep"),
        FailReason::NoSpace => format!("Copy failed — not enough space on this {local}"),
        FailReason::Unreadable { name } => {
            format!("Copy failed — couldn't read “{name}” on the {from}")
        }
        FailReason::Unwritable => "Copy failed — couldn't save here".to_string(),
        FailReason::Changed { name } => format!("Copy failed — “{name}” changed while copying"),
        FailReason::Cancelled => "Cancelled".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn file(name: &str, size: u64) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::File,
            size: Some(size),
        }
    }

    fn dir(name: &str) -> DragItem {
        DragItem {
            name: name.into(),
            kind: ItemKind::Dir,
            size: None,
        }
    }

    fn names(views: &[PillView]) -> Vec<&str> {
        views.iter().map(|v| v.name.as_str()).collect()
    }

    #[test]
    fn sub_text_examples() {
        assert_eq!(
            format_sub(
                Peer::Pc,
                2_100_000_000,
                Some(5_000_000_000),
                Some(Duration::from_secs(60))
            ),
            "From PC · 2.1 of 5.0 GB · 1 min left"
        );
        assert_eq!(
            format_sub(Peer::Mac, 512_000_000, Some(800_000_000), None),
            "From Mac · 512 of 800 MB"
        );
        // KB round up, MB round to nearest, GB to one decimal.
        assert_eq!(
            format_sub(Peer::Pc, 11_001, Some(36_200), None),
            "From PC · 12 of 37 KB"
        );
        assert_eq!(
            format_sub(Peer::Pc, 499_999, Some(1_500_000), None),
            "From PC · 0 of 2 MB"
        );
        assert_eq!(
            format_sub(Peer::Pc, 1_049_999_999, Some(1_050_000_000), None),
            "From PC · 1.0 of 1.1 GB"
        );
        // Unknown total: the unit comes from `done`.
        assert_eq!(
            format_sub(Peer::Mac, 36_200, None, None),
            "From Mac · 37 KB"
        );
        assert_eq!(
            format_sub(Peer::Mac, 3_400_000, None, None),
            "From Mac · 3 MB"
        );
        assert_eq!(format_sub(Peer::Pc, 0, None, None), "From PC · 0 KB");
    }

    #[test]
    fn eta_text_buckets() {
        let s = Duration::from_secs;
        for (eta, want) in [
            (s(0), "a few seconds left"),
            (ms(4_999), "a few seconds left"),
            (s(5), "5 s left"),
            (ms(5_001), "10 s left"),
            (s(12), "15 s left"),
            (s(55), "55 s left"),
            (ms(59_999), "60 s left"),
            (s(60), "1 min left"),
            (s(61), "2 min left"),
            (s(59 * 60), "59 min left"),
            (s(3_600), "1 h 0 min left"),
            (s(3_690), "1 h 2 min left"),
            (s(3 * 3_600 + 59 * 60), "3 h 59 min left"),
        ] {
            assert_eq!(eta_text(eta), want, "eta {eta:?}");
        }
        assert_eq!(
            format_sub(Peer::Mac, 1_000, Some(2_000), Some(s(12))),
            "From Mac · 1 of 2 KB · 15 s left"
        );
    }

    #[test]
    fn failed_pill_texts() {
        let name = || "Holiday.mov".to_string();
        for (reason, want) in [
            (FailReason::Disconnected, "Copy failed — Mac disconnected"),
            (FailReason::Sleep, "Copy failed — Mac went to sleep"),
            (
                FailReason::NoSpace,
                "Copy failed — not enough space on this PC",
            ),
            (
                FailReason::Unreadable { name: name() },
                "Copy failed — couldn't read “Holiday.mov” on the Mac",
            ),
            (FailReason::Unwritable, "Copy failed — couldn't save here"),
            (
                FailReason::Changed { name: name() },
                "Copy failed — “Holiday.mov” changed while copying",
            ),
        ] {
            assert_eq!(fail_text(&reason, Peer::Mac, Peer::Pc), want);

            let t0 = Instant::now();
            let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
            board.start(7, &[file("Holiday.mov", 9_000_000)], t0);
            board.progress(7, 3_000_000, Some(9_000_000), t0 + ms(800));
            board.finish(7, Err(reason), t0 + ms(900));
            let views = board.views(t0 + ms(1_000));
            assert_eq!(views.len(), 1);
            let v = &views[0];
            assert_eq!(v.sub, want);
            assert!(v.failed);
            assert_eq!(v.fraction, None);
            assert_eq!(v.right, "");
            assert_eq!(v.check_scale, 0.0);
            assert_eq!(v.opacity, 1.0);
        }
        assert_eq!(
            fail_text(&FailReason::NoSpace, Peer::Pc, Peer::Mac),
            "Copy failed — not enough space on this Mac"
        );
        assert_eq!(
            fail_text(&FailReason::Sleep, Peer::Pc, Peer::Mac),
            "Copy failed — PC went to sleep"
        );
    }

    #[test]
    fn tiny_copy_never_shows() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("a.txt", 4_000)], t0);
        board.progress(1, 4_000, Some(4_000), t0 + ms(300));
        board.finish(1, Ok(()), t0 + ms(400));
        assert!(board.views(t0 + ms(400)).is_empty());
        assert!(board.views(t0 + ms(500)).is_empty());
        assert!(board.views(t0 + ms(900)).is_empty());
        assert_eq!(board.next_frame(t0 + ms(400)), None);

        // A tiny cancel is not shown either, but a tiny failure is.
        board.start(2, &[file("b.txt", 4_000)], t0);
        board.finish(2, Err(FailReason::Cancelled), t0 + ms(100));
        assert!(board.views(t0 + ms(500)).is_empty());
        board.start(3, &[file("c.txt", 4_000)], t0);
        board.finish(3, Err(FailReason::Disconnected), t0 + ms(100));
        let views = board.views(t0 + ms(100));
        assert_eq!(names(&views), ["c.txt"]);
        assert!(views[0].failed);
    }

    #[test]
    fn slow_copy_shows_after_500ms() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
        board.start(1, &[file("clip.mp4", 10_000_000)], t0);
        assert!(board.views(t0).is_empty());
        assert_eq!(board.next_frame(t0 + ms(100)), Some(t0 + SHOW_AFTER));
        board.progress(1, 4_200_000, Some(10_000_000), t0 + ms(300));
        assert!(board.views(t0 + ms(499)).is_empty());

        let views = board.views(t0 + ms(500));
        assert_eq!(
            views,
            [PillView {
                drag: 1,
                icon: PillIcon::Video,
                name: "clip.mp4".into(),
                right: "42%".into(),
                sub: "From Mac · 4 of 10 MB".into(),
                fraction: Some(0.42),
                sheen_phase: 0.3125,
                check_scale: 0.0,
                opacity: 1.0,
                failed: false,
            }]
        );
        // The sheen animates while running.
        assert_eq!(board.next_frame(t0 + ms(500)), Some(t0 + ms(500) + FRAME));
        assert_eq!(board.views(t0 + ms(2_100))[0].sheen_phase, 0.3125);

        // The percentage is floored and never reads 100 % before done.
        board.progress(1, 9_999_999, Some(10_000_000), t0 + ms(600));
        assert_eq!(board.views(t0 + ms(600))[0].right, "99%");
        board.progress(1, 10_000_000, Some(10_000_000), t0 + ms(700));
        assert_eq!(board.views(t0 + ms(700))[0].right, "99%");
    }

    #[test]
    fn done_pops_check_holds_then_fades() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("big.iso", 10_000_000)], t0);
        board.progress(1, 6_000_000, Some(10_000_000), t0 + ms(700));
        let at = t0 + ms(1_000);
        board.finish(1, Ok(()), at);

        let v = &board.views(at)[0];
        assert_eq!(v.check_scale, 0.6);
        assert_eq!(v.right, "");
        assert_eq!(v.sub, "From PC · 10 MB");
        assert_eq!(v.fraction, Some(1.0));
        assert_eq!(v.opacity, 1.0);
        assert_eq!(board.next_frame(at), Some(at + FRAME));
        assert_eq!(board.next_frame(at + ms(140)), Some(at + CHECK_POP));

        let v = &board.views(at + ms(75))[0];
        assert!((v.check_scale - 0.8).abs() < 1e-6, "{}", v.check_scale);
        assert_eq!(board.views(at + ms(150))[0].check_scale, 1.0);
        // Nothing moves during the hold.
        assert_eq!(board.next_frame(at + ms(150)), Some(at + DONE_HOLD));
        let v = &board.views(at + ms(1_000))[0];
        assert_eq!((v.check_scale, v.opacity), (1.0, 1.0));

        let v = &board.views(at + ms(1_150))[0];
        assert!((v.opacity - 0.5).abs() < 1e-6, "{}", v.opacity);
        assert_eq!(board.next_frame(at + ms(1_290)), Some(at + ms(1_300)));
        let v = &board.views(at + ms(1_299))[0];
        assert!(v.opacity < 0.01, "{}", v.opacity);
        assert!(board.views(at + ms(1_300)).is_empty());
        assert_eq!(board.next_frame(at + ms(1_300)), None);

        // Cancelled reads "Cancelled", has no ✓ or bar, and fades the same way.
        board.start(2, &[file("x.zip", 10_000_000)], at + ms(2_000));
        let at = at + ms(2_600);
        board.finish(2, Err(FailReason::Cancelled), at);
        let v = &board.views(at)[0];
        assert_eq!(v.sub, "Cancelled");
        assert_eq!((v.check_scale, v.fraction, v.failed), (0.0, None, false));
        assert_eq!(board.next_frame(at), Some(at + DONE_HOLD));
        assert_eq!(board.views(at + ms(1_000))[0].opacity, 1.0);
        assert!(board.views(at + ms(1_300)).is_empty());
    }

    #[test]
    fn failed_persists_until_dismissed() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
        board.start(1, &[file("a.mov", 10_000_000)], t0);
        board.start(2, &[file("b.mov", 10_000_000)], t0);
        board.finish(1, Err(FailReason::Sleep), t0 + ms(2_000));
        board.dismiss(2); // Not failed: left alone.
        let later = t0 + Duration::from_secs(3_600);
        let views = board.views(later);
        assert_eq!(names(&views), ["a.mov", "b.mov"]);
        assert_eq!(views[0].sub, "Copy failed — Mac went to sleep");
        assert_eq!(views[0].opacity, 1.0);
        // A failed pill is static; only the running one animates.
        board.finish(2, Err(FailReason::Disconnected), later);
        assert_eq!(board.next_frame(later), None);
        // A late finish or progress doesn't change a failed pill.
        board.finish(1, Ok(()), later);
        board.progress(1, 1, Some(2), later);
        assert!(board.views(later)[0].failed);

        board.dismiss(1);
        assert_eq!(names(&board.views(later)), ["b.mov"]);
        board.dismiss(2);
        assert!(board.views(later).is_empty());
    }

    #[test]
    fn concurrent_drags_stack_in_start_order() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(10, &[file("a.mov", 1_000_000)], t0);
        board.start(5, &[file("b.png", 1_000_000)], t0 + ms(100));
        let pair = [file("c.txt", 1), file("d.txt", 1)];
        board.start(7, &pair, t0 + ms(200));
        // Starting again doesn't move or reset a pill.
        board.start(10, &[file("other", 1)], t0 + ms(300));
        assert_eq!(
            names(&board.views(t0 + ms(1_000))),
            ["a.mov", "b.png", "2 files"]
        );
        // Only the ones past SHOW_AFTER are shown yet.
        assert_eq!(names(&board.views(t0 + ms(650))), ["a.mov", "b.png"]);

        board.finish(10, Ok(()), t0 + ms(1_000));
        assert_eq!(
            names(&board.views(t0 + ms(2_000))),
            ["a.mov", "b.png", "2 files"]
        );
        assert_eq!(names(&board.views(t0 + ms(2_300))), ["b.png", "2 files"]);
        board.start(11, &[file("e.txt", 1_000_000)], t0 + ms(2_400));
        assert_eq!(
            names(&board.views(t0 + ms(3_000))),
            ["b.png", "2 files", "e.txt"]
        );
    }

    #[test]
    fn held_board_shows_nothing_and_expires_nothing() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
        board.start(1, &[file("a.mov", 10_000_000)], t0);
        board.start(2, &[file("b.mov", 10_000_000)], t0);
        board.start(3, &[file("c.mov", 10_000_000)], t0);
        board.finish(3, Ok(()), t0 + ms(1_000));
        assert_eq!(board.views(t0 + ms(1_500)).len(), 3);

        board.set_held(true, t0 + ms(1_500));
        assert!(board.views(t0 + ms(1_500)).is_empty());
        assert_eq!(board.next_frame(t0 + ms(1_500)), None);
        board.progress(1, 5_000_000, Some(10_000_000), t0 + ms(2_000));
        board.finish(1, Ok(()), t0 + ms(3_000));
        board.finish(2, Err(FailReason::Unwritable), t0 + ms(3_000));
        let long = t0 + Duration::from_secs(600);
        // Mutating calls while held don't expire anything either.
        board.start(4, &[file("tiny", 1)], long);
        board.finish(4, Ok(()), long + ms(10));
        assert!(board.views(long).is_empty());
        assert_eq!(board.next_frame(long), None);

        // On release, every finished pill replays its ✓, hold and fade from
        // the release; the failure is still there.
        let back = long + ms(20);
        board.set_held(false, back);
        let views = board.views(back);
        assert_eq!(names(&views), ["a.mov", "b.mov", "c.mov"]);
        assert_eq!(views[0].check_scale, 0.6);
        assert_eq!(views[0].sub, "From Mac · 10 MB");
        assert!(views[1].failed);
        assert_eq!(views[2].check_scale, 0.6);
        assert_eq!(board.next_frame(back), Some(back + FRAME));
        assert_eq!(board.views(back + ms(1_000))[0].opacity, 1.0);
        assert_eq!(names(&board.views(back + ms(1_300))), ["b.mov"]);
    }

    #[test]
    fn a_cancel_while_held_replays_from_the_release() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("a.mov", 10_000_000)], t0);
        board.set_held(true, t0 + ms(600));
        board.finish(1, Err(FailReason::Cancelled), t0 + ms(1_000));
        let back = t0 + ms(5_000);
        board.set_held(false, back);
        // Looking later doesn't move the timeline: it runs from the release.
        assert_eq!(board.next_frame(back + ms(500)), Some(back + DONE_HOLD));
        assert_eq!(board.views(back + ms(1_000))[0].sub, "Cancelled");
        assert!(board.views(back + ms(1_300)).is_empty());
    }

    #[test]
    fn board_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<PillBoard>();
    }

    #[test]
    fn a_faded_pill_never_replays_after_a_hold() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("a.mov", 10_000_000)], t0);
        let at = t0 + ms(1_000);
        board.finish(1, Ok(()), at);
        // Faded out, with no mutating call since to prune it.
        assert!(board.views(at + ms(1_300)).is_empty());
        board.set_held(true, at + ms(2_000));
        board.set_held(false, at + ms(3_000));
        assert!(board.views(at + ms(3_000)).is_empty());
        assert_eq!(board.next_frame(at + ms(3_000)), None);
        assert!(board.views(at + ms(3_500)).is_empty());
    }

    #[test]
    fn eta_comes_from_the_last_5_s() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("disk.img", 5_000_000_000)], t0);
        board.progress(1, 100_000_000, None, t0 + s(1));
        // No ETA before ETA_AFTER, nor with a single sample.
        assert_eq!(
            board.views(t0 + ms(1_500))[0].sub,
            "From PC · 0.1 of 5.0 GB"
        );
        board.progress(1, 200_000_000, Some(5_000_000_000), t0 + s(2));
        // 100 MB/s, 4.8 GB to go: 48 s.
        assert_eq!(
            board.views(t0 + s(2))[0].sub,
            "From PC · 0.2 of 5.0 GB · 50 s left"
        );
        // Then 25 MB/s. Only the last 5 s count: 4.6 GB at 25 MB/s is 184 s.
        for i in 1..=8 {
            board.progress(1, 200_000_000 + i * 25_000_000, None, t0 + s(2 + i));
        }
        assert_eq!(
            board.views(t0 + s(10))[0].sub,
            "From PC · 0.4 of 5.0 GB · 4 min left"
        );
        // Nothing heard for over 5 s: no rate, no ETA.
        assert_eq!(
            board.views(t0 + ms(15_001))[0].sub,
            "From PC · 0.4 of 5.0 GB"
        );
        // A stalled rate gives no ETA either.
        board.progress(1, 400_000_000, None, t0 + s(16));
        board.progress(1, 400_000_000, None, t0 + s(17));
        assert_eq!(board.views(t0 + s(17))[0].sub, "From PC · 0.4 of 5.0 GB");
    }

    #[test]
    fn unknown_total_has_no_bar_or_percent() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Mac, Peer::Pc);
        board.start(1, &[dir("Photos")], t0);
        board.progress(1, 3_400_000, None, t0 + ms(400));
        board.progress(1, 6_800_000, None, t0 + ms(2_400));
        let v = &board.views(t0 + ms(2_400))[0];
        assert_eq!(v.icon, PillIcon::Folder);
        assert_eq!(v.sub, "From Mac · 7 MB");
        assert_eq!((v.right.as_str(), v.fraction), ("", None));
        assert_eq!(board.next_frame(t0 + ms(2_400)), None);
        board.finish(1, Ok(()), t0 + ms(3_000));
        assert_eq!(board.views(t0 + ms(3_000))[0].sub, "From Mac · 7 MB");
    }

    #[test]
    fn done_past_the_total_raises_it() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("grew.log", 10_000_000)], t0);
        board.progress(1, 12_000_000, Some(10_000_000), t0 + ms(600));
        let v = &board.views(t0 + ms(600))[0];
        assert_eq!(v.sub, "From PC · 12 of 12 MB");
        assert_eq!((v.right.as_str(), v.fraction), ("99%", Some(1.0)));
        board.progress(1, 13_000_000, None, t0 + ms(700));
        assert_eq!(board.views(t0 + ms(700))[0].sub, "From PC · 13 of 13 MB");
        board.finish(1, Ok(()), t0 + ms(800));
        assert_eq!(board.views(t0 + ms(800))[0].sub, "From PC · 13 MB");
    }

    #[test]
    fn zero_total_has_no_bar_or_percent() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("empty.txt", 0)], t0);
        let v = &board.views(t0 + ms(600))[0];
        assert_eq!(v.sub, "From PC · 0 of 0 KB");
        assert_eq!((v.right.as_str(), v.fraction), ("", None));
        assert_eq!(board.next_frame(t0 + ms(600)), None);
    }

    #[test]
    fn progress_samples_coalesce() {
        let t0 = Instant::now();
        let mut board = PillBoard::new(Peer::Pc, Peer::Mac);
        board.start(1, &[file("disk.img", 5_000_000_000)], t0);
        // Every 10 ms for 6 s at 100 MB/s.
        for i in 1..=600u64 {
            board.progress(1, i * 1_000_000, None, t0 + ms(i * 10));
        }
        let n = board.pills[0].samples.len();
        assert!((50..=52).contains(&n), "{n} samples");
        assert_eq!(
            board.pills[0].samples.back(),
            Some(&(t0 + ms(6_000), 600_000_000))
        );
        // 4.4 GB at 100 MB/s: 44 s.
        assert_eq!(
            board.views(t0 + ms(6_000))[0].sub,
            "From PC · 0.6 of 5.0 GB · 45 s left"
        );
    }

    #[test]
    fn title_and_icon_rules() {
        assert_eq!(title_for(&[file("Report.pdf", 1)]), "Report.pdf");
        assert_eq!(title_for(&[file("", 1)]), "file");
        assert_eq!(title_for(&[]), "file");
        assert_eq!(title_for(&[dir("Photos")]), "Photos");
        assert_eq!(title_for(&[file("a", 1), file("b", 1)]), "2 files");
        assert_eq!(
            title_for(&[file("a", 1), dir("b"), file("c", 1)]),
            "3 items"
        );

        for (name, want) in [
            ("a.mp4", PillIcon::Video),
            ("a.MOV", PillIcon::Video),
            ("a.mkv", PillIcon::Video),
            ("a.avi", PillIcon::Video),
            ("a.webm", PillIcon::Video),
            ("a.jpg", PillIcon::Image),
            ("a.JPEG", PillIcon::Image),
            ("a.png", PillIcon::Image),
            ("a.gif", PillIcon::Image),
            ("a.heic", PillIcon::Image),
            ("a.webp", PillIcon::Image),
            ("a.bmp", PillIcon::Image),
            ("a.tiff", PillIcon::Image),
            ("a.tar.gz", PillIcon::Document),
            ("png", PillIcon::Document),
            (".png", PillIcon::Document),
            ("a.mp4.txt", PillIcon::Document),
            ("", PillIcon::Document),
        ] {
            assert_eq!(icon_for(&[file(name, 1)]), want, "name {name:?}");
        }
        assert_eq!(icon_for(&[dir("Movies.mov")]), PillIcon::Folder);
        assert_eq!(
            icon_for(&[file("a.mp4", 1), file("b.mp4", 1)]),
            PillIcon::Count(2)
        );
        assert_eq!(
            icon_for(&[dir("a"), dir("b"), file("c", 1)]),
            PillIcon::Count(3)
        );
    }
}
