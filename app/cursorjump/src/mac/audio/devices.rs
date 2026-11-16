//! The Mac's default input while connected, and a watch on its audio devices.

use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2_core_audio::{
    AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListenerBlock,
    AudioObjectSetPropertyData, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeBuiltIn, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
    kAudioObjectUnknown,
};
use objc2_core_foundation::{CFRetained, CFString};
use tracing::{debug, info, warn};

/// "CursorJump Microphone": the PC's mic, as an input every Mac app can pick.
pub const MIC_UID: &str = "com.cursorjump.audio.mic";
/// "CursorJump Mic Feed": the hidden output the app plays the PC's mic into.
pub const FEED_UID: &str = "com.cursorjump.audio.feed";

/// The Mac's input devices: Core Audio in the app, a fake in tests.
pub trait InputDevices {
    fn default_input_uid(&self) -> Option<String>;
    fn set_default_input(&mut self, uid: &str) -> Result<()>;
    fn exists(&self, uid: &str) -> bool;
    /// The built-in mic, where the Mac goes when nothing better is known.
    fn builtin_input_uid(&self) -> Option<String>;
}

/// Makes our mic the Mac's default input while connected, and always gives
/// the user's own input back.
///
/// The input it replaced is saved in the restore file before the switch, so
/// a crash leaves it on disk for [`MicRoute::recover`] at the next launch.
/// Whenever the Mac would otherwise stay on our mic with nothing better
/// known, it goes to the built-in mic: our mic is silent once disconnected.
pub struct MicRoute<D: InputDevices> {
    devices: D,
    restore_file: PathBuf,
    engaged: bool,
}

impl<D: InputDevices> MicRoute<D> {
    /// `restore_file` is `<app support>/audio-restore.toml`.
    pub fn new(devices: D, restore_file: PathBuf) -> Self {
        MicRoute {
            devices,
            restore_file,
            engaged: false,
        }
    }

    /// At launch: a leftover restore file means the app died while engaged.
    /// Sets its input back (or moves off our mic) and deletes it.
    pub fn recover(&mut self) {
        self.restore("recover");
    }

    /// Saves the current default input's UID, unless it is ours or one is
    /// saved already, then makes our mic the default. Never switches without
    /// knowing what to restore: with our mic installed there is always a
    /// default input, so not reading one is an error.
    pub fn engage(&mut self) -> Result<()> {
        let current = self
            .devices
            .default_input_uid()
            .context("reading the default input")?;
        if current == MIC_UID {
            info!("audio engage: {MIC_UID} is already the default input; nothing to save");
            self.engaged = true;
            return Ok(());
        }
        let mut wrote = false;
        if self.saved().is_none() {
            write_restore_file(&self.restore_file, &current)?;
            wrote = true;
        }
        if let Err(e) = self.devices.set_default_input(MIC_UID) {
            if wrote {
                self.delete_restore_file();
            }
            return Err(e.context(format!("making {MIC_UID} the default input")));
        }
        info!("audio engage: default input {current} -> {MIC_UID}");
        self.engaged = true;
        Ok(())
    }

    /// Sets the saved input back if it still exists, else leaves macOS's
    /// choice (moving off our mic if that is still the default), and
    /// deletes the restore file. A failed switch keeps the file for the
    /// next launch.
    pub fn release(&mut self) {
        self.restore("release");
    }

