//! The Mac's sound, captured for the PC: a muted process tap.
//!
//! A global stereo tap of every process but ours, muted (so the Mac's own
//! speakers stay quiet) and private, is the input of a private aggregate
//! device on the default output. Its IOProc, on Core Audio's real-time
//! thread, scales the samples by [`mix_gain`] and pushes them into a ring;
//! the thread `cj-audio-send` drains the ring into the `MacSound` sender,
//! resampling to 48 kHz if the tap runs at another rate.
//!
//! macOS gives no error when the user refused capture: a refused tap
//! delivers exact zeros, and still mutes (F6). So before the global tap,
//! [`SoundTap::start`] probes (P12): an unmuted tap of only our own process
//! listens while we play inaudible noise. Exact zeros mean refused, and no
//! global tap is made.

use std::ffi::{CStr, c_void};
use std::ptr::{NonNull, addr_of};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use block2::RcBlock;
use cj_core::audio::{AudioStream, RATE};
use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, Message};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProc, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListenerBlock,
    AudioObjectGetPropertyData, AudioObjectHasProperty, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyElement, AudioObjectPropertyScope, AudioObjectPropertySelector,
    AudioObjectRemovePropertyListenerBlock, AudioObjectSetPropertyData, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyIOProcStreamUsage, kAudioDevicePropertyMute,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreams,
    kAudioDevicePropertyVolumeDecibels, kAudioDevicePropertyVolumeScalar,
    kAudioDevicePropertyVolumeScalarToDecibels, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectUnknown, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    kAudioTapPropertyUID,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSMutableDictionary, NSNumber, NSString};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use tracing::{debug, error, info, warn};

use super::devices::{
    Listener, SYSTEM, SendBlock, device_uid, get, global, property_size, set, string_property,
};
use crate::common::audio_pipe::{AudioOut, AudioSender};

const STREAM: AudioStream = AudioStream::MacSound;
/// The ring from the IOProc to `cj-audio-send`: 0.5 s of stereo.
const RING: usize = 48_000;
/// The aggregate's IO buffer: 5.3 ms, about one packet a callback (T12-b).
const BUFFER_FRAMES: u32 = 256;
/// Longest `cj-audio-send` sleeps without an unpark.
const PARK: Duration = Duration::from_millis(2);
/// How often `cj-audio-send` may warn about dropped or refused buffers.
const REPORT: Duration = Duration::from_secs(1);
/// Frames per resampler call when the tap isn't at 48 kHz.
const RESAMPLE_CHUNK: usize = 256;
/// Where an output device keeps its volume and mute: the main element,
/// else channel 1.
const ELEMENTS: [AudioObjectPropertyElement; 2] = [kAudioObjectPropertyElementMain, 1];
/// How long the probe listens to its own noise, from the first callback
/// that played it, before calling capture refused.
const PROBE_WINDOW: Duration = Duration::from_millis(500);
/// The longest the probe waits for an answer once its IOProcs started:
/// then it gives up (Failed, never Denied).
const PROBE_CAP: Duration = Duration::from_secs(15);
/// The probe's noise: ±1e-4, -80 dBFS, inaudible.
const PROBE_LEVEL: f32 = 1e-4;

#[derive(Debug)]
pub enum TapError {
    /// The user refused capture (System Settings › Privacy & Security ›
    /// Screen & System Audio Recording). No tap was made: the Mac keeps
    /// its own sound.
    Denied,
    Failed(anyhow::Error),
}

impl std::fmt::Display for TapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TapError::Denied => f.write_str("capturing the Mac's sound was refused"),
            TapError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for TapError {
    fn from(e: anyhow::Error) -> Self {
        TapError::Failed(e)
    }
}

/// The gain that keeps the Mac's volume slider working (S2): 10^(dB/20), 0 when muted, 1 without a volume control.
pub fn mix_gain(volume_db: Option<f32>, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        volume_db.map_or(1.0, |db| 10f32.powf(db / 20.0))
    }
}

/// Sends the Mac's sound to the PC while it exists. Dropping it stops the
/// IOProc and destroys the aggregate, then the tap: the Mac's speakers play
/// again.
///
/// `start` blocks while macOS shows its permission prompt (T12-d), so it
/// runs on a short-lived helper thread (`cj-audio-tap`). The tap is not
/// `Send`: it is moved once, after `start`, to `cj-audio` through
/// `MovedTap` in `mac/audio/mod.rs`, then rebuilt and dropped there only.
/// Any new field must keep `MovedTap`'s SAFETY invariant: no `Rc`, no
/// `Cell`, no thread-affine Objective-C `Retained`, and no non-atomic
/// shared state.
pub struct SoundTap {
    /// Our own process object, left out of the tap.
    me: AudioObjectID,
    tap: Option<Tap>,
    /// Declared before `aggregate`: its listeners go first.
    watch: Option<DeviceWatch>,
    aggregate: Option<Io<Capturer>>,
    shared: Arc<Shared>,
    /// Each new aggregate's ring, to `cj-audio-send`; dropped to stop it.
    rings: Option<Sender<Consumer<f32>>>,
    sender: Option<JoinHandle<()>>,
    /// `cj-audio-send`, for the IOProc to wake.
    sender_thread: Thread,
}

impl SoundTap {
    /// Probes whether macOS lets us capture, then starts a global muted
    /// tap excluding our process, a private aggregate on the default
    /// output and its IOProc. `Err(Denied)` means refused: nothing was
    /// made, and the Mac keeps its own sound.
    pub fn start(out: AudioOut) -> Result<SoundTap, TapError> {
        let me = own_process_object()?;
        probe(me)?;
        let shared = Arc::new(Shared::new());
        let (rings, rings_rx) = unbounded();
        let sender = {
            let shared = Arc::clone(&shared);
            let sender = out.sender(STREAM);
            std::thread::Builder::new()
                .name("cj-audio-send".into())
                .spawn(move || {
                    let run = std::panic::AssertUnwindSafe(|| send(&rings_rx, &shared, sender));
                    if std::panic::catch_unwind(run).is_err() {
                        error!("cj-audio-send panicked; no more Mac sound goes to the PC");
                    }
                })
                .context("starting cj-audio-send")?
        };
        // From here on, Drop undoes whatever was made.
        let mut tap = SoundTap {
            me,
            tap: None,
            watch: None,
            aggregate: None,
            shared,
            rings: Some(rings),
            sender_thread: sender.thread().clone(),
            sender: Some(sender),
        };
        tap.tap = Some(Tap::new(TapKind::AllButUs, me)?);
        tap.open_aggregate()?;
        Ok(tap)
    }

    /// Moves to the current default output: call on `DefaultOutputChanged`
    /// (S1). Rebuilds the aggregate on the same tap, or on a new tap if
    /// that fails. It doesn't probe again: the grant is process-wide.
    ///
    /// After an `Err` the tap is down, so the Mac plays its own sound; a
    /// later rebuild tries again.
    pub fn rebuild(&mut self) -> Result<(), TapError> {
        self.close_aggregate();
        let opened = self.open_aggregate().or_else(|e| {
            warn!("Mac sound: rebuilding on the same tap failed ({e:#}); making a new tap");
            self.close_aggregate();
            self.tap = None;
            self.tap = Some(Tap::new(TapKind::AllButUs, self.me)?);
            self.open_aggregate()
        });
        if let Err(e) = opened {
            // Never leave the Mac muted with nothing reading the tap.
            self.close_aggregate();
            self.tap = None;
            warn!("Mac sound: rebuilding failed ({e:#}); the Mac plays its own sound");
            return Err(e.into());
        }
        Ok(())
    }

