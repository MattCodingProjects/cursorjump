//! THROWAWAY spike for file drag-and-drop on Windows (plan Task 1a). It proves the OLE
//! mechanics before the real build and is deleted or ignored afterwards.
//!
//! No input hooks, no `SendInput`, no cursor moving: only ordinary OLE drag-and-drop, a
//! `RegisterHotKey` hotkey and passive Raw Input (`RIDEV_INPUTSINK`).
//!
//! Every line is logged with a `+seconds` timestamp to the console and to a log file
//! under `%TEMP%\cursorjump-spike\` (the path is printed at start).
//!
//! # Catcher (A6): an edge drop target
//!
//! ```text
//! cargo run -p cursorjump --example dnd_spike_win -- catcher [--park] [--reject] [--helper] [--alpha N] [--quit-after SECS]
//! ```
//! A 2 px strip on the main monitor's right edge, full height, alpha 1/255, topmost,
//! never activated. Drag anything onto it. `DragEnter` logs every offered format and the
//! CF_HDROP paths or FILEGROUPDESCRIPTORW names/sizes/attributes.
//! - `--park`: on `DragEnter` the window grows to the whole virtual screen, so a release
//!   anywhere lands on it; it shrinks back after `Drop`/`DragLeave`.
//! - default (accept): `DragEnter`/`DragOver` answer DROPEFFECT_COPY, and `Drop` reads the
//!   first FILECONTENTS item (IStream or HGLOBAL) to a temp file and logs bytes read.
//! - `--reject`: `DragEnter`/`DragOver` answer DROPEFFECT_NONE (expect no `Drop`).
//! - `--helper`: call IDropTargetHelper so the source's drag image is drawn over us.
//! - `--alpha N`: window alpha 1..=255 (default 1; e.g. 128 to see the strip).
//!
//! # Source (A4, A5): a drag started by a hotkey with no button held
//!
//! ```text
//! cargo run -p cursorjump --example dnd_spike_win -- source [--format hdrop|virtual|virtual,hdrop|hdrop,virtual]
//!     [--wait-secs 60] [--size-mb 5] [--wait-when drop|first] [--hover-fail] [--image] [--fg]
//!     [--ri-same-thread] [--quit-after SECS]
//! ```
//! Press F8 (Fn+8 on a 60 % keyboard) with no mouse button held: `DoDragDrop` starts with
//! our own IDataObject/IDropSource. Move the mouse over a target and click (press and
//! release the left button) where you want to drop: `QueryContinueDrag` returns
//! DRAGDROP_S_DROP on the next Raw Input left-button up. Esc cancels.
//! - `--format`: the formats offered, in this order (plus "CursorJump.Self" last).
//!   `hdrop` = lazy CF_HDROP; `virtual` = FILEGROUPDESCRIPTORW + FILECONTENTS (IStream).
//! - `--wait-secs`: the simulated "file still arriving" wait (default 60). For hdrop it is
//!   spent in the first CF_HDROP GetData; for virtual, before the first stream Read (or in
//!   the FILECONTENTS GetData when the target asks for HGLOBAL).
//! - `--wait-when drop` (default): only GetData calls after QueryContinueDrag said DROP
//!   wait; hover-time calls answer at once. `first`: the very first one waits.
//! - `--hover-fail`: CF_HDROP GetData fails (DV_E_FORMATETC) before the drop, as if the
//!   file were not on disk yet, while still being offered by EnumFormatEtc.
//! - `--size-mb`: size of the generated text file (default 5; Gmail and Discord cap uploads).
//! - `--image`: attach a 96x96 drag image via IDragSourceHelper.
//! - `--fg`: show a small source window and SetForegroundWindow it before DoDragDrop
//!   (default off: A5 is "Windows thinks the button is up and we are in the background").
//! - `--ri-same-thread`: receive Raw Input on the DoDragDrop thread instead of its own
//!   thread (the real app has its own input thread, so the default matches it).
//!
//! # Self-test (no human)
//!
//! `-- selftest [--format virtual,hdrop] [--wait-secs 0] [--size-mb 5]` hands the source's
//! IDataObject straight to the catcher's readers in-process (no DoDragDrop), as a check
//! that the formats, the DROPFILES/descriptor layout and the IStream all work.

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() {
    win::main();
}