    /// While engaged: if the default input is no longer our mic, sets it back.
    pub fn reassert(&mut self) {
        if !self.engaged {
            return;
        }
        let current = self.devices.default_input_uid();
        if current.as_deref() == Some(MIC_UID) {
            return;
        }
        info!(
            "audio reassert: default input moved to {}; setting {MIC_UID} back",
            current.as_deref().unwrap_or("<none>")
        );
        if let Err(e) = self.devices.set_default_input(MIC_UID) {
            warn!("audio reassert: {e:#}");
        }
    }

    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// Release and recover: one rule, so the Mac never stays on our mic.
    fn restore(&mut self, why: &str) {
        self.engaged = false;
        let saved = self.saved();
        let current = self.devices.default_input_uid();
        let target = match &saved {
            Some(uid) if self.devices.exists(uid) => Some(uid.clone()),
            _ if current.as_deref() == Some(MIC_UID) => {
                let builtin = self.devices.builtin_input_uid();
                if builtin.is_none() {
                    warn!("audio {why}: no built-in mic to move off {MIC_UID} to");
                }
                builtin
            }
            _ => None,
        };
        match target {
            Some(uid) => {
                if let Err(e) = self.devices.set_default_input(&uid) {
                    warn!("audio {why}: setting the default input back to {uid}: {e:#}");
                    self.move_off_our_mic(why, &uid);
                    return; // keep the file: the next launch retries
                }
                info!(
                    "audio {why}: default input {} -> {uid} (saved {})",
                    current.as_deref().unwrap_or("<none>"),
                    saved.as_deref().unwrap_or("<none>")
                );
            }
            None if saved.is_some() => info!(
                "audio {why}: saved input {} is gone; leaving macOS's choice {}",
                saved.as_deref().unwrap_or_default(),
                current.as_deref().unwrap_or("<none>")
            ),
            None => debug!("audio {why}: nothing to restore"),
        }
        self.delete_restore_file();
    }

    /// After a failed restore to `failed`: if the default is still our mic,
    /// tries the built-in mic, so the Mac doesn't stay on a silent input.
    fn move_off_our_mic(&mut self, why: &str, failed: &str) {
        if self.devices.default_input_uid().as_deref() != Some(MIC_UID) {
            return;
        }
        let Some(builtin) = self.devices.builtin_input_uid().filter(|b| b != failed) else {
            warn!("audio {why}: no other mic to move off {MIC_UID} to");
            return;
        };
        match self.devices.set_default_input(&builtin) {
            Ok(()) => info!("audio {why}: default input {MIC_UID} -> {builtin} (for now)"),
            Err(e) => warn!("audio {why}: setting the default input to {builtin}: {e:#}"),
        }
    }

    /// The saved input, if the restore file holds one.
    fn saved(&self) -> Option<String> {
        let text = match std::fs::read_to_string(&self.restore_file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                warn!("audio: reading {}: {e}", self.restore_file.display());
                return None;
            }
        };
        let uid = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|t| t.get("previous_input")?.as_str().map(str::to_owned));
        if uid.is_none() {
            warn!(
                "audio: {} holds no previous_input",
                self.restore_file.display()
            );
        }
        uid
    }

    fn delete_restore_file(&self) {
        match std::fs::remove_file(&self.restore_file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("audio: deleting {}: {e}", self.restore_file.display()),
        }
    }
}