    fn open_aggregate(&mut self) -> Result<()> {
        let tap = self.tap.as_ref().context("no tap")?;
        let (out, out_uid) = default_output()?;
        let device = AggregateDevice::create(&tap.uid, &out_uid, "CursorJump sound tap")?;
        let st = set(
            device.0,
            global(kAudioDevicePropertyBufferFrameSize),
            BUFFER_FRAMES,
        );
        let frames = get::<u32>(device.0, global(kAudioDevicePropertyBufferFrameSize), None);
        info!(
            "Mac sound: asked for {BUFFER_FRAMES} frames (OSStatus {st}); the buffer is {frames:?}"
        );
        let layout = device.tap_layout(out, tap.channels)?;
        let (ring, consumer) = RingBuffer::new(RING);
        let capturer = Capturer {
            ring,
            layout,
            shared: Arc::clone(&self.shared),
            sender: self.sender_thread.clone(),
        };
        let mut io = Io::new(
            device.0,
            Some(device),
            Some(capture_proc),
            Box::new(capturer),
        )?;
        io.use_streams(kAudioObjectPropertyScopeInput, |i| i == layout.index)?;
        io.use_streams(kAudioObjectPropertyScopeOutput, |_| false)?;
        self.rings
            .as_ref()
            .context("cj-audio-send is gone")?
            .send(consumer)
            .map_err(|_| anyhow!("cj-audio-send is gone"))?;
        // Declared after `io`, so on an error it drops first.
        let watch = DeviceWatch::start(out, io.dev, Arc::clone(&self.shared));
        // Read after the watch starts, so no change falls in between.
        let rate = read_rate(io.dev).unwrap_or_else(|| {
            warn!("Mac sound: can't read the aggregate's rate; assuming {RATE} Hz");
            RATE
        });
        self.shared.rate.store(rate, Relaxed);
        let gain = read_gain(out);
        self.shared.gain.store(gain.to_bits(), Relaxed);
        io.start()?;
        info!("Mac sound: tapping on {out_uid} at {rate} Hz, gain {gain:.3}");
        self.watch = Some(watch);
        self.aggregate = Some(io);
        Ok(())
    }

    fn close_aggregate(&mut self) {
        self.watch = None;
        self.aggregate = None;
    }
}