#[cfg(windows)]
mod win {
    use std::cell::{Cell, RefCell};
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::mem::{ManuallyDrop, size_of};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, COLOR_HIGHLIGHT, CreateDIBSection,
        DIB_RGB_COLORS, DeleteObject, GetStockObject, HBRUSH,
    };
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Ole::*;
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey, VK_F8};
    use windows::Win32::UI::Input::{
        GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RID_INPUT,
        RIDEV_INPUTSINK, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE, RegisterRawInputDevices,
    };
    use windows::Win32::UI::Shell::*;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{BOOL, HRESULT, PCWSTR, PWSTR, Ref, implement, w};

    // ---------------------------------------------------------------- logging

    static START: OnceLock<Instant> = OnceLock::new();
    static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

    macro_rules! log {
        ($($a:tt)*) => { log_line(&format!($($a)*)) };
    }

    fn log_line(s: &str) {
        let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
        let th = std::thread::current();
        let line = format!("[+{t:9.3}] [{}] {s}", th.name().unwrap_or("?"));
        println!("{line}");
        if let Some(f) = LOG_FILE.get()
            && let Ok(mut f) = f.lock()
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn spike_dir() -> PathBuf {
        let d = std::env::temp_dir().join("cursorjump-spike");
        let _ = std::fs::create_dir_all(&d);
        d
    }

    fn open_log(mode: &str) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = spike_dir().join(format!("{mode}-{secs}.log"));
        if let Ok(f) = File::create(&path) {
            let _ = LOG_FILE.set(Mutex::new(f));
        }
        log!("log file: {}", path.display());
    }

    // ---------------------------------------------------------------- args

    fn usage() -> ! {
        eprintln!(
            "usage:\n  dnd_spike_win catcher [--park] [--reject] [--helper] [--alpha N] [--quit-after SECS]\n  \
             dnd_spike_win source [--format hdrop|virtual|virtual,hdrop|hdrop,virtual] [--wait-secs 60] \
             [--size-mb 5] [--wait-when drop|first] [--hover-fail] [--image] [--fg] [--ri-same-thread] \
             [--quit-after SECS]
  dnd_spike_win selftest [--format ...] [--wait-secs 0] [--size-mb 5]"
        );
        std::process::exit(2)
    }

    #[derive(Default)]
    struct Args {
        mode: String,
        park: bool,
        reject: bool,
        helper: bool,
        alpha: u8,
        formats: Vec<Fmt>,
        wait_secs: u64,
        size_mb: u64,
        wait_on_first: bool,
        hover_fail: bool,
        image: bool,
        fg: bool,
        ri_same_thread: bool,
        quit_after: Option<u64>,
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Fmt {
        Hdrop,
        Virtual,
    }

    fn parse_args() -> Args {
        let mut it = std::env::args().skip(1);
        let mut a = Args {
            mode: it.next().unwrap_or_else(|| usage()),
            alpha: 1,
            formats: vec![Fmt::Hdrop],
            wait_secs: 60,
            size_mb: 5,
            ..Default::default()
        };
        let num = |v: Option<String>| -> u64 {
            v.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())
        };
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--park" => a.park = true,
                "--reject" => a.reject = true,
                "--helper" => a.helper = true,
                "--alpha" => a.alpha = num(it.next()).clamp(1, 255) as u8,
                "--format" => {
                    let v = it.next().unwrap_or_else(|| usage());
                    a.formats = v
                        .split(',')
                        .map(|f| match f {
                            "hdrop" => Fmt::Hdrop,
                            "virtual" => Fmt::Virtual,
                            _ => usage(),
                        })
                        .collect();
                }
                "--wait-secs" => a.wait_secs = num(it.next()),
                "--size-mb" => a.size_mb = num(it.next()),
                "--wait-when" => match it.next().as_deref() {
                    Some("drop") => a.wait_on_first = false,
                    Some("first") => a.wait_on_first = true,
                    _ => usage(),
                },
                "--hover-fail" => a.hover_fail = true,
                "--image" => a.image = true,
                "--fg" => a.fg = true,
                "--ri-same-thread" => a.ri_same_thread = true,
                "--quit-after" => a.quit_after = Some(num(it.next())),
                _ => usage(),
            }
        }
        a
    }

    pub fn main() {
        START.get_or_init(Instant::now);
        let args = parse_args();
        // SAFETY: plain process-wide setting, before any window exists.
        if let Err(e) =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
        {
            log!("SetProcessDpiAwarenessContext failed: {e}");
        }
        // SAFETY: initialises OLE (an STA) on this thread for its whole life.
        if let Err(e) = unsafe { OleInitialize(None) } {
            log!("OleInitialize failed: {e}");
            return;
        }
        match args.mode.as_str() {
            "catcher" => {
                open_log("catcher");
                run_catcher(&args);
            }
            "source" => {
                open_log("source");
                run_source(&args);
            }
            "selftest" => {
                open_log("selftest");
                run_selftest(&args);
            }
            _ => usage(),
        }
        // SAFETY: pairs the OleInitialize above.
        unsafe { OleUninitialize() };
        log!("exit");
    }

    // ---------------------------------------------------------------- formats

    struct Cfs {
        descriptor: u16,
        contents: u16,
        self_fmt: u16,
    }

    fn cfs() -> &'static Cfs {
        static C: OnceLock<Cfs> = OnceLock::new();
        C.get_or_init(|| {
            // SAFETY: registering a name has no preconditions.
            let reg = |n: PCWSTR| unsafe { RegisterClipboardFormatW(n) } as u16;
            Cfs {
                descriptor: reg(CFSTR_FILEDESCRIPTORW),
                contents: reg(CFSTR_FILECONTENTS),
                self_fmt: reg(w!("CursorJump.Self")),
            }
        })
    }

    fn format_name(cf: u16) -> String {
        const STD: [&str; 18] = [
            "?",
            "CF_TEXT",
            "CF_BITMAP",
            "CF_METAFILEPICT",
            "CF_SYLK",
            "CF_DIF",
            "CF_TIFF",
            "CF_OEMTEXT",
            "CF_DIB",
            "CF_PALETTE",
            "CF_PENDATA",
            "CF_RIFF",
            "CF_WAVE",
            "CF_UNICODETEXT",
            "CF_ENHMETAFILE",
            "CF_HDROP",
            "CF_LOCALE",
            "CF_DIBV5",
        ];
        if (cf as usize) < STD.len() && cf != 0 {
            return STD[cf as usize].to_string();
        }
        let mut buf = [0u16; 256];
        // SAFETY: buf is a valid writable slice.
        let n = unsafe { GetClipboardFormatNameW(cf as u32, &mut buf) };
        if n > 0 {
            String::from_utf16_lossy(&buf[..n as usize])
        } else {
            format!("#{cf}")
        }
    }

    fn tymed_name(t: u32) -> String {
        let names = [
            (TYMED_HGLOBAL, "HGLOBAL"),
            (TYMED_FILE, "FILE"),
            (TYMED_ISTREAM, "ISTREAM"),
            (TYMED_ISTORAGE, "ISTORAGE"),
            (TYMED_GDI, "GDI"),
            (TYMED_MFPICT, "MFPICT"),
            (TYMED_ENHMF, "ENHMF"),
        ];
        let v: Vec<&str> = names
            .iter()
            .filter(|(f, _)| t & f.0 as u32 != 0)
            .map(|(_, n)| *n)
            .collect();
        if v.is_empty() {
            format!("{t:#x}")
        } else {
            v.join("|")
        }
    }

    fn fmt_desc(f: &FORMATETC) -> String {
        format!(
            "{} (cf {}) tymed={} lindex={} aspect={}",
            format_name(f.cfFormat),
            f.cfFormat,
            tymed_name(f.tymed),
            f.lindex,
            f.dwAspect
        )
    }

    fn formatetc(cf: u16, tymed: TYMED, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: tymed.0 as u32,
        }
    }

    fn effect_name(e: DROPEFFECT) -> String {
        if e.0 == 0 {
            return "NONE".into();
        }
        let mut v = vec![];
        for (f, n) in [
            (DROPEFFECT_COPY, "COPY"),
            (DROPEFFECT_MOVE, "MOVE"),
            (DROPEFFECT_LINK, "LINK"),
        ] {
            if e.0 & f.0 != 0 {
                v.push(n);
            }
        }
        if e.0 & DROPEFFECT_SCROLL.0 != 0 {
            v.push("SCROLL");
        }
        v.join("|")
    }

    fn hr_name(hr: HRESULT) -> String {
        let n = match hr {
            DRAGDROP_S_DROP => "DRAGDROP_S_DROP",
            DRAGDROP_S_CANCEL => "DRAGDROP_S_CANCEL",
            S_OK => "S_OK",
            E_OUTOFMEMORY => "E_OUTOFMEMORY",
            E_NOTIMPL => "E_NOTIMPL",
            E_FAIL => "E_FAIL",
            DV_E_FORMATETC => "DV_E_FORMATETC",
            DV_E_TYMED => "DV_E_TYMED",
            DV_E_LINDEX => "DV_E_LINDEX",
            OLE_E_ADVISENOTSUPPORTED => "OLE_E_ADVISENOTSUPPORTED",
            RPC_E_CALL_REJECTED => "RPC_E_CALL_REJECTED",
            RPC_E_SERVERCALL_RETRYLATER => "RPC_E_SERVERCALL_RETRYLATER",
            _ => "",
        };
        format!("{n} ({:#010x})", hr.0 as u32)
    }

    // ---------------------------------------------------------------- HGLOBAL helpers

    fn hglobal_from(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
        // SAFETY: a fresh movable block, locked only for the copy.
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
            let p = GlobalLock(h);
            if p.is_null() {
                let e = windows::core::Error::from_thread();
                let _ = GlobalFree(Some(h));
                return Err(e);
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(h);
            Ok(h)
        }
    }

    fn hglobal_bytes(h: HGLOBAL) -> Vec<u8> {
        // SAFETY: the block is locked for the copy and its size comes from GlobalSize.
        unsafe {
            let n = GlobalSize(h);
            let p = GlobalLock(h);
            if p.is_null() {
                return vec![];
            }
            let v = std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec();
            let _ = GlobalUnlock(h);
            v
        }
    }

    fn hglobal_medium(h: HGLOBAL) -> STGMEDIUM {
        STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        }
    }

    fn medium_from_bytes(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
        Ok(hglobal_medium(hglobal_from(bytes)?))
    }

    fn as_bytes<T>(v: &T) -> &[u8] {
        // SAFETY: T is a plain repr(C) Win32 struct; any byte view of it is valid.
        unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    // ================================================================ SOURCE

    /// Set by QueryContinueDrag when it says DROP; GetData before it is "hover" time.
    static DROPPED: AtomicBool = AtomicBool::new(false);
    /// The simulated "file still arriving" wait has been spent (once per drag).
    static WAITED: AtomicBool = AtomicBool::new(false);
    static DRAGGING: AtomicBool = AtomicBool::new(false);
    static RI_UP: AtomicBool = AtomicBool::new(false);
    static RI_UP_MS: AtomicU64 = AtomicU64::new(0);
    static RI_ESC: AtomicBool = AtomicBool::new(false);
    static MAIN_TID: AtomicU32 = AtomicU32::new(0);

    fn now_ms() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_millis() as u64
    }

    fn phase() -> &'static str {
        if DROPPED.load(Ordering::SeqCst) {
            "after-drop"
        } else {
            "hover"
        }
    }

    struct SourceCfg {
        formats: Vec<Fmt>,
        wait_secs: u64,
        wait_on_first: bool,
        hover_fail: bool,
        file: PathBuf,
        file_name: String,
        size: u64,
    }

    impl SourceCfg {
        fn offers(&self, f: Fmt) -> bool {
            self.formats.contains(&f)
        }

        /// Spend the simulated wait if this call is the one that should.
        fn maybe_wait(&self, what: &str) {
            let due = self.wait_on_first || DROPPED.load(Ordering::SeqCst);
            if due && self.wait_secs > 0 && !WAITED.swap(true, Ordering::SeqCst) {
                log!(
                    "WAIT start: {what} sleeping {} s (simulating the file still arriving)",
                    self.wait_secs
                );
                std::thread::sleep(Duration::from_secs(self.wait_secs));
                log!("WAIT end: {what} slept {} s, answering now", self.wait_secs);
            }
        }
    }

    fn ensure_test_file(size_mb: u64) -> PathBuf {
        let path = spike_dir().join(format!("cj-spike-{size_mb}MB.txt"));
        let want = size_mb * 1024 * 1024;
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == want) {
            return path;
        }
        let mut f = File::create(&path).expect("create test file");
        let mut written = 0u64;
        let mut line_no = 0u64;
        let mut buf = String::with_capacity(1 << 20);
        while written < want {
            buf.clear();
            while buf.len() < (1 << 20) {
                line_no += 1;
                buf.push_str(&format!(
                    "CursorJump drag-and-drop spike test file, line {line_no:010}\n"
                ));
            }
            let take = (want - written).min(buf.len() as u64) as usize;
            f.write_all(&buf.as_bytes()[..take])
                .expect("write test file");
            written += take as u64;
        }
        path
    }

    #[implement(IDataObject)]
    struct DataObject {
        cfg: SourceCfg,
        offered: Vec<FORMATETC>,
        stored: RefCell<Vec<(u16, Vec<u8>)>>,
    }

    impl DataObject {
        fn new(cfg: SourceCfg) -> Self {
            let c = cfs();
            let mut offered = vec![];
            for f in &cfg.formats {
                match f {
                    Fmt::Hdrop => offered.push(formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)),
                    Fmt::Virtual => {
                        offered.push(formatetc(c.descriptor, TYMED_HGLOBAL, -1));
                        offered.push(formatetc(
                            c.contents,
                            TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0),
                            0,
                        ));
                    }
                }
            }
            offered.push(formatetc(c.self_fmt, TYMED_HGLOBAL, -1));
            DataObject {
                cfg,
                offered,
                stored: RefCell::new(vec![]),
            }
        }

        fn dropfiles(&self) -> Vec<u8> {
            let header = DROPFILES {
                pFiles: size_of::<DROPFILES>() as u32,
                pt: POINT::default(),
                fNC: false.into(),
                fWide: true.into(),
            };
            let mut v = as_bytes(&header).to_vec();
            for u in wide(&self.cfg.file.to_string_lossy())
                .into_iter()
                .chain([0, 0])
            {
                v.extend_from_slice(&u.to_le_bytes());
            }
            v
        }

        fn descriptor(&self) -> Vec<u8> {
            let mut fd = FILEDESCRIPTORW {
                dwFlags: (FD_FILESIZE.0 | FD_ATTRIBUTES.0 | FD_PROGRESSUI.0) as u32,
                dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
                nFileSizeHigh: (self.cfg.size >> 32) as u32,
                nFileSizeLow: self.cfg.size as u32,
                ..Default::default()
            };
            for (i, u) in wide(&self.cfg.file_name).into_iter().take(259).enumerate() {
                fd.cFileName[i] = u;
            }
            let g = FILEGROUPDESCRIPTORW {
                cItems: 1,
                fgd: [fd],
            };
            as_bytes(&g).to_vec()
        }

        fn get_data(&self, f: &FORMATETC) -> windows::core::Result<STGMEDIUM> {
            let c = cfs();
            let hglobal_ok = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
            if f.cfFormat == CF_HDROP.0 && self.cfg.offers(Fmt::Hdrop) {
                if !hglobal_ok {
                    return Err(DV_E_TYMED.into());
                }
                if self.cfg.hover_fail && !DROPPED.load(Ordering::SeqCst) {
                    log!("  CF_HDROP before drop: failing on purpose (--hover-fail)");
                    return Err(DV_E_FORMATETC.into());
                }
                self.cfg.maybe_wait("CF_HDROP GetData");
                return medium_from_bytes(&self.dropfiles());
            }
            if f.cfFormat == c.descriptor && self.cfg.offers(Fmt::Virtual) {
                if !hglobal_ok {
                    return Err(DV_E_TYMED.into());
                }
                return medium_from_bytes(&self.descriptor());
            }
            if f.cfFormat == c.contents && self.cfg.offers(Fmt::Virtual) {
                if f.lindex > 0 {
                    return Err(DV_E_LINDEX.into());
                }
                if f.tymed & TYMED_ISTREAM.0 as u32 != 0 {
                    let file = File::open(&self.cfg.file).map_err(|_| E_FAIL)?;
                    let stream: IStream = FileStream {
                        file: RefCell::new(file),
                        size: self.cfg.size,
                        name: self.cfg.file_name.clone(),
                        wait_secs: self.cfg.wait_secs,
                        started: Cell::new(false),
                        read_total: Cell::new(0),
                        next_report: Cell::new(self.cfg.size.div_ceil(4).max(1)),
                    }
                    .into();
                    log!("  FILECONTENTS: handing out an IStream");
                    return Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 {
                            pstm: ManuallyDrop::new(Some(stream)),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }
                if hglobal_ok {
                    self.cfg.maybe_wait("FILECONTENTS HGLOBAL GetData");
                    let bytes = std::fs::read(&self.cfg.file).map_err(|_| E_FAIL)?;
                    log!("  FILECONTENTS: HGLOBAL of {} bytes", bytes.len());
                    return medium_from_bytes(&bytes);
                }
                return Err(DV_E_TYMED.into());
            }
            if f.cfFormat == c.self_fmt {
                return medium_from_bytes(&std::process::id().to_le_bytes());
            }
            if let Some((_, b)) = self
                .stored
                .borrow()
                .iter()
                .find(|(cf, _)| *cf == f.cfFormat)
            {
                return medium_from_bytes(b);
            }
            Err(DV_E_FORMATETC.into())
        }

        fn has(&self, f: &FORMATETC) -> bool {
            self.offered
                .iter()
                .any(|o| o.cfFormat == f.cfFormat && o.tymed & f.tymed != 0)
                || self.stored.borrow().iter().any(|(cf, _)| *cf == f.cfFormat)
        }
    }

    impl IDataObject_Impl for DataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            // SAFETY: OLE passes a valid FORMATETC.
            let f = unsafe { &*pformatetcin };
            let t = Instant::now();
            log!("GetData [{}] {}", phase(), fmt_desc(f));
            let r = self.get_data(f);
            match &r {
                Ok(m) => log!(
                    "GetData -> ok ({}) {} in {} ms",
                    format_name(f.cfFormat),
                    tymed_name(m.tymed),
                    t.elapsed().as_millis()
                ),
                Err(e) => log!(
                    "GetData -> {} ({})",
                    hr_name(e.code()),
                    format_name(f.cfFormat)
                ),
            }
            r
        }

        fn GetDataHere(
            &self,
            pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            // SAFETY: OLE passes a valid FORMATETC.
            log!(
                "GetDataHere {} -> E_NOTIMPL",
                fmt_desc(unsafe { &*pformatetc })
            );
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            // SAFETY: OLE passes a valid FORMATETC.
            let f = unsafe { &*pformatetc };
            let ok = self.has(f);
            log!(
                "QueryGetData [{}] {} -> {}",
                phase(),
                fmt_desc(f),
                if ok { "S_OK" } else { "DV_E_FORMATETC" }
            );
            if ok { S_OK } else { DV_E_FORMATETC }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            if !pformatetcout.is_null() {
                // SAFETY: OLE passes a writable FORMATETC.
                unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
            }
            E_NOTIMPL
        }

        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> windows::core::Result<()> {
            // SAFETY: OLE passes valid pointers.
            let (f, m) = unsafe { (&*pformatetc, &*pmedium) };
            if m.tymed != TYMED_HGLOBAL.0 as u32 {
                log!(
                    "SetData {} medium {} -> E_NOTIMPL",
                    fmt_desc(f),
                    tymed_name(m.tymed)
                );
                return Err(E_NOTIMPL.into());
            }
            // SAFETY: tymed says the union holds an HGLOBAL.
            let bytes = hglobal_bytes(unsafe { m.u.hGlobal });
            let extra = if bytes.len() >= 4 && bytes.len() <= 8 {
                format!(
                    " dword={}",
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                )
            } else {
                String::new()
            };
            log!(
                "SetData [{}] {} ({} bytes){extra}",
                phase(),
                format_name(f.cfFormat),
                bytes.len()
            );
            {
                let mut s = self.stored.borrow_mut();
                s.retain(|(cf, _)| *cf != f.cfFormat);
                s.push((f.cfFormat, bytes));
            }
            if frelease.as_bool() {
                // SAFETY: with fRelease we own the medium.
                unsafe { ReleaseStgMedium(pmedium.cast_mut()) };
            }
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                log!("EnumFormatEtc(SET) -> E_NOTIMPL");
                return Err(E_NOTIMPL.into());
            }
            let names: Vec<String> = self
                .offered
                .iter()
                .map(|f| format_name(f.cfFormat))
                .collect();
            log!("EnumFormatEtc [{}] -> {}", phase(), names.join(", "));
            // SAFETY: the slice outlives the call; the shell copies it.
            unsafe { SHCreateStdEnumFmtEtc(&self.offered) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Ref<IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    /// FILECONTENTS served from the generated file; the first Read spends the wait.
    #[implement(IStream)]
    struct FileStream {
        file: RefCell<File>,
        size: u64,
        name: String,
        wait_secs: u64,
        started: Cell<bool>,
        read_total: Cell<u64>,
        next_report: Cell<u64>,
    }

    impl ISequentialStream_Impl for FileStream_Impl {
        fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            if !self.started.replace(true) {
                log!("IStream first Read (cb={cb})");
                if self.wait_secs > 0 && !WAITED.swap(true, Ordering::SeqCst) {
                    log!(
                        "WAIT start: IStream first Read sleeping {} s (simulating the file still arriving)",
                        self.wait_secs
                    );
                    std::thread::sleep(Duration::from_secs(self.wait_secs));
                    log!(
                        "WAIT end: IStream slept {} s, serving bytes now",
                        self.wait_secs
                    );
                }
            }
            // SAFETY: the caller gives a buffer of cb bytes.
            let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
            let mut n = 0usize;
            let mut file = self.file.borrow_mut();
            while n < buf.len() {
                match file.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return E_FAIL,
                }
            }
            if !pcbread.is_null() {
                // SAFETY: optional out pointer, checked above.
                unsafe { *pcbread = n as u32 };
            }
            let total = self.read_total.get() + n as u64;
            self.read_total.set(total);
            // Report each quarter of the file and the end.
            if total >= self.next_report.get() || n == 0 {
                log!(
                    "IStream Read progress: {total}/{} bytes of {}",
                    self.size,
                    self.name
                );
                self.next_report.set(total + self.size.div_ceil(4).max(1));
            }
            S_OK
        }

        fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            STG_E_ACCESSDENIED
        }
    }

    impl IStream_Impl for FileStream_Impl {
        fn Seek(
            &self,
            dlibmove: i64,
            dworigin: STREAM_SEEK,
            plibnewposition: *mut u64,
        ) -> windows::core::Result<()> {
            let from = match dworigin {
                STREAM_SEEK_SET => SeekFrom::Start(dlibmove as u64),
                STREAM_SEEK_CUR => SeekFrom::Current(dlibmove),
                STREAM_SEEK_END => SeekFrom::End(dlibmove),
                _ => return Err(STG_E_INVALIDFUNCTION.into()),
            };
            let pos = self
                .file
                .borrow_mut()
                .seek(from)
                .map_err(|_| STG_E_INVALIDFUNCTION)?;
            if dlibmove != 0 || dworigin != STREAM_SEEK_CUR {
                log!("IStream Seek({dlibmove}, origin {}) -> {pos}", dworigin.0);
            }
            if !plibnewposition.is_null() {
                // SAFETY: optional out pointer, checked above.
                unsafe { *plibnewposition = pos };
            }
            Ok(())
        }

        fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn CopyTo(
            &self,
            pstm: Ref<IStream>,
            cb: u64,
            pcbread: *mut u64,
            pcbwritten: *mut u64,
        ) -> windows::core::Result<()> {
            log!("IStream CopyTo(cb={cb})");
            let dst = pstm.ok()?;
            let mut buf = vec![0u8; 1 << 20];
            let (mut read, mut written) = (0u64, 0u64);
            while read < cb {
                let want = (cb - read).min(buf.len() as u64) as u32;
                let mut got = 0u32;
                let hr =
                    ISequentialStream_Impl::Read(self, buf.as_mut_ptr().cast(), want, &mut got);
                hr.ok()?;
                if got == 0 {
                    break;
                }
                read += got as u64;
                let mut w = 0u32;
                // SAFETY: buf holds `got` valid bytes.
                unsafe { dst.Write(buf.as_ptr().cast(), got, Some(&mut w)) }.ok()?;
                written += w as u64;
            }
            // SAFETY: optional out pointers.
            unsafe {
                if !pcbread.is_null() {
                    *pcbread = read;
                }
                if !pcbwritten.is_null() {
                    *pcbwritten = written;
                }
            }
            Ok(())
        }

        fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
            Ok(())
        }

        fn Revert(&self) -> windows::core::Result<()> {
            Ok(())
        }

        fn LockRegion(&self, _o: u64, _cb: u64, _t: &LOCKTYPE) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn UnlockRegion(&self, _o: u64, _cb: u64, _t: u32) -> windows::core::Result<()> {
            Err(STG_E_INVALIDFUNCTION.into())
        }

        fn Stat(
            &self,
            pstatstg: *mut STATSTG,
            grfstatflag: &STATFLAG,
        ) -> windows::core::Result<()> {
            log!("IStream Stat(flag {})", grfstatflag.0);
            let mut name = PWSTR::null();
            if *grfstatflag != STATFLAG_NONAME {
                let w: Vec<u16> = wide(&self.name).into_iter().chain([0]).collect();
                // SAFETY: the caller frees pwcsName with CoTaskMemFree.
                unsafe {
                    let p = CoTaskMemAlloc(w.len() * 2).cast::<u16>();
                    if !p.is_null() {
                        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                        name = PWSTR(p);
                    }
                }
            }
            // SAFETY: the caller gives a writable STATSTG.
            unsafe {
                *pstatstg = STATSTG {
                    pwcsName: name,
                    r#type: STGTY_STREAM.0 as u32,
                    cbSize: self.size,
                    grfMode: STGM_READ,
                    ..Default::default()
                };
            }
            Ok(())
        }

        fn Clone(&self) -> windows::core::Result<IStream> {
            log!("IStream Clone -> E_NOTIMPL");
            Err(E_NOTIMPL.into())
        }
    }

    #[implement(IDropSource)]
    struct DropSource {
        last_keys: Cell<u32>,
        last_effect: Cell<u32>,
        saw_down: Cell<bool>,
        warned_keystate_release: Cell<bool>,
        calls: Cell<u64>,
        window_start: Cell<Instant>,
        window_calls: Cell<u64>,
    }

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(
            &self,
            fescapepressed: BOOL,
            grfkeystate: MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            self.calls.set(self.calls.get() + 1);
            self.window_calls.set(self.window_calls.get() + 1);
            if self.window_start.get().elapsed() >= Duration::from_secs(2) {
                log!(
                    "QueryContinueDrag alive: {} calls in the last {:.1} s ({} total)",
                    self.window_calls.get(),
                    self.window_start.get().elapsed().as_secs_f64(),
                    self.calls.get()
                );
                self.window_start.set(Instant::now());
                self.window_calls.set(0);
            }
            let keys = grfkeystate.0;
            if keys != self.last_keys.get() {
                log!(
                    "QueryContinueDrag keystate {:#x} -> {:#x} (MK_LBUTTON {})",
                    self.last_keys.get(),
                    keys,
                    keys & MK_LBUTTON.0 != 0
                );
                self.last_keys.set(keys);
            }
            if keys & MK_LBUTTON.0 != 0 {
                self.saw_down.set(true);
            }
            if fescapepressed.as_bool() {
                log!("QueryContinueDrag: fEscapePressed -> DRAGDROP_S_CANCEL");
                return DRAGDROP_S_CANCEL;
            }
            if RI_ESC.swap(false, Ordering::SeqCst) {
                log!(
                    "QueryContinueDrag: Raw Input Esc (fEscapePressed was FALSE) -> DRAGDROP_S_CANCEL"
                );
                return DRAGDROP_S_CANCEL;
            }
            if RI_UP.swap(false, Ordering::SeqCst) {
                let lag = now_ms().saturating_sub(RI_UP_MS.load(Ordering::SeqCst));
                log!(
                    "QueryContinueDrag: Raw Input left-button up seen {lag} ms ago -> DRAGDROP_S_DROP"
                );
                DROPPED.store(true, Ordering::SeqCst);
                return DRAGDROP_S_DROP;
            }
            if self.saw_down.get()
                && keys & MK_LBUTTON.0 == 0
                && !self.warned_keystate_release.replace(true)
            {
                log!(
                    "QueryContinueDrag: keystate shows the left button released but no Raw Input up yet"
                );
            }
            S_OK
        }

        fn GiveFeedback(&self, dweffect: DROPEFFECT) -> HRESULT {
            if dweffect.0 != self.last_effect.get() {
                log!("GiveFeedback effect -> {}", effect_name(dweffect));
                self.last_effect.set(dweffect.0);
            }
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    fn set_drag_image(data: &IDataObject) {
        const N: i32 = 96;
        // SAFETY: standard IDragSourceHelper setup; on success the helper owns the bitmap.
        unsafe {
            let helper: IDragSourceHelper =
                match CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) {
                    Ok(h) => h,
                    Err(e) => {
                        log!("IDragSourceHelper create failed: {e}");
                        return;
                    }
                };
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: N,
                    biHeight: -N,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let hbmp = match CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(b) => b,
                Err(e) => {
                    log!("CreateDIBSection failed: {e}");
                    return;
                }
            };
            // Premultiplied BGRA: a translucent blue square.
            let px = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (N * N) as usize);
            let a = 0xC0u32;
            let b = 0xE0 * a / 255;
            let g = 0x60 * a / 255;
            let r = 0x20 * a / 255;
            px.fill((a << 24) | (r << 16) | (g << 8) | b);
            let shdi = SHDRAGIMAGE {
                sizeDragImage: SIZE { cx: N, cy: N },
                ptOffset: POINT { x: N / 2, y: N / 2 },
                hbmpDragImage: hbmp,
                crColorKey: COLORREF(0xFFFF_FFFF),
            };
            match helper.InitializeFromBitmap(&shdi, data) {
                Ok(()) => log!("drag image attached via IDragSourceHelper"),
                Err(e) => {
                    log!("InitializeFromBitmap failed: {e}");
                    let _ = DeleteObject(hbmp.into());
                }
            }
        }
    }

    fn run_drag(args: &Args, file: &Path, fg: Option<HWND>) {
        if DRAGGING.swap(true, Ordering::SeqCst) {
            return;
        }
        DROPPED.store(false, Ordering::SeqCst);
        WAITED.store(false, Ordering::SeqCst);
        RI_UP.store(false, Ordering::SeqCst);
        RI_ESC.store(false, Ordering::SeqCst);
        let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        let cfg = SourceCfg {
            formats: args.formats.clone(),
            wait_secs: args.wait_secs,
            wait_on_first: args.wait_on_first,
            hover_fail: args.hover_fail,
            file: file.to_path_buf(),
            file_name: file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size,
        };
        let data: IDataObject = DataObject::new(cfg).into();
        let src: IDropSource = DropSource {
            last_keys: Cell::new(0),
            last_effect: Cell::new(u32::MAX),
            saw_down: Cell::new(false),
            warned_keystate_release: Cell::new(false),
            calls: Cell::new(0),
            window_start: Cell::new(Instant::now()),
            window_calls: Cell::new(0),
        }
        .into();
        if args.image {
            set_drag_image(&data);
        }
        if let Some(h) = fg {
            // SAFETY: plain window call on our own window.
            let ok = unsafe { SetForegroundWindow(h) }.as_bool();
            log!("SetForegroundWindow(source window) -> {ok}");
        }
        let mut pt = POINT::default();
        // SAFETY: reads the cursor position only (no moving).
        let _ = unsafe { GetCursorPos(&mut pt) };
        log!(
            "F8: DoDragDrop START formats={:?} file={} ({size} bytes) cursor=({}, {}) allowed=COPY",
            args.formats,
            file.display(),
            pt.x,
            pt.y
        );
        let t = Instant::now();
        let mut effect = DROPEFFECT_NONE;
        // SAFETY: both objects are live COM objects on this STA thread.
        let hr = unsafe { DoDragDrop(&data, &src, DROPEFFECT_COPY, &mut effect) };
        log!(
            "DoDragDrop RETURNED {} effect={} after {:.1} s",
            hr_name(hr),
            effect_name(effect),
            t.elapsed().as_secs_f64()
        );
        DRAGGING.store(false, Ordering::SeqCst);
    }

    /// No-human check: hand our IDataObject straight to the catcher's readers
    /// (no DoDragDrop, no window), as if it had been dropped.
    fn run_selftest(args: &Args) {
        let file = ensure_test_file(args.size_mb);
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        DROPPED.store(false, Ordering::SeqCst);
        WAITED.store(false, Ordering::SeqCst);
        let cfg = SourceCfg {
            formats: args.formats.clone(),
            wait_secs: args.wait_secs,
            wait_on_first: args.wait_on_first,
            hover_fail: args.hover_fail,
            file: file.clone(),
            file_name: file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size,
        };
        let data: IDataObject = DataObject::new(cfg).into();
        log!("selftest: hover phase");
        dump_formats(&data);
        extract_files(&data);
        DROPPED.store(true, Ordering::SeqCst);
        log!("selftest: drop phase");
        extract_files(&data);
        read_first_contents(&data);
    }

    fn run_source(args: &Args) {
        let file = ensure_test_file(args.size_mb);
        // SAFETY: thread id of the current thread.
        MAIN_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        log!(
            "source: formats {:?}, wait {} s ({}), hover-fail {}, file {}",
            args.formats,
            args.wait_secs,
            if args.wait_on_first {
                "on the first GetData/Read"
            } else {
                "only after DROP"
            },
            args.hover_fail,
            file.display()
        );
        if args.ri_same_thread {
            match raw_input_window() {
                Ok(_) => log!("Raw Input on the DoDragDrop thread"),
                Err(e) => log!("Raw Input setup failed: {e}"),
            }
        } else {
            std::thread::Builder::new()
                .name("rawinput".into())
                .spawn(|| match raw_input_window() {
                    Ok(_) => {
                        log!("Raw Input on its own thread");
                        message_loop_simple();
                    }
                    Err(e) => log!("Raw Input setup failed: {e}"),
                })
                .expect("spawn raw input thread");
        }
        let fg = if args.fg {
            match source_window() {
                Ok(h) => Some(h),
                Err(e) => {
                    log!("source window failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        // SAFETY: a thread hotkey; WM_HOTKEY arrives in this thread's queue.
        if let Err(e) = unsafe { RegisterHotKey(None, 1, MOD_NOREPEAT, VK_F8.0 as u32) } {
            log!("RegisterHotKey(F8) failed: {e}");
            return;
        }
        let quit_timer = quit_timer(args.quit_after);
        log!(
            "ready: press F8 with no mouse button held, then click where you want to drop (Esc cancels)"
        );
        let mut msg = MSG::default();
        // SAFETY: standard message loop.
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.hwnd.is_invalid() && msg.message == WM_HOTKEY {
                run_drag(args, &file, fg);
                continue;
            }
            if quit_timer.is_some_and(|id| msg.message == WM_TIMER && msg.wParam.0 == id) {
                log!("--quit-after elapsed");
                break;
            }
            // SAFETY: standard message loop.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// A thread timer for `--quit-after`; its WM_TIMER carries the returned id.
    fn quit_timer(secs: Option<u64>) -> Option<usize> {
        // SAFETY: a thread timer; WM_TIMER arrives in this thread's queue.
        secs.map(|s| unsafe { SetTimer(None, 0, (s * 1000) as u32, None) })
    }

    fn message_loop_simple() {
        let mut msg = MSG::default();
        // SAFETY: standard message loop.
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn register_class(
        name: PCWSTR,
        proc_: WNDPROC,
        brush: HBRUSH,
    ) -> windows::core::Result<HINSTANCE> {
        // SAFETY: GetModuleHandleW(None) returns this executable's handle.
        let hinst: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: proc_,
            hInstance: hinst,
            lpszClassName: name,
            hbrBackground: brush,
            // SAFETY: a stock system cursor.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            ..Default::default()
        };
        // SAFETY: wc is fully initialised.
        if unsafe { RegisterClassExW(&wc) } == 0 {
            let e = windows::core::Error::from_thread();
            if e.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                return Err(e);
            }
        }
        Ok(hinst)
    }

    fn raw_input_window() -> windows::core::Result<HWND> {
        let class = w!("CjSpikeRawInput");
        let hinst = register_class(class, Some(raw_input_proc), HBRUSH::default())?;
        // SAFETY: a hidden, never-shown WS_POPUP window.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("CursorJump spike raw input"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst),
                None,
            )
        }?;
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x02,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        // SAFETY: passive Raw Input registration (not a hook).
        unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) }?;
        Ok(hwnd)
    }

    unsafe extern "system" fn raw_input_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_INPUT {
            on_raw_input(HRAWINPUT(lparam.0 as _));
        }
        // DefWindowProcW also performs the required WM_INPUT cleanup.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn on_raw_input(handle: HRAWINPUT) {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        // SAFETY: `raw` is a RAWINPUT-sized buffer and `size` says so.
        let n = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some((&mut raw as *mut RAWINPUT).cast()),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if n == 0 || n == u32::MAX {
            return;
        }
        let dragging = DRAGGING.load(Ordering::SeqCst);
        if raw.header.dwType == RIM_TYPEMOUSE.0 {
            // SAFETY: dwType says the union holds mouse data.
            let flags = unsafe { raw.data.mouse.Anonymous.Anonymous.usButtonFlags } as u32;
            if flags & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
                log!("Raw Input: left button DOWN (dragging={dragging})");
            }
            if flags & RI_MOUSE_LEFT_BUTTON_UP != 0 {
                log!("Raw Input: left button UP (dragging={dragging})");
                if dragging {
                    RI_UP_MS.store(now_ms(), Ordering::SeqCst);
                    RI_UP.store(true, Ordering::SeqCst);
                    wake_main();
                }
            }
        } else if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            // SAFETY: dwType says the union holds keyboard data.
            let kb = unsafe { raw.data.keyboard };
            if kb.VKey == 0x1B && (kb.Flags as u32 & RI_KEY_BREAK) == 0 {
                log!("Raw Input: Esc down (dragging={dragging})");
                if dragging {
                    RI_ESC.store(true, Ordering::SeqCst);
                    wake_main();
                }
            }
        }
    }

    /// Nudge the DoDragDrop thread's queue so OLE gets a chance to call QueryContinueDrag.
    fn wake_main() {
        let tid = MAIN_TID.load(Ordering::SeqCst);
        if tid != 0 {
            // SAFETY: posts a no-op message to our own thread.
            let _ = unsafe { PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0)) };
        }
    }

    fn source_window() -> windows::core::Result<HWND> {
        let class = w!("CjSpikeSource");
        let brush = HBRUSH((COLOR_HIGHLIGHT.0 + 1) as isize as *mut _);
        let hinst = register_class(class, Some(plain_proc), brush)?;
        // SAFETY: a small visible tool window.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                class,
                w!("CursorJump spike source"),
                WS_POPUP | WS_VISIBLE,
                20,
                20,
                160,
                40,
                None,
                None,
                Some(hinst),
                None,
            )
        }?;
        Ok(hwnd)
    }

    unsafe extern "system" fn plain_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    // ================================================================ CATCHER

    const STRIP_W: i32 = 2;

    fn strip_rect() -> RECT {
        // SAFETY: plain metrics queries.
        let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
        RECT {
            left: cx - STRIP_W,
            top: 0,
            right: cx,
            bottom: cy,
        }
    }

    fn virtual_rect() -> RECT {
        // SAFETY: plain metrics queries.
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            RECT {
                left: x,
                top: y,
                right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        }
    }

    fn place(hwnd: HWND, r: RECT, what: &str) {
        // SAFETY: repositions our own window without activating it.
        let res = unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOACTIVATE,
            )
        };
        log!(
            "{what}: window -> ({}, {}) {}x{} {:?}",
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            res.err()
        );
    }

    #[implement(IDropTarget)]
    struct DropTarget {
        hwnd: HWND,
        park: bool,
        reject: bool,
        helper: Option<IDropTargetHelper>,
        entered: Cell<Option<Instant>>,
        last_effect: Cell<u32>,
        last_keys: Cell<u32>,
        overs: Cell<u64>,
        parked: Cell<bool>,
    }

    impl DropTarget {
        fn choose(&self, allowed: DROPEFFECT) -> DROPEFFECT {
            if self.reject {
                DROPEFFECT_NONE
            } else if allowed.0 & DROPEFFECT_COPY.0 != 0 {
                DROPEFFECT_COPY
            } else {
                // The source doesn't allow COPY; take whatever it allows.
                DROPEFFECT(allowed.0 & (DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0))
            }
        }

        fn unpark(&self, why: &str) {
            if self.parked.replace(false) {
                place(self.hwnd, strip_rect(), &format!("unpark ({why})"));
            }
        }
    }

    impl IDropTarget_Impl for DropTarget_Impl {
        fn DragEnter(
            &self,
            pdataobj: Ref<IDataObject>,
            grfkeystate: MODIFIERKEYS_FLAGS,
            pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            self.entered.set(Some(Instant::now()));
            self.overs.set(0);
            self.last_keys.set(grfkeystate.0);
            // SAFETY: OLE passes a valid in/out effect pointer.
            let allowed = unsafe { *pdweffect };
            log!(
                "=== DragEnter at ({}, {}) keys={:#x} source-allows={}",
                pt.x,
                pt.y,
                grfkeystate.0,
                effect_name(allowed)
            );
            if let Ok(data) = pdataobj.ok() {
                dump_formats(data);
                extract_files(data);
            }
            if self.park {
                self.parked.set(true);
                place(self.hwnd, virtual_rect(), "park on DragEnter");
            }
            let eff = self.choose(allowed);
            log!("DragEnter -> {}", effect_name(eff));
            self.last_effect.set(eff.0);
            // SAFETY: as above.
            unsafe { *pdweffect = eff };
            if let (Some(h), Ok(data)) = (&self.helper, pdataobj.ok()) {
                let p = POINT { x: pt.x, y: pt.y };
                // SAFETY: helper call with our window and the live data object.
                let _ = unsafe { h.DragEnter(self.hwnd, data, &p, eff) };
            }
            Ok(())
        }

        fn DragOver(
            &self,
            grfkeystate: MODIFIERKEYS_FLAGS,
            pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            self.overs.set(self.overs.get() + 1);
            // SAFETY: OLE passes a valid in/out effect pointer.
            let allowed = unsafe { *pdweffect };
            let eff = self.choose(allowed);
            if eff.0 != self.last_effect.get() || grfkeystate.0 != self.last_keys.get() {
                log!(
                    "DragOver #{} at ({}, {}) keys={:#x} allows={} -> {}",
                    self.overs.get(),
                    pt.x,
                    pt.y,
                    grfkeystate.0,
                    effect_name(allowed),
                    effect_name(eff)
                );
                self.last_effect.set(eff.0);
                self.last_keys.set(grfkeystate.0);
            }
            // SAFETY: as above.
            unsafe { *pdweffect = eff };
            if let Some(h) = &self.helper {
                let p = POINT { x: pt.x, y: pt.y };
                // SAFETY: helper call.
                let _ = unsafe { h.DragOver(&p, eff) };
            }
            Ok(())
        }

        fn DragLeave(&self) -> windows::core::Result<()> {
            let held = self
                .entered
                .take()
                .map(|t| t.elapsed().as_secs_f64())
                .unwrap_or(0.0);
            log!(
                "=== DragLeave (no Drop) after {held:.1} s, {} DragOvers",
                self.overs.get()
            );
            self.unpark("DragLeave");
            if let Some(h) = &self.helper {
                // SAFETY: helper call.
                let _ = unsafe { h.DragLeave() };
            }
            Ok(())
        }

        fn Drop(
            &self,
            pdataobj: Ref<IDataObject>,
            grfkeystate: MODIFIERKEYS_FLAGS,
            pt: &POINTL,
            pdweffect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            // SAFETY: OLE passes a valid in/out effect pointer.
            let allowed = unsafe { *pdweffect };
            let eff = self.choose(allowed);
            log!(
                "=== DROP CALLED at ({}, {}) keys={:#x} allows={} reject={} -> {}",
                pt.x,
                pt.y,
                grfkeystate.0,
                effect_name(allowed),
                self.reject,
                effect_name(eff)
            );
            self.entered.set(None);
            self.unpark("Drop");
            if let Some(h) = &self.helper
                && let Ok(data) = pdataobj.ok()
            {
                let p = POINT { x: pt.x, y: pt.y };
                // SAFETY: helper call.
                let _ = unsafe { h.Drop(data, &p, eff) };
            }
            if !self.reject
                && let Ok(data) = pdataobj.ok()
            {
                read_first_contents(data);
            }
            // SAFETY: as above.
            unsafe { *pdweffect = eff };
            log!("Drop returning {}", effect_name(eff));
            Ok(())
        }
    }

    fn dump_formats(data: &IDataObject) {
        // SAFETY: plain COM calls on a live data object.
        let en = match unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) } {
            Ok(e) => e,
            Err(e) => {
                log!("EnumFormatEtc failed: {e}");
                return;
            }
        };
        let mut i = 0;
        loop {
            let mut f = [FORMATETC::default()];
            let mut got = 0u32;
            // SAFETY: one-element buffer and count out pointer.
            let hr = unsafe { en.Next(&mut f, Some(&mut got)) };
            if hr != S_OK || got == 0 {
                break;
            }
            log!("  format #{i}: {}", fmt_desc(&f[0]));
            if f[0].cfFormat == cfs().self_fmt {
                log!("  ^ CursorJump.Self: this drag came from the spike source");
            }
            if !f[0].ptd.is_null() {
                // SAFETY: the enumerator allocated ptd with CoTaskMemAlloc.
                unsafe { CoTaskMemFree(Some(f[0].ptd as *const _)) };
            }
            i += 1;
        }
        log!("  ({i} formats offered)");
    }

    fn extract_files(data: &IDataObject) {
        let c = cfs();
        // CF_HDROP paths.
        let f = formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1);
        // SAFETY: plain COM calls; the medium is released below.
        if unsafe { data.QueryGetData(&f) } == S_OK {
            let t = Instant::now();
            match unsafe { data.GetData(&f) } {
                Ok(mut m) => {
                    // SAFETY: CF_HDROP comes as an HGLOBAL holding DROPFILES.
                    unsafe {
                        let h = HDROP(m.u.hGlobal.0);
                        let n = DragQueryFileW(h, u32::MAX, None);
                        log!(
                            "  CF_HDROP: {n} path(s) (GetData took {} ms)",
                            t.elapsed().as_millis()
                        );
                        for i in 0..n.min(20) {
                            let mut buf = [0u16; 1024];
                            let len = DragQueryFileW(h, i, Some(&mut buf)) as usize;
                            let p = String::from_utf16_lossy(&buf[..len]);
                            let size = std::fs::metadata(&p).map(|m| m.len()).ok();
                            log!("    [{i}] {p} (size {size:?})");
                        }
                        ReleaseStgMedium(&mut m);
                    }
                }
                Err(e) => log!("  CF_HDROP GetData failed: {}", hr_name(e.code())),
            }
        }
        // FILEGROUPDESCRIPTORW names/sizes/attributes.
        let f = formatetc(c.descriptor, TYMED_HGLOBAL, -1);
        if unsafe { data.QueryGetData(&f) } == S_OK {
            match unsafe { data.GetData(&f) } {
                Ok(mut m) => {
                    // SAFETY: bounds come from GlobalSize; layout is FILEGROUPDESCRIPTORW.
                    let bytes = hglobal_bytes(unsafe { m.u.hGlobal });
                    unsafe { ReleaseStgMedium(&mut m) };
                    log_descriptors(&bytes);
                }
                Err(e) => log!(
                    "  FILEGROUPDESCRIPTORW GetData failed: {}",
                    hr_name(e.code())
                ),
            }
        }
    }

    fn descriptors(bytes: &[u8]) -> Vec<FILEDESCRIPTORW> {
        if bytes.len() < 4 {
            return vec![];
        }
        let n = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let sz = size_of::<FILEDESCRIPTORW>();
        (0..n)
            .take_while(|i| 4 + (i + 1) * sz <= bytes.len())
            .map(|i| {
                // SAFETY: bounds checked above; read_unaligned copes with the Vec's alignment.
                unsafe {
                    std::ptr::read_unaligned(
                        bytes.as_ptr().add(4 + i * sz).cast::<FILEDESCRIPTORW>(),
                    )
                }
            })
            .collect()
    }

    fn fd_name(fd: &FILEDESCRIPTORW) -> String {
        // FILEDESCRIPTORW is packed: copy fields out before borrowing them.
        let name = fd.cFileName;
        let len = name.iter().position(|&c| c == 0).unwrap_or(260);
        String::from_utf16_lossy(&name[..len])
    }

    fn log_descriptors(bytes: &[u8]) {
        let fds = descriptors(bytes);
        log!("  FILEGROUPDESCRIPTORW: {} item(s)", fds.len());
        for (i, fd) in fds.iter().enumerate().take(20) {
            let (flags, attrs) = (fd.dwFlags, fd.dwFileAttributes);
            let size = if flags & FD_FILESIZE.0 as u32 != 0 {
                let (hi, lo) = (fd.nFileSizeHigh, fd.nFileSizeLow);
                format!("{}", ((hi as u64) << 32) | lo as u64)
            } else {
                "unknown".into()
            };
            let attrs = if flags & FD_ATTRIBUTES.0 as u32 != 0 {
                format!("{attrs:#x}")
            } else {
                "n/a".into()
            };
            log!(
                "    [{i}] {:?} size={size} attrs={attrs} flags={flags:#x}",
                fd_name(fd)
            );
        }
    }

    fn read_first_contents(data: &IDataObject) {
        let c = cfs();
        let mut name = "item0".to_string();
        let f = formatetc(c.descriptor, TYMED_HGLOBAL, -1);
        // SAFETY: plain COM calls; media released below.
        if let Ok(mut m) = unsafe { data.GetData(&f) } {
            let bytes = hglobal_bytes(unsafe { m.u.hGlobal });
            unsafe { ReleaseStgMedium(&mut m) };
            if let Some(fd) = descriptors(&bytes).first() {
                name = fd_name(fd);
            }
        }
        let f = formatetc(c.contents, TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0), 0);
        if unsafe { data.QueryGetData(&f) } != S_OK {
            log!(
                "Drop: no FILECONTENTS offered (CF_HDROP paths above are what a real drop would copy)"
            );
            return;
        }
        let safe: String = name
            .chars()
            .map(|ch| if "\\/:*?\"<>|".contains(ch) { '_' } else { ch })
            .collect();
        let out_path = spike_dir().join(format!("caught-{safe}"));
        let t = Instant::now();
        log!("Drop: GetData(FILECONTENTS lindex 0) for {name:?}");
        let mut m = match unsafe { data.GetData(&f) } {
            Ok(m) => m,
            Err(e) => {
                log!("Drop: FILECONTENTS GetData failed: {}", hr_name(e.code()));
                return;
            }
        };
        log!(
            "Drop: FILECONTENTS came as {} after {} ms",
            tymed_name(m.tymed),
            t.elapsed().as_millis()
        );
        let mut out = match File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                log!("Drop: can't create {}: {e}", out_path.display());
                unsafe { ReleaseStgMedium(&mut m) };
                return;
            }
        };
        let mut total = 0u64;
        let mut first_byte_ms = None;
        if m.tymed == TYMED_ISTREAM.0 as u32 {
            // SAFETY: tymed says the union holds an IStream.
            let stm = unsafe { (*m.u.pstm).clone() };
            if let Some(stm) = stm {
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let mut got = 0u32;
                    // SAFETY: buf is writable for its length.
                    let hr = unsafe {
                        stm.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut got))
                    };
                    if hr.is_err() {
                        log!("Drop: IStream Read failed {}", hr_name(hr));
                        break;
                    }
                    if got == 0 {
                        break;
                    }
                    first_byte_ms.get_or_insert(t.elapsed().as_millis());
                    let _ = out.write_all(&buf[..got as usize]);
                    total += got as u64;
                }
            }
        } else if m.tymed == TYMED_HGLOBAL.0 as u32 {
            // SAFETY: tymed says the union holds an HGLOBAL.
            let bytes = hglobal_bytes(unsafe { m.u.hGlobal });
            first_byte_ms = Some(t.elapsed().as_millis());
            let _ = out.write_all(&bytes);
            total = bytes.len() as u64;
        }
        // SAFETY: we own the medium from GetData.
        unsafe { ReleaseStgMedium(&mut m) };
        let secs = t.elapsed().as_secs_f64();
        log!(
            "Drop: read {total} bytes of FILECONTENTS into {} (first byte after {:?} ms, total {secs:.2} s, {:.1} MB/s)",
            out_path.display(),
            first_byte_ms,
            total as f64 / 1_048_576.0 / secs.max(0.001)
        );
    }

    fn run_catcher(args: &Args) {
        let class = w!("CjSpikeCatcher");
        // SAFETY: a stock brush.
        let brush = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
        let hinst = match register_class(class, Some(plain_proc), brush) {
            Ok(h) => h,
            Err(e) => {
                log!("RegisterClassExW failed: {e}");
                return;
            }
        };
        let r = strip_rect();
        // SAFETY: our own layered topmost popup; never activated.
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!("CursorJump spike catcher"),
                WS_POPUP,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                None,
                None,
                Some(hinst),
                None,
            )
        } {
            Ok(h) => h,
            Err(e) => {
                log!("CreateWindowExW failed: {e}");
                return;
            }
        };
        // SAFETY: our own window.
        if let Err(e) =
            unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), args.alpha, LWA_ALPHA) }
        {
            log!("SetLayeredWindowAttributes failed: {e}");
        }
        let helper: Option<IDropTargetHelper> = if args.helper {
            // SAFETY: standard shell helper object.
            match unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) } {
                Ok(h) => Some(h),
                Err(e) => {
                    log!("IDropTargetHelper create failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        let target: IDropTarget = DropTarget {
            hwnd,
            park: args.park,
            reject: args.reject,
            helper,
            entered: Cell::new(None),
            last_effect: Cell::new(u32::MAX),
            last_keys: Cell::new(0),
            overs: Cell::new(0),
            parked: Cell::new(false),
        }
        .into();
        // SAFETY: OLE is initialised on this thread; the window is ours.
        if let Err(e) = unsafe { RegisterDragDrop(hwnd, &target) } {
            log!("RegisterDragDrop failed: {e}");
            return;
        }
        // SAFETY: shows our window without activating it.
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let v = virtual_rect();
        log!(
            "catcher ready: strip ({}, {}) {}x{} alpha {} park={} reject={} helper={}; virtual screen ({}, {}) {}x{}",
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            args.alpha,
            args.park,
            args.reject,
            args.helper,
            v.left,
            v.top,
            v.right - v.left,
            v.bottom - v.top
        );
        log!("drag something onto the right edge of the main monitor; Ctrl+C to stop");
        let quit_timer = quit_timer(args.quit_after);
        let mut msg = MSG::default();
        // SAFETY: standard message loop.
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if quit_timer.is_some_and(|id| msg.message == WM_TIMER && msg.wParam.0 == id) {
                log!("--quit-after elapsed");
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // SAFETY: undoes RegisterDragDrop on the same thread.
        let _ = unsafe { RevokeDragDrop(hwnd) };
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}