/// Writes `previous_input = "<uid>"` atomically: a temporary file in the
/// same directory, then a rename (as `Config::save` does).
fn write_restore_file(path: &Path, uid: &str) -> Result<()> {
    let text = format!("previous_input = {}\n", toml::Value::String(uid.into()));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let written = std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// The Mac's real input devices.
pub struct CoreAudioDevices;

impl InputDevices for CoreAudioDevices {
    fn default_input_uid(&self) -> Option<String> {
        let id: AudioObjectID = get(
            SYSTEM,
            global(kAudioHardwarePropertyDefaultInputDevice),
            None,
        )
        .ok()?;
        (id != kAudioObjectUnknown)
            .then(|| device_uid(id))
            .flatten()
    }

    fn set_default_input(&mut self, uid: &str) -> Result<()> {
        let id = device_id(uid).with_context(|| format!("no audio device {uid}"))?;
        let st = set(SYSTEM, global(kAudioHardwarePropertyDefaultInputDevice), id);
        anyhow::ensure!(st == 0, "setting the default input to {uid}: OSStatus {st}");
        Ok(())
    }

    fn exists(&self, uid: &str) -> bool {
        device_id(uid).is_some()
    }

    fn builtin_input_uid(&self) -> Option<String> {
        device_ids()
            .into_iter()
            .find(|&id| {
                get::<u32>(id, global(kAudioDevicePropertyTransportType), None)
                    == Ok(kAudioDeviceTransportTypeBuiltIn)
                    && input_streams(id) > 0
            })
            .and_then(device_uid)
    }
}

/// Whether the CursorJump audio driver is installed: both of its devices exist.
pub fn driver_present() -> bool {
    device_id(MIC_UID).is_some() && device_id(FEED_UID).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioDevEv {
    DefaultInputChanged,
    DefaultOutputChanged,
    /// A device appeared or went away.
    DevicesChanged,
    /// Whether some app records from our mic.
    MicListening(bool),
}

type Listener = RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>;

/// A listener block that moves between threads. Sound: the closures inside
/// capture only `Send + Sync` data, and block copies and releases are
/// atomic.
struct SendBlock(Listener);
// SAFETY: see above.
unsafe impl Send for SendBlock {}

/// The system object's properties [`AudioWatch`] follows.
const SYSTEM_PROPERTIES: [AudioObjectPropertySelector; 3] = [
    kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices,
];

/// Core Audio notifications for the default devices, the device list and
/// whether some app records from our mic.
///
/// Every listener runs on a serial queue of its own, so `on` is called
/// there, one event at a time. Dropping the watch removes the listeners and
/// waits for a callback already running; don't drop it from inside `on`.
pub struct AudioWatch {
    shared: Arc<Shared>,
    system: SendBlock,
    added: Vec<AudioObjectPropertySelector>,
}

struct Shared {
    on: Box<dyn Fn(AudioDevEv) + Send + Sync>,
    queue: DispatchRetained<DispatchQueue>,
    mic: Mutex<MicListener>,
}

/// The `IsRunningSomewhere` listener on our mic, while it exists.
#[derive(Default)]
struct MicListener {
    id: AudioObjectID,
    block: Option<SendBlock>,
    last: Option<bool>,
}

impl AudioWatch {
    pub fn start(on: impl Fn(AudioDevEv) + Send + Sync + 'static) -> Result<AudioWatch> {
        let shared = Arc::new(Shared {
            on: Box::new(on),
            queue: DispatchQueue::new("com.cursorjump.audio.watch", DispatchQueueAttr::SERIAL),
            mic: Mutex::default(),
        });
        let s = Arc::clone(&shared);
        let system: Listener = RcBlock::new(
            move |n: u32, addresses: NonNull<AudioObjectPropertyAddress>| {
                // SAFETY: Core Audio passes `n` addresses.
                let addresses =
                    unsafe { std::slice::from_raw_parts(addresses.as_ptr(), n as usize) };
                for a in addresses {
                    debug!("audio watch: system property {:#x}", a.mSelector);
                    let ev = match a.mSelector {
                        sel if sel == kAudioHardwarePropertyDefaultInputDevice => {
                            AudioDevEv::DefaultInputChanged
                        }
                        sel if sel == kAudioHardwarePropertyDefaultOutputDevice => {
                            AudioDevEv::DefaultOutputChanged
                        }
                        sel if sel == kAudioHardwarePropertyDevices => {
                            s.follow_mic();
                            AudioDevEv::DevicesChanged
                        }
                        _ => continue,
                    };
                    (s.on)(ev);
                }
            },
        );
        let mut watch = AudioWatch {
            shared,
            system: SendBlock(system),
            added: Vec::new(),
        };
        for selector in SYSTEM_PROPERTIES {
            let st = add_listener(SYSTEM, selector, &watch.shared.queue, &watch.system.0);
            // On failure, Drop removes the ones already added.
            anyhow::ensure!(
                st == 0,
                "listening to audio property {selector:#x}: OSStatus {st}"
            );
            watch.added.push(selector);
        }
        let s = &watch.shared;
        s.queue.exec_sync(|| s.follow_mic());
        Ok(watch)
    }
}

impl Drop for AudioWatch {
    fn drop(&mut self) {
        let s = &self.shared;
        for &selector in &self.added {
            let st = remove_listener(SYSTEM, selector, &s.queue, &self.system.0);
            if st != 0 {
                warn!("audio watch: removing listener {selector:#x}: OSStatus {st}");
            }
        }
        // No system callback runs after this, so the mic listener stays put.
        s.queue.exec_sync(|| {});
        let mut mic = s.mic.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(block) = mic.block.take() {
            remove_listener(
                mic.id,
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                &s.queue,
                &block.0,
            );
        }
        drop(mic);
        s.queue.exec_sync(|| {});
    }
}

impl Shared {
    fn mic(&self) -> std::sync::MutexGuard<'_, MicListener> {
        self.mic.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Moves the mic listener to our mic's current object, if that changed.
    /// Runs on the queue.
    fn follow_mic(self: &Arc<Self>) {
        let id = device_id(MIC_UID).unwrap_or(kAudioObjectUnknown);
        let mut mic = self.mic();
        if id == mic.id {
            return;
        }
        debug!("audio watch: our mic is object {id} (was {})", mic.id);
        if let Some(block) = mic.block.take() {
            // The old object may be gone already; nothing to undo then.
            remove_listener(
                mic.id,
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                &self.queue,
                &block.0,
            );
        }
        let was = mic.last.take();
        // Stored only once its listener is added, so a failed registration
        // is retried on the next DevicesChanged.
        mic.id = kAudioObjectUnknown;
        if id != kAudioObjectUnknown {
            let weak = Arc::downgrade(self);
            let block: Listener =
                RcBlock::new(move |_: u32, _: NonNull<AudioObjectPropertyAddress>| {
                    if let Some(s) = weak.upgrade() {
                        s.mic_changed();
                    }
                });
            let st = add_listener(
                id,
                kAudioDevicePropertyDeviceIsRunningSomewhere,
                &self.queue,
                &block,
            );
            if st == 0 {
                mic.id = id;
                mic.block = Some(SendBlock(block));
                let running = running_somewhere(id);
                mic.last = Some(running);
                drop(mic);
                (self.on)(AudioDevEv::MicListening(running));
                return;
            }
            warn!("audio watch: listening to our mic {id}: OSStatus {st}");
        }
        drop(mic);
        if was == Some(true) {
            (self.on)(AudioDevEv::MicListening(false));
        }
    }

    /// Our mic's `IsRunningSomewhere` changed. Runs on the queue.
    fn mic_changed(&self) {
        let mut mic = self.mic();
        if mic.id == kAudioObjectUnknown {
            return;
        }
        let running = running_somewhere(mic.id);
        debug!("audio watch: our mic running somewhere: {running}");
        if mic.last == Some(running) {
            return;
        }
        mic.last = Some(running);
        drop(mic);
        (self.on)(AudioDevEv::MicListening(running));
    }
}

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

fn global(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a fixed-size property; Err is the OSStatus.
fn get<T: Copy>(
    obj: AudioObjectID,
    mut address: AudioObjectPropertyAddress,
    qualifier: Option<&[u8]>,
) -> Result<T, i32> {
    let mut out = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let (qsize, qptr) = qualifier.map_or((0, std::ptr::null()), |q| {
        (q.len() as u32, q.as_ptr().cast::<c_void>())
    });
    // SAFETY: every pointer outlives the call, and `size` is `out`'s size.
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&mut address),
            qsize,
            qptr,
            NonNull::from(&mut size),
            NonNull::from(&mut out).cast(),
        )
    };
    if st != 0 || size as usize != size_of::<T>() {
        return Err(st);
    }
    // SAFETY: Core Audio wrote all `size_of::<T>()` bytes.
    Ok(unsafe { out.assume_init() })
}