impl Drop for SoundTap {
    fn drop(&mut self) {
        // Stops the IOProc, destroys its ID, the aggregate, then the tap.
        self.close_aggregate();
        self.tap = None;
        // cj-audio-send ends once the channel is gone.
        drop(self.rings.take());
        if let Some(t) = self.sender.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Our own process object, so the tap leaves out the PC's mic that we
/// play into the feed device (and the probe's noise).
fn own_process_object() -> Result<AudioObjectID> {
    let pid = std::process::id() as i32;
    let id: AudioObjectID = get(
        SYSTEM,
        global(kAudioHardwarePropertyTranslatePIDToProcessObject),
        Some(&pid.to_ne_bytes()),
    )
    .map_err(|st| anyhow!("finding our Core Audio process object: OSStatus {st}"))?;
    ensure!(
        id != kAudioObjectUnknown,
        "this process has no Core Audio process object"
    );
    Ok(id)
}

/// The default output and its UID.
fn default_output() -> Result<(AudioObjectID, String)> {
    let out = get::<AudioObjectID>(
        SYSTEM,
        global(kAudioHardwarePropertyDefaultOutputDevice),
        None,
    )
    .ok()
    .filter(|&d| d != kAudioObjectUnknown)
    .context("the Mac has no default output")?;
    let uid = device_uid(out).context("reading the default output's UID")?;
    Ok((out, uid))
}

// The probe (P12).

/// Whether macOS lets us capture: an unmuted tap of only our own process,
/// on its own aggregate, listens while our process plays ±1e-4 noise to
/// the default output. Everything is torn down before it returns. Blocks
/// while macOS asks the user, the first time.
fn probe(me: AudioObjectID) -> Result<(), TapError> {
    let (out, out_uid) = default_output()?;
    let tap = Tap::new(TapKind::OnlyUs, me)?;
    let device = AggregateDevice::create(&tap.uid, &out_uid, "CursorJump probe")?;
    let layout = device.tap_layout(out, tap.channels)?;
    let counts = Arc::new(ProbeCounts::default());
    let listener = ProbeListener {
        layout,
        counts: Arc::clone(&counts),
    };
    let mut listen = Io::new(device.0, Some(device), Some(probe_proc), Box::new(listener))?;
    listen.use_streams(kAudioObjectPropertyScopeInput, |i| i == layout.index)?;
    listen.use_streams(kAudioObjectPropertyScopeOutput, |_| false)?;
    let t = Instant::now();
    listen.start()?;
    debug!("Mac sound: the probe's tap started in {:?}", t.elapsed());
    let noise = Noise {
        seed: 0x9E37_79B9,
        counts: Arc::clone(&counts),
    };
    let mut play = Io::new(out, None, Some(noise_proc), Box::new(noise))?;
    play.use_streams(kAudioObjectPropertyScopeInput, |_| false)?;
    play.start()?;
    // The starts aren't timed: the first one waits on the user's answer to
    // macOS's prompt. The window runs from the first noise played.
    let t0 = Instant::now();
    // When the noise began, and how many tap callbacks had come by then.
    let mut noise_began: Option<(Instant, u64)> = None;
    let verdict = loop {
        let listened = counts.listened.load(Relaxed);
        if noise_began.is_none() && counts.played.load(Relaxed) > 0 {
            noise_began = Some((Instant::now(), listened));
        }
        let v = verdict(
            counts.heard.load(Relaxed),
            noise_began.map_or(0, |(_, before)| listened - before),
            noise_began.map(|(at, _)| at.elapsed()),
            t0.elapsed(),
        );
        if v != Verdict::Wait {
            break v;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let waited = t0.elapsed();
    // The noise first, then the probe's aggregate, then its tap.
    drop(play);
    drop(listen);
    drop(tap);
    let refused = counts.mismatched.load(Relaxed);
    info!(
        "Mac sound: probe {verdict:?} after {waited:?} ({} tap callbacks, {} noise callbacks, \
         {refused} with an unexpected stream layout)",
        counts.listened.load(Relaxed),
        counts.played.load(Relaxed)
    );
    if refused > 0 {
        warn!("Mac sound: the probe refused {refused} buffers with an unexpected stream layout");
    }
    match verdict {
        Verdict::Allowed => Ok(()),
        Verdict::Denied => Err(TapError::Denied),
        _ => Err(anyhow!("the probe got no answer: its tap or its noise never ran").into()),
    }
}

/// What the probe has shown so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Keep listening.
    Wait,
    Allowed,
    Denied,
    /// No answer: the tap heard no buffer through the window, or the
    /// noise never played before [`PROBE_CAP`].
    Stalled,
}

/// The probe's decision. Any non-zero sample is allowed. Exact zeros for
/// [`PROBE_WINDOW`] from the first noise played, with the tap's buffers
/// arriving (`listened` of them since the noise began), are refused.
/// `noise_for` is how long ago the noise began, `elapsed` the time since
/// both IOProcs started.
fn verdict(heard: bool, listened: u64, noise_for: Option<Duration>, elapsed: Duration) -> Verdict {
    if heard {
        Verdict::Allowed
    } else if elapsed >= PROBE_CAP {
        Verdict::Stalled
    } else {
        match noise_for {
            Some(d) if d >= PROBE_WINDOW && listened == 0 => Verdict::Stalled,
            Some(d) if d >= PROBE_WINDOW => Verdict::Denied,
            _ => Verdict::Wait,
        }
    }
}

#[derive(Default)]
struct ProbeCounts {
    heard: AtomicBool,
    /// Callbacks that brought the tap's buffer.
    listened: AtomicU64,
    /// Callbacks that wrote noise.
    played: AtomicU64,
    /// Callbacks whose stream layout wasn't the one set up.
    mismatched: AtomicU64,
}

/// The probe tap's IOProc state.
struct ProbeListener {
    layout: TapLayout,
    counts: Arc<ProbeCounts>,
}

/// The noise player's IOProc state.
struct Noise {
    /// xorshift32.
    seed: u32,
    counts: Arc<ProbeCounts>,
}

/// Real-time: notes whether the probe's tap heard anything.
unsafe extern "C-unwind" fn probe_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is this IOProc's own state (Io::new).
    let p = unsafe { &*client.cast::<ProbeListener>() };
    // SAFETY: Core Audio passes a valid list for this callback.
    match unsafe { tap_samples(input, &p.layout) } {
        Tapped::Samples(samples) => {
            if samples.iter().any(|&x| x != 0.0) {
                p.counts.heard.store(true, Relaxed);
            }
            p.counts.listened.fetch_add(1, Relaxed);
        }
        Tapped::Off => {}
        Tapped::Mismatch => {
            p.counts.mismatched.fetch_add(1, Relaxed);
        }
    }
    0
}

/// Real-time: fills every output buffer with ±[`PROBE_LEVEL`] noise.
unsafe extern "C-unwind" fn noise_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    _input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is this IOProc's own state (Io::new).
    let n = unsafe { &mut *client.cast::<Noise>() };
    let mut wrote = false;
    // SAFETY: Core Audio passes a valid list for this callback.
    for b in unsafe { buffers(output) } {
        if b.mData.is_null() || b.mDataByteSize < 4 {
            continue;
        }
        wrote = true;
        // SAFETY: an output buffer of `mDataByteSize` bytes of float32.
        let samples = unsafe {
            std::slice::from_raw_parts_mut(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        };
        for s in samples {
            n.seed ^= n.seed << 13;
            n.seed ^= n.seed >> 17;
            n.seed ^= n.seed << 5;
            *s = if n.seed & 1 == 0 {
                PROBE_LEVEL
            } else {
                -PROBE_LEVEL
            };
        }
    }
    if wrote {
        n.counts.played.fetch_add(1, Relaxed);
    }
    0
}

// Taps, aggregates and IOProcs.

/// Which processes a tap hears.
#[derive(Debug, Clone, Copy)]
enum TapKind {
    /// Every process but ours, muted: the Mac's sound for the PC.
    AllButUs,
    /// Only ours, unmuted: the probe.
    OnlyUs,
}

/// A private stereo process tap.
struct Tap {
    id: AudioObjectID,
    uid: String,
    /// Channels in its stream: 2.
    channels: usize,
}

impl Tap {
    fn new(kind: TapKind, me: AudioObjectID) -> Result<Tap> {
        let processes = NSArray::from_retained_slice(&[NSNumber::new_u32(me)]);
        // SAFETY: plain Objective-C calls on a description this function owns.
        let desc = unsafe {
            let alloc = CATapDescription::alloc();
            let (desc, mute) = match kind {
                TapKind::AllButUs => (
                    CATapDescription::initStereoGlobalTapButExcludeProcesses(alloc, &processes),
                    CATapMuteBehavior::Muted,
                ),
                TapKind::OnlyUs => (
                    CATapDescription::initStereoMixdownOfProcesses(alloc, &processes),
                    CATapMuteBehavior::Unmuted,
                ),
            };
            desc.setName(&NSString::from_str("CursorJump"));
            desc.setPrivate(true);
            desc.setMuteBehavior(mute);
            desc
        };
        let mut id = kAudioObjectUnknown;
        // SAFETY: `id` outlives the call.
        let st = unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut id) };
        ensure!(st == 0, "creating the {kind:?} tap: OSStatus {st}");
        let mut tap = Tap {
            id,
            uid: String::new(),
            channels: 2,
        };
        tap.uid = string_property(id, kAudioTapPropertyUID).context("reading the tap's UID")?;
        match get::<AudioStreamBasicDescription>(id, global(kAudioTapPropertyFormat), None) {
            Ok(f) => {
                info!(
                    "Mac sound: {kind:?} tap {id}: {} Hz, {} ch, flags {:#x}",
                    f.mSampleRate, f.mChannelsPerFrame, f.mFormatFlags
                );
                tap.channels = f.mChannelsPerFrame as usize;
            }
            Err(st) => warn!("Mac sound: reading the tap's format: OSStatus {st}"),
        }
        Ok(tap)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: our own tap, destroyed once.
        let st = unsafe { AudioHardwareDestroyProcessTap(self.id) };
        if st != 0 {
            warn!("Mac sound: destroying the tap: OSStatus {st}");
        }
    }
}

/// A private aggregate device whose input is a tap, on an output device.
/// Dropping it destroys it; an [`Io`] destroys it only after its IOProc.
struct AggregateDevice(AudioObjectID);

impl AggregateDevice {
    fn create(tap_uid: &str, out_uid: &str, name: &str) -> Result<AggregateDevice> {
        static BUILT: AtomicU32 = AtomicU32::new(0);
        let n = BUILT.fetch_add(1, Relaxed);
        let uid = format!("com.cursorjump.sound-tap.{}.{n}", std::process::id());
        let sub_device = dict(&[(kAudioSubDeviceUIDKey, obj(ns(out_uid)))]);
        let sub_tap = dict(&[
            (kAudioSubTapUIDKey, obj(ns(tap_uid))),
            (
                kAudioSubTapDriftCompensationKey,
                obj(NSNumber::new_bool(true)),
            ),
        ]);
        let desc = dict(&[
            (kAudioAggregateDeviceNameKey, obj(ns(name))),
            (kAudioAggregateDeviceUIDKey, obj(ns(&uid))),
            (
                kAudioAggregateDeviceIsPrivateKey,
                obj(NSNumber::new_bool(true)),
            ),
            (kAudioAggregateDeviceMainSubDeviceKey, obj(ns(out_uid))),
            (
                kAudioAggregateDeviceSubDeviceListKey,
                obj(NSArray::from_retained_slice(&[obj(sub_device)])),
            ),
            (
                kAudioAggregateDeviceTapListKey,
                obj(NSArray::from_retained_slice(&[obj(sub_tap)])),
            ),
            // On, the IOProc stops in silence and P4 needs packets (T12-a).
            (
                kAudioAggregateDeviceTapAutoStartKey,
                obj(NSNumber::new_bool(false)),
            ),
        ]);
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
        let cf: &CFDictionary = unsafe { &*Retained::as_ptr(&desc).cast::<CFDictionary>() };
        let mut dev = kAudioObjectUnknown;
        // SAFETY: `dev` outlives the call.
        let st = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut dev)) };
        ensure!(st == 0, "creating the aggregate device: OSStatus {st}");
        Ok(AggregateDevice(dev))
    }

    /// Where the tap's input stream is: the aggregate lists the output
    /// device's own input streams first, then the tap's one.
    fn tap_layout(&self, out: AudioObjectID, channels: usize) -> Result<TapLayout> {
        let own = stream_count(out, kAudioObjectPropertyScopeInput);
        let all = stream_count(self.0, kAudioObjectPropertyScopeInput);
        info!(
            "Mac sound: aggregate {} has {all} input streams ({own} of the output device's own)",
            self.0
        );
        ensure!(
            all == own + 1,
            "the aggregate has {all} input streams; expected {own} + the tap's"
        );
        Ok(TapLayout {
            index: own,
            buffers: all,
            channels,
        })
    }

    /// Destroys it now; Err is the OSStatus.
    fn destroy(self) -> Result<(), i32> {
        let dev = self.0;
        std::mem::forget(self);
        // SAFETY: our own aggregate, destroyed once.
        let st = unsafe { AudioHardwareDestroyAggregateDevice(dev) };
        if st == 0 { Ok(()) } else { Err(st) }
    }
}