fn set<T: Copy>(obj: AudioObjectID, mut address: AudioObjectPropertyAddress, value: T) -> i32 {
    let mut v = value;
    // SAFETY: every pointer outlives the call, and the size is `v`'s.
    unsafe {
        AudioObjectSetPropertyData(
            obj,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            size_of::<T>() as u32,
            NonNull::from(&mut v).cast(),
        )
    }
}

fn property_size(obj: AudioObjectID, mut address: AudioObjectPropertyAddress) -> u32 {
    let mut size = 0u32;
    // SAFETY: every pointer outlives the call.
    let st = unsafe {
        AudioObjectGetPropertyDataSize(
            obj,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if st != 0 { 0 } else { size }
}

/// The device whose UID is `uid`, if it exists.
fn device_id(uid: &str) -> Option<AudioObjectID> {
    let cf = CFString::from_str(uid);
    let ptr: *const CFString = &*cf;
    // The qualifier is the CFStringRef itself.
    let qualifier = (ptr as usize).to_ne_bytes();
    let id: AudioObjectID = get(
        SYSTEM,
        global(kAudioHardwarePropertyTranslateUIDToDevice),
        Some(&qualifier),
    )
    .ok()?;
    (id != kAudioObjectUnknown).then_some(id)
}

fn device_uid(id: AudioObjectID) -> Option<String> {
    let p: *mut CFString = get(id, global(kAudioDevicePropertyDeviceUID), None).ok()?;
    // SAFETY: Core Audio returns the UID retained (+1); this takes that reference.
    let s = unsafe { CFRetained::from_raw(NonNull::new(p)?) };
    Some(s.to_string())
}

fn device_ids() -> Vec<AudioObjectID> {
    let mut address = global(kAudioHardwarePropertyDevices);
    let mut size = property_size(SYSTEM, address);
    let mut ids = vec![kAudioObjectUnknown; size as usize / size_of::<AudioObjectID>()];
    if ids.is_empty() {
        return ids;
    }
    // SAFETY: every pointer outlives the call, and `size` is `ids`' size in bytes.
    let st = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(ids.as_mut_ptr()).unwrap().cast(),
        )
    };
    if st != 0 {
        return Vec::new();
    }
    // The list may have shrunk since the size was read.
    ids.truncate(size as usize / size_of::<AudioObjectID>());
    ids
}