impl Drop for AggregateDevice {
    fn drop(&mut self) {
        // SAFETY: our own aggregate, destroyed once.
        let st = unsafe { AudioHardwareDestroyAggregateDevice(self.0) };
        if st != 0 {
            warn!("Mac sound: destroying the aggregate: OSStatus {st}");
        }
    }
}

/// An IOProc on a device, with the state it runs on, and the private
/// aggregate it runs on, if any. Drop stops the IOProc, destroys its ID,
/// then the aggregate, and only then frees the state; if Core Audio
/// refused any of that, the state is leaked rather than freed under it.
struct Io<S> {
    dev: AudioObjectID,
    aggregate: Option<AggregateDevice>,
    proc_id: AudioDeviceIOProcID,
    started: bool,
    /// From `Box::leak`; the IOProc's alone while it runs.
    state: NonNull<S>,
}

impl<S> Io<S> {
    fn new(
        dev: AudioObjectID,
        aggregate: Option<AggregateDevice>,
        proc: AudioDeviceIOProc,
        state: Box<S>,
    ) -> Result<Io<S>> {
        let mut io = Io {
            dev,
            aggregate,
            proc_id: None,
            started: false,
            state: NonNull::from(Box::leak(state)),
        };
        // SAFETY: the state stays valid until after the IOProc is destroyed
        // (Drop), and only the IOProc uses it.
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                dev,
                proc,
                io.state.as_ptr().cast(),
                NonNull::from(&mut io.proc_id),
            )
        };
        ensure!(st == 0, "creating an IOProc on device {dev}: OSStatus {st}");
        Ok(io)
    }

    /// Turns each of this IOProc's streams in `scope` on or off, so the
    /// device opens only what it needs (no microphone).
    fn use_streams(
        &self,
        scope: AudioObjectPropertyScope,
        on: impl Fn(usize) -> bool,
    ) -> Result<()> {
        let n = stream_count(self.dev, scope);
        if n == 0 {
            return Ok(());
        }
        // AudioHardwareIOProcStreamUsage: the IOProc ID, a stream count,
        // then one UInt32 flag per stream. u64s keep the pointer aligned.
        let size = 12 + 4 * n;
        let mut usage = vec![0u64; size.div_ceil(8)];
        let p = usage.as_mut_ptr().cast::<u8>();
        let proc_ptr = self
            .proc_id
            .map_or(std::ptr::null(), |f| f as *const c_void);
        // SAFETY: every write is inside `usage`, at its C layout's offsets.
        unsafe {
            p.cast::<*const c_void>().write(proc_ptr);
            p.add(8).cast::<u32>().write(n as u32);
            for i in 0..n {
                p.add(12 + 4 * i).cast::<u32>().write(u32::from(on(i)));
            }
        }
        let mut address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyIOProcStreamUsage,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        };
        // SAFETY: every pointer outlives the call; `size` bytes of `usage`.
        let st = unsafe {
            AudioObjectSetPropertyData(
                self.dev,
                NonNull::from(&mut address),
                0,
                std::ptr::null(),
                size as u32,
                NonNull::new(p).unwrap().cast(),
            )
        };
        ensure!(
            st == 0,
            "choosing the IOProc's streams on device {}: OSStatus {st}",
            self.dev
        );
        Ok(())
    }

    /// Blocks while macOS asks the user for permission, the first time.
    fn start(&mut self) -> Result<()> {
        // SAFETY: our own IOProc on this device.
        let st = unsafe { AudioDeviceStart(self.dev, self.proc_id) };
        ensure!(st == 0, "starting the IOProc: OSStatus {st}");
        self.started = true;
        Ok(())
    }
}

impl<S> Drop for Io<S> {
    fn drop(&mut self) {
        let mut failed = Vec::new();
        // SAFETY: our own IOProc on this device, each step done once.
        unsafe {
            if self.started {
                let st = AudioDeviceStop(self.dev, self.proc_id);
                if st != 0 {
                    failed.push(("stopping the IOProc", st));
                }
            }
            if self.proc_id.is_some() {
                let st = AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
                if st != 0 {
                    failed.push(("destroying the IOProc", st));
                }
            }
        }
        if let Some(aggregate) = self.aggregate.take()
            && let Err(st) = aggregate.destroy()
        {
            failed.push(("destroying the aggregate", st));
        }
        if failed.is_empty() {
            // SAFETY: from Box::leak in `new`; no IOProc can run on it now.
            drop(unsafe { Box::from_raw(self.state.as_ptr()) });
        } else {
            warn!(
                "Mac sound: on device {}, {failed:?} failed; leaking the IOProc's state",
                self.dev
            );
        }
    }
}

fn stream_count(dev: AudioObjectID, scope: AudioObjectPropertyScope) -> usize {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    property_size(dev, address) as usize / size_of::<AudioObjectID>()
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn obj<T: Message>(r: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { Retained::cast_unchecked(r) }
}

fn dict(
    pairs: &[(&CStr, Retained<AnyObject>)],
) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
    let d = NSMutableDictionary::<NSString, AnyObject>::new();
    for (k, v) in pairs {
        let k = ns(k.to_str().expect("Core Audio's keys are ASCII"));
        // SAFETY: NSString keys and object values, as Core Audio expects.
        unsafe { d.setObject_forKey(v, ProtocolObject::from_ref(&*k)) };
    }
    d
}

/// The buffers of `list`.
///
/// # Safety
/// `list` must point at a valid AudioBufferList for the returned lifetime.
unsafe fn buffers<'a>(list: NonNull<AudioBufferList>) -> &'a [AudioBuffer] {
    let list = list.as_ptr();
    // SAFETY: the list holds `mNumberBuffers` buffers; the pointer comes
    // from the list itself, not from its one-element array.
    unsafe {
        let n = (*list).mNumberBuffers as usize;
        let first = addr_of!((*list).mBuffers).cast::<AudioBuffer>();
        std::slice::from_raw_parts(first, n)
    }
}

/// Where an aggregate's IOProc finds the tap, as set up at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TapLayout {
    /// The tap's input buffer.
    index: usize,
    /// Input buffers in each callback.
    buffers: usize,
    /// Channels in the tap's buffer.
    channels: usize,
}

/// What a callback's input list holds for the tap.
#[derive(Debug, PartialEq)]
enum Tapped<'a> {
    /// The tap's samples, interleaved, [`TapLayout::channels`] a frame.
    Samples(&'a [f32]),
    /// The tap's buffer is there but off.
    Off,
    /// Not the layout set up (say, the output device's inputs changed):
    /// use nothing, so a wrong stream (a microphone) is never sent.
    Mismatch,
}

/// The tap's buffer in `list`, checked against `layout`.
///
/// # Safety
/// As [`buffers`]; the buffers hold float32 samples.
unsafe fn tap_samples<'a>(list: NonNull<AudioBufferList>, layout: &TapLayout) -> Tapped<'a> {
    // SAFETY: as documented.
    let buffers = unsafe { buffers(list) };
    if buffers.len() != layout.buffers {
        return Tapped::Mismatch;
    }
    let Some(b) = buffers.get(layout.index) else {
        return Tapped::Mismatch;
    };
    if b.mData.is_null() {
        return Tapped::Off;
    }
    if b.mNumberChannels as usize != layout.channels {
        return Tapped::Mismatch;
    }
    // SAFETY: the buffer holds `mDataByteSize` bytes of float32 samples.
    Tapped::Samples(unsafe {
        std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
    })
}

// Capture.

/// The capture IOProc's own state.
struct Capturer {
    ring: Producer<f32>,
    layout: TapLayout,
    shared: Arc<Shared>,
    /// `cj-audio-send`, woken after each callback.
    sender: Thread,
}

/// Runs on Core Audio's real-time thread: no allocation, lock or log.
unsafe extern "C-unwind" fn capture_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is this IOProc's own state (Io::new); nothing else
    // touches it while the IOProc exists.
    let c = unsafe { &mut *client.cast::<Capturer>() };
    // SAFETY: Core Audio passes a valid list of float32 buffers.
    match unsafe { tap_samples(input, &c.layout) } {
        Tapped::Samples(samples) => {
            push_frames(&mut c.ring, samples, c.layout.channels, &c.shared);
        }
        Tapped::Off => {}
        Tapped::Mismatch => {
            c.shared.mismatched.fetch_add(1, Relaxed);
        }
    }
    c.sender.unpark();
    0
}

/// Counters and settings the IOProc shares with the other threads.
struct Shared {
    /// [`mix_gain`], as f32 bits.
    gain: AtomicU32,
    /// Frames lost to a full ring.
    dropped: AtomicU64,
    /// Callbacks refused for an unexpected stream layout.
    mismatched: AtomicU64,
    /// The rate the IOProc gets, in Hz.
    rate: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Shared {
            gain: AtomicU32::new(1.0f32.to_bits()),
            dropped: AtomicU64::new(0),
            mismatched: AtomicU64::new(0),
            rate: AtomicU32::new(RATE),
        }
    }
}

/// Pushes `samples` (interleaved, `channels` a frame) into the ring as
/// stereo times the gain; frames that don't fit are dropped and counted.
/// Real-time safe.
fn push_frames(ring: &mut Producer<f32>, samples: &[f32], channels: usize, shared: &Shared) {
    let ch = channels.max(1);
    let frames = samples.len() / ch;
    let fit = frames.min(ring.slots() / 2);
    if fit < frames {
        shared.dropped.fetch_add((frames - fit) as u64, Relaxed);
    }
    let gain = f32::from_bits(shared.gain.load(Relaxed));
    let right = if ch == 1 { 0 } else { 1 };
    if let Ok(chunk) = ring.write_chunk_uninit(fit * 2) {
        chunk.fill_from_iter(
            samples
                .chunks_exact(ch)
                .take(fit)
                .flat_map(|f| [f[0] * gain, f[right] * gain]),
        );
    }
}