fn input_streams(id: AudioObjectID) -> u32 {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    property_size(id, address) / size_of::<AudioObjectID>() as u32
}

fn running_somewhere(id: AudioObjectID) -> bool {
    get::<u32>(
        id,
        global(kAudioDevicePropertyDeviceIsRunningSomewhere),
        None,
    )
    .is_ok_and(|r| r != 0)
}

fn add_listener(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
    queue: &DispatchQueue,
    block: &Listener,
) -> i32 {
    let mut address = global(selector);
    // SAFETY: the block stays alive (in the watch) until it is removed.
    unsafe {
        AudioObjectAddPropertyListenerBlock(
            obj,
            NonNull::from(&mut address),
            Some(queue),
            RcBlock::as_ptr(block),
        )
    }
}

fn remove_listener(
    obj: AudioObjectID,
    selector: AudioObjectPropertySelector,
    queue: &DispatchQueue,
    block: &Listener,
) -> i32 {
    let mut address = global(selector);
    // SAFETY: the same object, address, queue and block it was added with.
    unsafe {
        AudioObjectRemovePropertyListenerBlock(
            obj,
            NonNull::from(&mut address),
            Some(queue),
            RcBlock::as_ptr(block),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    const BUILTIN: &str = "BuiltInMicrophoneDevice";
    const USB: &str = "usb-mic";

    #[derive(Default)]
    struct State {
        default: Option<String>,
        present: Vec<String>,
        sets: Vec<String>,
        /// Sets to these devices fail, as if coreaudiod refused them.
        refused: Vec<String>,
    }

    /// Fake devices; the test keeps a handle on the state.
    #[derive(Clone, Default)]
    struct Fake(Rc<RefCell<State>>);

    impl Fake {
        /// The built-in mic, our mic and a USB mic, with `default` the default.
        fn with_default(default: &str) -> Fake {
            let f = Fake::default();
            {
                let mut s = f.0.borrow_mut();
                s.present = vec![BUILTIN.into(), MIC_UID.into(), USB.into()];
                s.default = Some(default.into());
            }
            f
        }
        fn current(&self) -> Option<String> {
            self.0.borrow().default.clone()
        }
        fn sets(&self) -> Vec<String> {
            self.0.borrow().sets.clone()
        }
        /// macOS (or the user) picks `uid`, not through the route.
        fn pick(&self, uid: &str) {
            self.0.borrow_mut().default = Some(uid.into());
        }
        fn unplug(&self, uid: &str) {
            self.0.borrow_mut().present.retain(|d| d != uid);
        }
    }

    impl InputDevices for Fake {
        fn default_input_uid(&self) -> Option<String> {
            self.current()
        }
        fn set_default_input(&mut self, uid: &str) -> Result<()> {
            let mut s = self.0.borrow_mut();
            anyhow::ensure!(!s.refused.iter().any(|d| d == uid), "refused {uid}");
            anyhow::ensure!(s.present.iter().any(|d| d == uid), "no device {uid}");
            s.default = Some(uid.into());
            s.sets.push(uid.into());
            Ok(())
        }
        fn exists(&self, uid: &str) -> bool {
            self.0.borrow().present.iter().any(|d| d == uid)
        }
        fn builtin_input_uid(&self) -> Option<String> {
            self.exists(BUILTIN).then(|| BUILTIN.into())
        }
    }

    fn route(fake: &Fake, dir: &tempfile::TempDir) -> MicRoute<Fake> {
        MicRoute::new(fake.clone(), dir.path().join("audio-restore.toml"))
    }

    fn saved(dir: &tempfile::TempDir) -> Option<String> {
        std::fs::read_to_string(dir.path().join("audio-restore.toml")).ok()
    }

    #[test]
    fn engage_saves_the_previous_input_once_and_switches() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        assert!(r.engaged());
        assert_eq!(fake.current().as_deref(), Some(MIC_UID));
        assert_eq!(
            saved(&dir).as_deref(),
            Some("previous_input = \"usb-mic\"\n")
        );
        r.engage().unwrap();
        assert_eq!(
            saved(&dir).as_deref(),
            Some("previous_input = \"usb-mic\"\n")
        );
        // A fresh route after a crash, before recover, doesn't overwrite it either.
        let mut again = route(&fake, &dir);
        again.engage().unwrap();
        assert_eq!(
            saved(&dir).as_deref(),
            Some("previous_input = \"usb-mic\"\n")
        );
    }

    #[test]
    fn engage_when_our_mic_is_already_the_default_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(MIC_UID);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        assert!(r.engaged());
        assert_eq!(saved(&dir), None);
        assert!(fake.sets().is_empty());
    }

    #[test]
    fn engage_that_fails_to_switch_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        fake.unplug(MIC_UID);
        let mut r = route(&fake, &dir);
        assert!(r.engage().is_err());
        assert!(!r.engaged());
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_restores_the_previous_input_and_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        r.release();
        assert!(!r.engaged());
        assert_eq!(fake.current().as_deref(), Some(USB));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_when_the_previous_input_is_gone_leaves_macos_choice() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        fake.unplug(USB);
        fake.pick("headset");
        r.release();
        assert_eq!(fake.current().as_deref(), Some("headset"));
        assert_eq!(fake.sets(), [MIC_UID]);
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_when_the_previous_input_is_gone_moves_off_our_mic() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        fake.unplug(USB);
        r.release();
        assert_eq!(fake.current().as_deref(), Some(BUILTIN));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_with_nothing_saved_moves_off_our_mic() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(MIC_UID);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        r.release();
        assert_eq!(fake.current().as_deref(), Some(BUILTIN));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_that_fails_to_restore_moves_to_the_builtin_mic_and_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        fake.0.borrow_mut().refused = vec![USB.into()];
        r.release();
        assert!(!r.engaged());
        assert_eq!(fake.current().as_deref(), Some(BUILTIN));
        assert_eq!(
            saved(&dir).as_deref(),
            Some("previous_input = \"usb-mic\"\n")
        );
        // The next launch retries the user's own device.
        fake.0.borrow_mut().refused.clear();
        let mut next = route(&fake, &dir);
        next.recover();
        assert_eq!(fake.current().as_deref(), Some(USB));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn release_that_cannot_switch_at_all_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.engage().unwrap();
        fake.0.borrow_mut().refused = vec![USB.into(), BUILTIN.into()];
        r.release();
        assert!(!r.engaged());
        assert_eq!(fake.current().as_deref(), Some(MIC_UID));
        assert_eq!(
            saved(&dir).as_deref(),
            Some("previous_input = \"usb-mic\"\n")
        );
    }

    #[test]
    fn engage_that_cannot_read_the_default_does_not_switch() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        fake.0.borrow_mut().default = None;
        let mut r = route(&fake, &dir);
        assert!(r.engage().is_err());
        assert!(!r.engaged());
        assert!(fake.sets().is_empty());
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn recover_after_a_crash_restores_the_saved_input() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        route(&fake, &dir).engage().unwrap(); // and then the app dies
        assert_eq!(fake.current().as_deref(), Some(MIC_UID));
        let mut r = route(&fake, &dir);
        r.recover();
        assert!(!r.engaged());
        assert_eq!(fake.current().as_deref(), Some(USB));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn recover_with_the_saved_input_gone_falls_back_to_the_builtin_mic() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        route(&fake, &dir).engage().unwrap();
        fake.unplug(USB);
        route(&fake, &dir).recover();
        assert_eq!(fake.current().as_deref(), Some(BUILTIN));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn recover_without_a_file_moves_off_our_mic_only() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        route(&fake, &dir).recover();
        assert_eq!(fake.current().as_deref(), Some(USB));
        assert!(fake.sets().is_empty());

        fake.pick(MIC_UID);
        route(&fake, &dir).recover();
        assert_eq!(fake.current().as_deref(), Some(BUILTIN));
        assert_eq!(saved(&dir), None);
    }

    #[test]
    fn reassert_switches_back_when_the_default_moves() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.reassert(); // not engaged: hands off
        assert_eq!(fake.current().as_deref(), Some(USB));
        r.engage().unwrap();
        fake.pick(BUILTIN);
        r.reassert();
        assert_eq!(fake.current().as_deref(), Some(MIC_UID));
        r.reassert(); // already ours: no set
        assert_eq!(fake.sets(), [MIC_UID, MIC_UID]);
        // The first default is still what release restores.
        r.release();
        assert_eq!(fake.current().as_deref(), Some(USB));
    }

    #[test]
    fn nothing_is_written_while_never_engaged() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::with_default(USB);
        let mut r = route(&fake, &dir);
        r.recover();
        r.reassert();
        r.release();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(fake.sets().is_empty());
        assert_eq!(fake.current().as_deref(), Some(USB));
    }

    /// Waits up to `within` for `want`, skipping other events.
    fn wait_for(
        rx: &std::sync::mpsc::Receiver<AudioDevEv>,
        want: AudioDevEv,
        within: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if ev == want => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    #[test]
    #[ignore = "reads this Mac's inputs; needs the CursorJump audio driver"]
    fn core_audio_reads_this_macs_inputs() {
        assert!(driver_present());
        let builtin = CoreAudioDevices
            .builtin_input_uid()
            .expect("a built-in mic");
        assert_ne!(builtin, MIC_UID);
        assert!(CoreAudioDevices.exists(&builtin));
        let default = CoreAudioDevices
            .default_input_uid()
            .expect("a default input");
        assert!(CoreAudioDevices.exists(&default));
        assert!(!CoreAudioDevices.exists("com.cursorjump.audio.no-such-device"));
        println!("default input {default}, built-in mic {builtin}");
    }

    #[test]
    #[ignore = "switches this Mac's default input; needs the CursorJump audio driver"]
    fn engage_and_release_on_this_mac() {
        /// Releases even if an assertion panics, so the Mac gets its input back.
        struct ReleaseOnDrop(MicRoute<CoreAudioDevices>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.release();
            }
        }

        let before = CoreAudioDevices.default_input_uid();
        println!(
            "default input before the test: {}",
            before.as_deref().unwrap_or("<none>")
        );
        assert!(driver_present(), "the CursorJump audio driver is installed");
        let expected = if before.as_deref() == Some(MIC_UID) {
            CoreAudioDevices.builtin_input_uid()
        } else {
            before.clone()
        };
        // Declared before the guard, so the restore file outlives it: locals
        // drop in reverse order, also while unwinding.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("audio-restore.toml");
        let mut r = ReleaseOnDrop(MicRoute::new(CoreAudioDevices, file.clone()));
        r.0.engage().unwrap();
        assert_eq!(
            CoreAudioDevices.default_input_uid().as_deref(),
            Some(MIC_UID)
        );
        // Time to see the switch in System Settings > Sound > Input.
        std::thread::sleep(std::time::Duration::from_secs(3));
        r.0.release();
        assert_eq!(CoreAudioDevices.default_input_uid(), expected);
        assert!(!file.exists());
    }

    unsafe extern "C-unwind" fn record_nothing(
        _: AudioObjectID,
        _: NonNull<objc2_core_audio_types::AudioTimeStamp>,
        _: NonNull<objc2_core_audio_types::AudioBufferList>,
        _: NonNull<objc2_core_audio_types::AudioTimeStamp>,
        _: NonNull<objc2_core_audio_types::AudioBufferList>,
        _: NonNull<objc2_core_audio_types::AudioTimeStamp>,
        _: *mut c_void,
    ) -> i32 {
        0
    }

    #[test]
    #[ignore = "records from the CursorJump Microphone; needs the driver and no sandbox"]
    fn mic_listening_follows_a_recording_client() {
        use objc2_core_audio::{
            AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
            AudioDeviceStart, AudioDeviceStop,
        };
        use std::time::Duration;

        let mic = device_id(MIC_UID).expect("the CursorJump audio driver is installed");
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = AudioWatch::start(move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        assert!(
            wait_for(&rx, AudioDevEv::MicListening(false), Duration::from_secs(1)),
            "the watch reports the mic idle at start"
        );

        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `proc_id` outlives the call; the IOProc touches nothing.
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                mic,
                Some(record_nothing),
                std::ptr::null_mut(),
                NonNull::from(&mut proc_id),
            )
        };
        assert_eq!(st, 0, "AudioDeviceCreateIOProcID");
        // SAFETY: the IOProc just created on this device.
        let st = unsafe { AudioDeviceStart(mic, proc_id) };
        assert_eq!(st, 0, "AudioDeviceStart");
        let started = wait_for(&rx, AudioDevEv::MicListening(true), Duration::from_secs(2));
        let polled = running_somewhere(mic);

        // SAFETY: as above; stopped, then destroyed once.
        unsafe {
            AudioDeviceStop(mic, proc_id);
            AudioDeviceDestroyIOProcID(mic, proc_id);
        }
        assert!(
            started,
            "MicListening(true) within 2 s (IsRunningSomewhere polled: {polled})"
        );
        let stopped = wait_for(&rx, AudioDevEv::MicListening(false), Duration::from_secs(2));
        assert!(
            stopped,
            "MicListening(false) within 2 s (IsRunningSomewhere polled: {})",
            running_somewhere(mic)
        );
        drop(watch);
    }
}