/// `cj-audio-send`: drains the current ring into `sender` until `rings`
/// is dropped.
fn send(rings: &Receiver<Consumer<f32>>, shared: &Shared, mut sender: AudioSender) {
    let mut drain = Drain::new();
    let mut next_report = Instant::now() + REPORT;
    let (mut dropped, mut mismatched) = (0, 0);
    loop {
        loop {
            match rings.try_recv() {
                Ok(ring) => drain.set_ring(ring),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        drain.set_rate(shared.rate.load(Relaxed));
        drain.drain(|s| sender.push(s));
        let now = Instant::now();
        if now >= next_report {
            next_report = now + REPORT;
            let d = shared.dropped.load(Relaxed);
            if d != dropped {
                warn!(
                    "Mac sound: {} frames dropped, the ring was full",
                    d - dropped
                );
                dropped = d;
            }
            let m = shared.mismatched.load(Relaxed);
            if m != mismatched {
                warn!(
                    "Mac sound: {} callbacks had an unexpected stream layout; sent silence",
                    m - mismatched
                );
                mismatched = m;
            }
        }
        std::thread::park_timeout(PARK);
    }
}

/// The ring's far end: samples out at 48 kHz.
struct Drain {
    ring: Option<Consumer<f32>>,
    rate: u32,
    resample: Option<Resample>,
}

impl Drain {
    fn new() -> Drain {
        Drain {
            ring: None,
            rate: RATE,
            resample: None,
        }
    }

    /// A new aggregate's ring: the old one's samples are dropped.
    fn set_ring(&mut self, ring: Consumer<f32>) {
        self.ring = Some(ring);
    }

    /// The tap's rate now. On a change, the samples already in the ring
    /// (at the old rate) are dropped, and the resampler starts afresh.
    fn set_rate(&mut self, rate: u32) {
        if rate == self.rate {
            return;
        }
        info!("Mac sound: the tap runs at {rate} Hz");
        self.rate = rate;
        self.resample = (rate != RATE).then(|| Resample::new(rate));
        if let Some(ring) = &mut self.ring
            && let Ok(stale) = ring.read_chunk(ring.slots())
        {
            stale.commit_all();
        }
    }

    /// Hands everything in the ring to `out`, at 48 kHz.
    fn drain(&mut self, mut out: impl FnMut(&[f32])) {
        let Some(ring) = &mut self.ring else {
            return;
        };
        if let Ok(chunk) = ring.read_chunk(ring.slots()) {
            let (a, b) = chunk.as_slices();
            for s in [a, b] {
                match &mut self.resample {
                    Some(r) => r.push(s, &mut out),
                    None => out(s),
                }
            }
            chunk.commit_all();
        }
    }
}

/// A fixed-ratio resampler from the tap's rate to 48 kHz, stereo.
struct Resample {
    resampler: Async<f32>,
    /// One chunk of input, interleaved, filled up to `filled` samples.
    input: Vec<f32>,
    filled: usize,
    output: Vec<f32>,
}

impl Resample {
    fn new(rate: u32) -> Resample {
        let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2)
            .oversampling_factor(256)
            .interpolation(SincInterpolationType::Linear);
        let resampler = Async::new_sinc(
            f64::from(RATE) / f64::from(rate),
            1.0,
            &params,
            RESAMPLE_CHUNK,
            2,
            FixedAsync::Input,
        )
        .expect("valid resampler parameters");
        Resample {
            input: vec![0.0; RESAMPLE_CHUNK * 2],
            filled: 0,
            output: vec![0.0; resampler.output_frames_max() * 2],
            resampler,
        }
    }

    /// Adds interleaved stereo; calls `out` with each resampled chunk.
    fn push(&mut self, mut samples: &[f32], mut out: impl FnMut(&[f32])) {
        while !samples.is_empty() {
            let take = (self.input.len() - self.filled).min(samples.len());
            self.input[self.filled..self.filled + take].copy_from_slice(&samples[..take]);
            self.filled += take;
            samples = &samples[take..];
            if self.filled < self.input.len() {
                return;
            }
            self.filled = 0;
            let frames_out = self.resampler.output_frames_next();
            let input =
                InterleavedSlice::new(&self.input[..], 2, RESAMPLE_CHUNK).expect("sized in new");
            let mut output =
                InterleavedSlice::new_mut(&mut self.output[..frames_out * 2], 2, frames_out)
                    .expect("sized in new");
            let (_, written) = self
                .resampler
                .process_into_buffer(&input, &mut output, None)
                .expect("buffers sized to the resampler");
            out(&self.output[..written * 2]);
        }
    }
}

// Volume, mute and rate.

/// Follows the output device's volume and mute into [`Shared::gain`], and
/// the aggregate's rate (which follows the output device's) into
/// [`Shared::rate`].
struct DeviceWatch {
    queue: DispatchRetained<DispatchQueue>,
    block: SendBlock,
    added: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
}

impl DeviceWatch {
    fn start(out: AudioObjectID, aggregate: AudioObjectID, shared: Arc<Shared>) -> DeviceWatch {
        let block: Listener =
            RcBlock::new(move |_: u32, _: NonNull<AudioObjectPropertyAddress>| {
                let gain = read_gain(out);
                shared.gain.store(gain.to_bits(), Relaxed);
                if let Some(rate) = read_rate(aggregate) {
                    shared.rate.store(rate, Relaxed);
                }
                debug!("Mac sound: gain {gain:.3}");
            });
        let mut watch = DeviceWatch {
            queue: DispatchQueue::new("com.cursorjump.audio.volume", DispatchQueueAttr::SERIAL),
            block: SendBlock(block),
            added: Vec::new(),
        };
        let rate = |dev| {
            (
                dev,
                address(
                    kAudioDevicePropertyNominalSampleRate,
                    kAudioObjectPropertyScopeGlobal,
                    kAudioObjectPropertyElementMain,
                ),
            )
        };
        let volume = [kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyMute]
            .into_iter()
            .flat_map(|sel| ELEMENTS.map(|el| (out, output(sel, el))));
        for (dev, mut address) in volume.chain([rate(out), rate(aggregate)]) {
            if !has(dev, address) {
                continue;
            }
            // SAFETY: the block lives in the watch until it is removed.
            let st = unsafe {
                AudioObjectAddPropertyListenerBlock(
                    dev,
                    NonNull::from(&mut address),
                    Some(&watch.queue),
                    RcBlock::as_ptr(&watch.block.0),
                )
            };
            if st == 0 {
                watch.added.push((dev, address));
            } else {
                warn!(
                    "Mac sound: listening to {:#x} on device {dev}: OSStatus {st}",
                    address.mSelector
                );
            }
        }
        watch
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        for (dev, address) in &mut self.added {
            // SAFETY: as added: the same object, address, queue and block.
            unsafe {
                AudioObjectRemovePropertyListenerBlock(
                    *dev,
                    NonNull::from(address),
                    Some(&self.queue),
                    RcBlock::as_ptr(&self.block.0),
                );
            }
        }
        // Waits for a callback already running.
        self.queue.exec_sync(|| {});
    }
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

fn output(
    selector: AudioObjectPropertySelector,
    element: AudioObjectPropertyElement,
) -> AudioObjectPropertyAddress {
    address(selector, kAudioObjectPropertyScopeOutput, element)
}

fn has(dev: AudioObjectID, address: AudioObjectPropertyAddress) -> bool {
    let mut address = address;
    // SAFETY: `address` outlives the call.
    unsafe { AudioObjectHasProperty(dev, NonNull::from(&mut address)) }
}

/// A device's nominal rate, in Hz.
fn read_rate(dev: AudioObjectID) -> Option<u32> {
    get::<f64>(dev, global(kAudioDevicePropertyNominalSampleRate), None)
        .ok()
        .filter(|&r| r >= 1.0)
        .map(|r| r.round() as u32)
}

/// [`mix_gain`] for the output device's volume and mute now.
fn read_gain(dev: AudioObjectID) -> f32 {
    let (db, muted) = read_volume(dev);
    mix_gain(db, muted)
}

/// The output device's volume in dB (`None` without a volume control) and
/// whether it is muted. The volume slider at 0 counts as muted, whatever
/// the device's lowest dB.
fn read_volume(dev: AudioObjectID) -> (Option<f32>, bool) {
    let mut muted = ELEMENTS
        .iter()
        .find_map(|&el| get::<u32>(dev, output(kAudioDevicePropertyMute, el), None).ok())
        .is_some_and(|m| m != 0);
    for el in ELEMENTS {
        let Ok(scalar) = get::<f32>(dev, output(kAudioDevicePropertyVolumeScalar, el), None) else {
            continue;
        };
        muted |= scalar <= 0.0;
        let db = get::<f32>(dev, output(kAudioDevicePropertyVolumeDecibels, el), None)
            .ok()
            .or_else(|| scalar_to_db(dev, el, scalar));
        return (db, muted);
    }
    (None, muted)
}

/// The device's own translation of a volume scalar into dB.
fn scalar_to_db(
    dev: AudioObjectID,
    element: AudioObjectPropertyElement,
    scalar: f32,
) -> Option<f32> {
    let mut address = output(kAudioDevicePropertyVolumeScalarToDecibels, element);
    let mut value = scalar;
    let mut size = size_of::<f32>() as u32;
    // SAFETY: every pointer outlives the call; the value is read and
    // written in place, `size` bytes.
    let st = unsafe {
        AudioObjectGetPropertyData(
            dev,
            NonNull::from(&mut address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (st == 0 && size as usize == size_of::<f32>()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_gain_follows_volume_and_mute() {
        assert_eq!(mix_gain(Some(0.0), false), 1.0);
        assert!((mix_gain(Some(-6.0206), false) - 0.5).abs() < 1e-4);
        assert_eq!(mix_gain(Some(-20.0), true), 0.0);
        assert_eq!(mix_gain(None, false), 1.0);
        assert_eq!(mix_gain(None, true), 0.0);
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn the_probe_allows_on_any_sound() {
        assert_eq!(verdict(true, 1, Some(MS), MS), Verdict::Allowed);
        // Even before the noise counted as begun, or at the cap.
        assert_eq!(verdict(true, 0, None, MS), Verdict::Allowed);
        assert_eq!(verdict(true, 50, Some(MS), PROBE_CAP), Verdict::Allowed);
    }

    #[test]
    fn the_probe_waits_out_its_window_on_zeros() {
        assert_eq!(verdict(false, 0, None, Duration::ZERO), Verdict::Wait);
        assert_eq!(
            verdict(false, 40, Some(PROBE_WINDOW - MS), PROBE_WINDOW - MS),
            Verdict::Wait
        );
    }

    #[test]
    fn the_window_runs_from_the_first_noise() {
        // The noise began late (a slow output waking): 6 s after the start,
        // 10 ms of it is no answer yet.
        let late = Duration::from_secs(6);
        assert_eq!(verdict(false, 0, None, late), Verdict::Wait);
        assert_eq!(verdict(false, 2, Some(10 * MS), late), Verdict::Wait);
        assert_eq!(
            verdict(false, 40, Some(PROBE_WINDOW), late + PROBE_WINDOW),
            Verdict::Denied
        );
    }

    #[test]
    fn the_probe_denies_after_a_window_of_zeros() {
        assert_eq!(
            verdict(false, 40, Some(PROBE_WINDOW), PROBE_WINDOW),
            Verdict::Denied
        );
    }

    #[test]
    fn zeros_without_the_taps_buffer_are_no_answer() {
        // A window of noise, but the tap's buffer never came.
        assert_eq!(
            verdict(false, 0, Some(PROBE_WINDOW), PROBE_WINDOW),
            Verdict::Stalled
        );
    }

    #[test]
    fn the_cap_gives_no_answer_never_denied() {
        // The noise never played.
        assert_eq!(verdict(false, 0, None, PROBE_CAP), Verdict::Stalled);
        // It began just before the cap.
        assert_eq!(verdict(false, 3, Some(MS), PROBE_CAP), Verdict::Stalled);
    }

    /// An AudioBufferList with three buffers, laid out as Core Audio does.
    #[repr(C)]
    struct ThreeBuffers {
        number: u32,
        buffers: [AudioBuffer; 3],
    }

    fn buffer(channels: u32, data: &mut [f32]) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: (data.len() * 4) as u32,
            mData: data.as_mut_ptr().cast(),
        }
    }

    #[test]
    fn the_tap_buffer_is_chosen_by_its_index() {
        // A headset: its mic's stream (off, so no data), its second
        // stream, then the tap.
        let mut mic = [0.7f32; 4];
        let mut tap = [0.1f32, 0.2, 0.3, 0.4];
        let mut list = ThreeBuffers {
            number: 3,
            buffers: [
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: 0,
                    mData: std::ptr::null_mut(),
                },
                buffer(1, &mut mic),
                buffer(2, &mut tap),
            ],
        };
        let list = NonNull::from(&mut list).cast::<AudioBufferList>();
        let layout = |index, buffers, channels| TapLayout {
            index,
            buffers,
            channels,
        };
        // SAFETY: `list` is a valid three-buffer list of float32 buffers.
        unsafe {
            assert_eq!(buffers(list).len(), 3);
            assert_eq!(tap_samples(list, &layout(2, 3, 2)), Tapped::Samples(&tap));
            // A stream turned off.
            assert_eq!(tap_samples(list, &layout(0, 3, 1)), Tapped::Off);
        }
    }

    #[test]
    fn a_changed_stream_layout_sends_nothing() {
        let mut mic = [0.7f32; 4];
        let mut tap = [0.1f32; 4];
        let mut list = ThreeBuffers {
            number: 3,
            buffers: [
                buffer(1, &mut mic),
                buffer(1, &mut mic.clone()),
                buffer(2, &mut tap),
            ],
        };
        let list = NonNull::from(&mut list).cast::<AudioBufferList>();
        // SAFETY: `list` is a valid three-buffer list of float32 buffers.
        unsafe {
            // Set up for two buffers, the tap second: it now has three.
            let two = TapLayout {
                index: 1,
                buffers: 2,
                channels: 2,
            };
            assert_eq!(tap_samples(list, &two), Tapped::Mismatch);
            // The count matches, but the buffer there is a mono mic.
            let mono = TapLayout {
                index: 1,
                buffers: 3,
                channels: 2,
            };
            assert_eq!(tap_samples(list, &mono), Tapped::Mismatch);
            // An index past the list.
            let past = TapLayout {
                index: 3,
                buffers: 3,
                channels: 2,
            };
            assert_eq!(tap_samples(list, &past), Tapped::Mismatch);
        }
    }

    fn ring(slots: usize) -> (Producer<f32>, Consumer<f32>) {
        RingBuffer::new(slots)
    }

    fn pop_all(c: &mut Consumer<f32>) -> Vec<f32> {
        std::iter::from_fn(|| c.pop().ok()).collect()
    }

    #[test]
    fn frames_are_pushed_as_stereo_with_the_gain() {
        let shared = Shared::new();
        shared.gain.store(0.5f32.to_bits(), Relaxed);
        let (mut p, mut c) = ring(64);
        push_frames(&mut p, &[0.2, -0.4, 0.6, 0.8], 2, &shared);
        assert_eq!(pop_all(&mut c), [0.1, -0.2, 0.3, 0.4]);
        // Mono is doubled; more than two channels keep the first two.
        push_frames(&mut p, &[0.2, 0.4], 1, &shared);
        assert_eq!(pop_all(&mut c), [0.1, 0.1, 0.2, 0.2]);
        push_frames(&mut p, &[0.2, 0.4, 0.9, 0.9], 4, &shared);
        assert_eq!(pop_all(&mut c), [0.1, 0.2]);
        assert_eq!(shared.dropped.load(Relaxed), 0);
    }

    #[test]
    fn a_full_ring_drops_whole_frames_and_counts_them() {
        let shared = Shared::new();
        let (mut p, mut c) = ring(5);
        push_frames(&mut p, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 2, &shared);
        // Two frames fit in five slots; the third is dropped.
        assert_eq!(pop_all(&mut c), [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(shared.dropped.load(Relaxed), 1);
    }

    fn drained(d: &mut Drain) -> Vec<f32> {
        let mut out = Vec::new();
        d.drain(|s| out.extend_from_slice(s));
        out
    }

    #[test]
    fn the_drain_passes_48_khz_through() {
        let (mut p, c) = ring(64);
        let mut d = Drain::new();
        assert_eq!(drained(&mut d), [0.0; 0], "no ring yet");
        d.set_ring(c);
        p.push_entire_slice(&[0.1, 0.2, 0.3, 0.4]).unwrap();
        d.set_rate(RATE);
        assert_eq!(drained(&mut d), [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn a_rate_change_drops_the_stale_samples() {
        let (mut p, c) = ring(64);
        let mut d = Drain::new();
        d.set_ring(c);
        // 48 kHz samples, then the device moves to 44.1 kHz.
        p.push_entire_slice(&[0.5; 8]).unwrap();
        d.set_rate(44_100);
        assert!(d.resample.is_some());
        // And back: what is in the ring now is fresh.
        p.push_entire_slice(&[0.25; 4]).unwrap();
        d.set_rate(RATE);
        assert!(d.resample.is_none());
        p.push_entire_slice(&[0.125; 2]).unwrap();
        assert_eq!(drained(&mut d), [0.125; 2]);
    }

    #[test]
    fn a_new_ring_replaces_the_old_one() {
        let (mut p1, c1) = ring(64);
        let (mut p2, c2) = ring(64);
        let mut d = Drain::new();
        d.set_ring(c1);
        p1.push_entire_slice(&[0.9; 4]).unwrap();
        d.set_ring(c2);
        p2.push_entire_slice(&[0.1, 0.2]).unwrap();
        assert_eq!(drained(&mut d), [0.1, 0.2]);
    }

    #[test]
    fn other_rates_are_resampled_to_48_khz() {
        // One second of a 1 kHz stereo sine at 44.1 kHz, in uneven blocks.
        let rate = 44_100;
        let input: Vec<f32> = (0..rate)
            .flat_map(|n| {
                let v = (n as f32 * 2.0 * std::f32::consts::PI * 1000.0 / rate as f32).sin();
                [0.5 * v; 2]
            })
            .collect();
        let mut r = Resample::new(rate);
        let mut out = Vec::new();
        for block in input.chunks(2 * 333) {
            r.push(block, |o| out.extend_from_slice(o));
        }
        // About 48 000 frames, less what is still in the filter and the
        // last partial chunk.
        let frames = out.len() / 2;
        assert!((47_000..=48_000).contains(&frames), "{frames} frames");
        // Still a sine at 0.5: RMS 0.354, away from the filter's start.
        let tail = &out[out.len() / 2..];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.3536).abs() < 0.01, "rms {rms}");
    }

    // Hands-on tests: real Core Audio IO, so run them with the sandbox off.

    use super::super::devices::{AudioDevEv, AudioWatch};
    use cj_core::audio::decode_pcm;
    use cj_core::proto::Datagram;
    use cj_net::session::{Listener as NetListener, connect};
    use cj_net::{Identity, Trust};
    use std::sync::Mutex;

    /// A loopback session whose far end records each audio packet's
    /// arrival time and RMS.
    struct Loopback {
        rt: tokio::runtime::Runtime,
        _listener: NetListener,
        out: AudioOut,
        packets: Arc<Mutex<Vec<(Instant, f64)>>>,
        reader: tokio::task::JoinHandle<()>,
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
    }

    impl Loopback {
        fn new() -> Loopback {
            let rt = tokio::runtime::Runtime::new().expect("a tokio runtime");
            let ident = || {
                let dir = tempfile::tempdir().unwrap();
                Identity::load_or_create(dir.path()).unwrap()
            };
            let (a, b) = (ident(), ident());
            let any_port = (std::net::Ipv4Addr::LOCALHOST, 0).into();
            let (listener, sa, sb) = rt.block_on(async {
                let listener = NetListener::bind(&a, Trust::PairingMode, any_port, "A")
                    .await
                    .unwrap();
                let (sa, sb) = tokio::join!(
                    listener.accept(),
                    connect(&b, Trust::PairingMode, listener.local_addr(), "B")
                );
                (listener, sa.unwrap(), sb.unwrap())
            });
            let packets: Arc<Mutex<Vec<(Instant, f64)>>> = Arc::default();
            let reader = {
                let packets = Arc::clone(&packets);
                rt.spawn(async move {
                    while let Ok(d) = sb.recv_datagram().await {
                        if let Datagram::Audio { pcm, .. } = d {
                            let level = rms(&decode_pcm(&pcm));
                            packets.lock().unwrap().push((Instant::now(), level));
                        }
                    }
                })
            };
            let out = AudioOut::default();
            out.set(Some(sa));
            Loopback {
                rt,
                _listener: listener,
                out,
                packets,
                reader,
            }
        }

        fn packets(&self) -> Vec<(Instant, f64)> {
            self.packets.lock().unwrap().clone()
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            self.reader.abort();
            let _ = self.rt.block_on(&mut self.reader);
        }
    }

    /// Plays the Submarine sound in a loop until dropped.
    struct Player {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Player {
        fn start() -> Player {
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Relaxed) {
                        let st = std::process::Command::new("afplay")
                            .arg("/System/Library/Sounds/Submarine.aiff")
                            .status()
                            .expect("running afplay");
                        assert!(st.success(), "afplay: {st}");
                    }
                })
            };
            Player {
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Player {
        fn drop(&mut self) {
            self.stop.store(true, Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn dbfs(rms: f64) -> f64 {
        20.0 * rms.max(1e-9).log10()
    }

    /// Mean packet RMS, and how many packets, between two instants.
    fn level(packets: &[(Instant, f64)], from: Instant, to: Instant) -> (f64, usize) {
        let levels: Vec<f64> = packets
            .iter()
            .filter(|(t, _)| (from..to).contains(t))
            .map(|&(_, l)| l)
            .collect();
        let mean = levels.iter().sum::<f64>() / levels.len().max(1) as f64;
        (mean, levels.len())
    }

    #[test]
    #[ignore = "hands-on: plays inaudible noise for up to 0.5 s; no sandbox"]
    fn the_probe_says_allowed_from_terminal() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let t = Instant::now();
        let result = probe(own_process_object().unwrap());
        println!("probe: {result:?} in {:?}", t.elapsed());
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    #[ignore = "hands-on: mutes this Mac for about 10 s while it plays a sound; no sandbox"]
    fn the_tap_captures_and_the_speakers_stay_quiet() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        println!(
            "For 10 s the Mac plays the Submarine sound in a loop into a muted tap.
             LISTEN: nothing should come from the Mac's speakers until the test ends."
        );
        let lb = Loopback::new();
        let tap = SoundTap::start(lb.out.clone()).expect("the tap starts");
        let player = Player::start();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs(10));
        let t1 = Instant::now();
        drop(tap);
        drop(player);
        println!("DONE: the Mac's speakers play again.");

        let packets = lb.packets();
        // Skip the first second: afplay takes a moment to start.
        let (mean, n) = level(&packets, t0 + Duration::from_secs(1), t1);
        let audible = packets.iter().filter(|&&(_, l)| l > 0.0).count();
        println!(
            "{} packets in all; over seconds 1-10, {n} packets, mean RMS {mean:.4} ({:.1} dBFS); \
             {audible} packets not silent",
            packets.len(),
            dbfs(mean),
        );
        // 200 packets a second.
        assert!(n > 1500, "only {n} packets in 9 s");
        assert!(mean > 0.0, "the tap heard nothing");
    }

    #[test]
    #[ignore = "hands-on: someone connects AirPods or wired headphones during it; no sandbox"]
    fn the_tap_stays_muted_across_an_output_switch() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        println!(
            "The Mac plays the Submarine sound in a loop into a muted tap.
             Within 30 s, CONNECT AirPods or wired headphones so they become the Mac's output.
             LISTEN: nothing should play from them, or from the Mac's speakers.
             WATCH: the mic indicator must stay off, and AirPods must not drop to call quality."
        );
        let lb = Loopback::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watch = AudioWatch::start(move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        let mut tap = SoundTap::start(lb.out.clone()).expect("the tap starts");
        let player = Player::start();

        let deadline = Instant::now() + Duration::from_secs(30);
        let switched = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(AudioDevEv::DefaultOutputChanged) => break Instant::now(),
                Ok(_) => {}
                Err(_) => panic!("the default output didn't change within 30 s"),
            }
        };
        tap.rebuild().expect("the tap moves to the new output");
        println!("The output changed; keep listening for 5 s.");
        // Later switches (a second device event) rebuild too.
        let end = switched + Duration::from_secs(5);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(AudioDevEv::DefaultOutputChanged) = rx.recv_timeout(left) {
                tap.rebuild().expect("the tap moves to the new output");
            }
        }
        let t1 = Instant::now();
        drop(tap);
        drop(player);
        println!("DONE: the Mac plays sound again.");

        let packets = lb.packets();
        let resumed = packets
            .iter()
            .find(|&&(t, l)| t > switched && l > 0.0)
            .map(|&(t, _)| t - switched);
        let (mean, n) = level(&packets, switched + Duration::from_secs(1), t1);
        println!(
            "sound resumed {resumed:?} after the switch; then {n} packets, mean RMS {mean:.4} ({:.1} dBFS)",
            dbfs(mean)
        );
        let resumed = resumed.expect("sound came back after the switch");
        assert!(
            resumed < Duration::from_secs(1),
            "resumed after {resumed:?}"
        );
        assert!(n > 600, "only {n} packets in the 4 s after the switch");
        assert!(mean > 0.0);
    }
}
