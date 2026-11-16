//! The Mac's sound, captured for the PC: a muted process tap.
//!
//! A global stereo tap of every process but ours, muted (so the Mac's own
//! speakers stay quiet) and private, is the input of a private aggregate
//! device on the default output. Its IOProc, on Core Audio's real-time
//! thread, scales the samples by [`mix_gain`] and pushes them into a ring;
//! the thread `cj-audio-send` drains the ring into the `MacSound` sender,
//! resampling to 48 kHz if the tap runs at another rate.
//!
//! macOS gives no error when the user refused capture: the tap just
//! delivers exact zeros. [`SoundTap::capture`] tells that apart from
//! silence (T12-c).

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use block2::RcBlock;
use cj_core::audio::{AudioStream, RATE};
use cj_core::audio_state::Capture;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, Message};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectAddPropertyListenerBlock, AudioObjectGetPropertyData, AudioObjectHasProperty,
    AudioObjectID, AudioObjectPropertyAddress, AudioObjectPropertyElement,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListenerBlock, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyMute,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyVolumeDecibels,
    kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyVolumeScalarToDecibels,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyProcessObjectList,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeOutput, kAudioObjectUnknown, kAudioProcessPropertyIsRunningOutput,
    kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
    kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSMutableDictionary, NSNumber, NSString};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use tracing::{debug, info, warn};

use super::devices::{
    Listener, SYSTEM, SendBlock, device_uid, get, global, object_list, set, string_property,
};
use crate::common::audio_pipe::{AudioOut, AudioSender};

const STREAM: AudioStream = AudioStream::MacSound;
/// The ring from the IOProc to `cj-audio-send`: 0.5 s of stereo.
const RING: usize = 48_000;
/// The aggregate's IO buffer: 5.3 ms, about one packet a callback (T12-b).
const BUFFER_FRAMES: u32 = 256;
/// Longest `cj-audio-send` sleeps without an unpark.
const PARK: Duration = Duration::from_millis(2);
/// How often `cj-audio-send` looks for denial.
const LOOK: Duration = Duration::from_millis(500);
/// How long exact zeros, while another app plays, mean capture is refused.
const DENIED_AFTER: Duration = Duration::from_secs(3);
/// Frames per resampler call when the tap isn't at 48 kHz.
const RESAMPLE_CHUNK: usize = 256;
/// Where an output device keeps its volume and mute: the main element,
/// else channel 1.
const ELEMENTS: [AudioObjectPropertyElement; 2] = [kAudioObjectPropertyElementMain, 1];

#[derive(Debug)]
pub enum TapError {
    /// Capture refused. Never returned by [`SoundTap::start`]: refusal
    /// shows only as zeros, later, so [`SoundTap::capture`] reports it.
    #[cfg_attr(
        test,
        expect(dead_code, reason = "denial is reported through SoundTap::capture")
    )]
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
/// Make, rebuild and drop it on the `cj-audio` thread only: starting the
/// tap blocks while macOS shows its permission prompt (T12-d). It is not
/// `Send`, so it stays on the thread that made it.
pub struct SoundTap {
    /// Our own process object, left out of the tap.
    me: AudioObjectID,
    tap: Option<Tap>,
    aggregate: Option<Aggregate>,
    volume: Option<VolumeWatch>,
    /// The IOProc's state, from `Box::leak`; freed in Drop once no
    /// aggregate runs the IOProc. It outlives rebuilds, counters and all.
    io: NonNull<IoProc>,
    shared: Arc<Shared>,
    sender: Option<JoinHandle<()>>,
}

impl SoundTap {
    /// Starts a global muted tap excluding our process, a private aggregate
    /// on the default output and its IOProc. `on_capture` is told, on
    /// `cj-audio-send`, of each change in [`SoundTap::capture`].
    pub fn start(
        out: AudioOut,
        on_capture: impl Fn(Capture) + Send + 'static,
    ) -> Result<SoundTap, TapError> {
        let me = own_process_object()?;
        let shared = Arc::new(Shared::new());
        let (ring, consumer) = RingBuffer::new(RING);
        let sender = {
            let shared = Arc::clone(&shared);
            let sender = out.sender(STREAM);
            std::thread::Builder::new()
                .name("cj-audio-send".into())
                .spawn(move || send(consumer, &shared, sender, me, on_capture))
                .context("starting cj-audio-send")?
        };
        let io = Box::leak(Box::new(IoProc {
            ring,
            shared: Arc::clone(&shared),
            sender: sender.thread().clone(),
        }));
        // From here on, Drop undoes whatever was made.
        let mut tap = SoundTap {
            me,
            tap: None,
            aggregate: None,
            volume: None,
            io: NonNull::from(io),
            shared,
            sender: Some(sender),
        };
        tap.tap = Some(Tap::new(me)?);
        tap.open_aggregate()?;
        Ok(tap)
    }

    /// Moves to the current default output: call on `DefaultOutputChanged`
    /// (S1). Rebuilds the aggregate on the same tap, or on a new tap if
    /// that fails.
    pub fn rebuild(&mut self) -> Result<(), TapError> {
        self.close_aggregate();
        if let Err(e) = self.open_aggregate() {
            warn!("Mac sound: rebuilding on the same tap failed ({e:#}); making a new tap");
            self.close_aggregate();
            self.tap = None;
            self.tap = Some(Tap::new(self.me)?);
            self.open_aggregate()?;
        }
        Ok(())
    }

    /// Whether macOS lets us capture, as far as the stream shows yet.
    pub fn capture(&self) -> Capture {
        capture_from_u8(self.shared.capture.load(Relaxed))
    }

    fn open_aggregate(&mut self) -> Result<()> {
        let tap = self.tap.as_ref().context("no tap")?;
        let out = get::<AudioObjectID>(
            SYSTEM,
            global(kAudioHardwarePropertyDefaultOutputDevice),
            None,
        )
        .ok()
        .filter(|&d| d != kAudioObjectUnknown)
        .context("the Mac has no default output")?;
        let out_uid = device_uid(out).context("reading the default output's UID")?;
        // Listen before reading, so no change falls in between.
        self.volume = Some(VolumeWatch::start(out, Arc::clone(&self.shared)));
        let gain = read_gain(out);
        self.shared.gain.store(gain.to_bits(), Relaxed);
        let mut aggregate = Aggregate::create(&tap.uid, &out_uid, self.io)?;
        self.shared.rate.store(aggregate.rate, Relaxed);
        aggregate.start()?;
        info!(
            "Mac sound: tapping on {out_uid} at {} Hz, gain {gain:.3}",
            aggregate.rate
        );
        self.aggregate = Some(aggregate);
        Ok(())
    }

    fn close_aggregate(&mut self) {
        self.volume = None;
        self.aggregate = None;
    }
}

impl Drop for SoundTap {
    fn drop(&mut self) {
        // Stops the IOProc, destroys its ID, then the aggregate.
        self.close_aggregate();
        self.tap = None;
        // SAFETY: from Box::leak in `start`; no aggregate runs the IOProc
        // any more, so nothing else points at it.
        drop(unsafe { Box::from_raw(self.io.as_ptr()) });
        // The ring's producer is gone: cj-audio-send drains it and ends.
        if let Some(t) = self.sender.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Our own process object, so the tap leaves out the PC's mic that we
/// play into the feed device.
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

/// A global stereo tap of every process but one, muted and private.
struct Tap {
    id: AudioObjectID,
    uid: String,
}

impl Tap {
    fn new(exclude: AudioObjectID) -> Result<Tap> {
        let excluded = NSArray::from_retained_slice(&[NSNumber::new_u32(exclude)]);
        // SAFETY: plain Objective-C calls on a description this function owns.
        let desc = unsafe {
            let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &excluded,
            );
            desc.setName(&NSString::from_str("CursorJump"));
            desc.setPrivate(true);
            desc.setMuteBehavior(CATapMuteBehavior::Muted);
            desc
        };
        let mut id = kAudioObjectUnknown;
        // SAFETY: `id` outlives the call.
        let st = unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut id) };
        ensure!(st == 0, "creating the tap: OSStatus {st}");
        let mut tap = Tap {
            id,
            uid: String::new(),
        };
        tap.uid = string_property(id, kAudioTapPropertyUID).context("reading the tap's UID")?;
        match get::<AudioStreamBasicDescription>(id, global(kAudioTapPropertyFormat), None) {
            Ok(f) => info!(
                "Mac sound: tap {id}: {} Hz, {} ch, flags {:#x}",
                f.mSampleRate, f.mChannelsPerFrame, f.mFormatFlags
            ),
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

/// A private aggregate device whose input is the tap, on the default output.
struct Aggregate {
    dev: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    started: bool,
    /// The rate the IOProc gets.
    rate: u32,
}

impl Aggregate {
    fn create(tap_uid: &str, out_uid: &str, io: NonNull<IoProc>) -> Result<Aggregate> {
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
            (
                kAudioAggregateDeviceNameKey,
                obj(ns("CursorJump sound tap")),
            ),
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
        let mut aggregate = Aggregate {
            dev,
            proc_id: None,
            started: false,
            rate: RATE,
        };

        let st = set(
            dev,
            global(kAudioDevicePropertyBufferFrameSize),
            BUFFER_FRAMES,
        );
        let frames = get::<u32>(dev, global(kAudioDevicePropertyBufferFrameSize), None);
        info!(
            "Mac sound: asked for {BUFFER_FRAMES} frames (OSStatus {st}); the buffer is {frames:?}"
        );
        match get::<f64>(dev, global(kAudioDevicePropertyNominalSampleRate), None) {
            Ok(r) if r >= 1.0 => aggregate.rate = r.round() as u32,
            other => warn!("Mac sound: the aggregate's rate is {other:?}; assuming {RATE} Hz"),
        }

        // SAFETY: `io` stays valid until after this aggregate is destroyed
        // (SoundTap's Drop), and only the IOProc uses it.
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                dev,
                Some(io_proc),
                io.as_ptr().cast(),
                NonNull::from(&mut aggregate.proc_id),
            )
        };
        ensure!(st == 0, "creating the IOProc: OSStatus {st}");
        Ok(aggregate)
    }

    /// Blocks while macOS asks the user for permission, the first time.
    fn start(&mut self) -> Result<()> {
        let t = Instant::now();
        // SAFETY: our own device and IOProc.
        let st = unsafe { AudioDeviceStart(self.dev, self.proc_id) };
        ensure!(st == 0, "starting the IOProc: OSStatus {st}");
        self.started = true;
        debug!("Mac sound: AudioDeviceStart took {:?}", t.elapsed());
        Ok(())
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: our own device and IOProc, each undone once, in order.
        unsafe {
            if self.started {
                AudioDeviceStop(self.dev, self.proc_id);
            }
            if self.proc_id.is_some() {
                AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
            }
            let st = AudioHardwareDestroyAggregateDevice(self.dev);
            if st != 0 {
                warn!("Mac sound: destroying the aggregate: OSStatus {st}");
            }
        }
    }
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

/// The IOProc's own state.
struct IoProc {
    ring: Producer<f32>,
    shared: Arc<Shared>,
    /// `cj-audio-send`, woken after each callback.
    sender: Thread,
}

/// Runs on Core Audio's real-time thread: no allocation, lock or log.
unsafe extern "C-unwind" fn io_proc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the IoProc the IOProc was made with; nothing
    // else touches it while the aggregate runs (see Aggregate::create).
    let io = unsafe { &mut *client.cast::<IoProc>() };
    // SAFETY: Core Audio passes `mNumberBuffers` buffers.
    let buffers = unsafe {
        let list = input.as_ref();
        std::slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize)
    };
    // The tap is the aggregate's last input, after any of the output
    // device's own.
    if let Some(b) = buffers.iter().rev().find(|b| !b.mData.is_null()) {
        // SAFETY: the buffer holds `mDataByteSize` bytes of float32 samples.
        let samples = unsafe {
            std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
        };
        push_frames(
            &mut io.ring,
            samples,
            b.mNumberChannels as usize,
            &io.shared,
        );
    }
    io.shared.callbacks.fetch_add(1, Relaxed);
    io.sender.unpark();
    0
}

/// Counters and settings the IOProc shares with the other threads.
struct Shared {
    /// [`mix_gain`], as f32 bits.
    gain: AtomicU32,
    callbacks: AtomicU64,
    /// A non-zero sample came since `cj-audio-send` last looked.
    heard: AtomicBool,
    /// Frames lost to a full ring.
    dropped: AtomicU64,
    /// The rate the IOProc gets, in Hz.
    rate: AtomicU32,
    /// A [`Capture`], as [`capture_to_u8`].
    capture: AtomicU8,
}

impl Shared {
    fn new() -> Self {
        Shared {
            gain: AtomicU32::new(1.0f32.to_bits()),
            callbacks: AtomicU64::new(0),
            heard: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            rate: AtomicU32::new(RATE),
            capture: AtomicU8::new(capture_to_u8(Capture::Unknown)),
        }
    }
}

fn capture_to_u8(c: Capture) -> u8 {
    match c {
        Capture::Unknown => 0,
        Capture::Allowed => 1,
        Capture::Denied => 2,
    }
}

fn capture_from_u8(v: u8) -> Capture {
    match v {
        1 => Capture::Allowed,
        2 => Capture::Denied,
        _ => Capture::Unknown,
    }
}

/// Pushes `samples` (interleaved, `channels` a frame) into the ring as
/// stereo times the gain; frames that don't fit are dropped and counted.
/// Real-time safe.
fn push_frames(ring: &mut Producer<f32>, samples: &[f32], channels: usize, shared: &Shared) {
    let ch = channels.max(1);
    // Whether capture works shows before the gain: muted is still allowed.
    if samples.iter().any(|&x| x != 0.0) {
        shared.heard.store(true, Relaxed);
    }
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

/// `cj-audio-send`: drains the ring into `sender` until the IOProc's state
/// is freed, and looks for denial every [`LOOK`].
fn send(
    mut ring: Consumer<f32>,
    shared: &Shared,
    mut sender: AudioSender,
    me: AudioObjectID,
    on_capture: impl Fn(Capture),
) {
    let mut rate = RATE;
    let mut resample: Option<Resample> = None;
    let mut watch = CaptureWatch::new();
    let mut next_look = Instant::now() + LOOK;
    let (mut callbacks, mut dropped) = (0, 0);
    loop {
        let r = shared.rate.load(Relaxed);
        if r != rate {
            rate = r;
            resample = (r != RATE).then(|| Resample::new(r));
            info!("Mac sound: the tap runs at {r} Hz");
        }
        let abandoned = ring.is_abandoned();
        if let Ok(chunk) = ring.read_chunk(ring.slots()) {
            let (a, b) = chunk.as_slices();
            for s in [a, b] {
                match &mut resample {
                    Some(r) => r.push(s, |o| sender.push(o)),
                    None => sender.push(s),
                }
            }
            chunk.commit_all();
        }
        if abandoned {
            return;
        }
        let now = Instant::now();
        if now >= next_look {
            next_look = now + LOOK;
            let c = shared.callbacks.load(Relaxed);
            let (ran, heard) = (c != callbacks, shared.heard.swap(false, Relaxed));
            callbacks = c;
            let seen = Seen {
                ran,
                heard,
                // Asked only while it can matter.
                others_playing: ran
                    && !heard
                    && watch.state != Capture::Allowed
                    && others_playing(me),
            };
            if let Some(state) = watch.observe(now, seen) {
                info!("Mac sound: capture {state:?}");
                shared.capture.store(capture_to_u8(state), Relaxed);
                on_capture(state);
            }
            let d = shared.dropped.load(Relaxed);
            if d != dropped {
                warn!(
                    "Mac sound: {} frames dropped, the ring was full",
                    d - dropped
                );
                dropped = d;
            }
        }
        std::thread::park_timeout(PARK);
    }
}

/// Whether a process other than `me` plays sound.
fn others_playing(me: AudioObjectID) -> bool {
    object_list(kAudioHardwarePropertyProcessObjectList)
        .into_iter()
        .filter(|&p| p != me)
        .any(|p| {
            get::<u32>(p, global(kAudioProcessPropertyIsRunningOutput), None).is_ok_and(|r| r != 0)
        })
}

/// What the IOProc and the HAL showed since the last look.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    /// The IOProc ran.
    ran: bool,
    /// It saw a non-zero sample.
    heard: bool,
    /// A process other than ours plays sound.
    others_playing: bool,
}

/// Whether macOS lets us capture (T12-c): `Allowed` on the first non-zero
/// sample; `Denied` after [`DENIED_AFTER`] of exact zeros while another
/// process plays; `Unknown` before either. Once allowed, it stays allowed:
/// an app can keep its output running on silence.
struct CaptureWatch {
    state: Capture,
    /// When the current stretch of zeros while another app plays began.
    zeros_since: Option<Instant>,
}

impl CaptureWatch {
    fn new() -> Self {
        CaptureWatch {
            state: Capture::Unknown,
            zeros_since: None,
        }
    }

    /// Takes one look; returns the new state if it changed.
    fn observe(&mut self, now: Instant, seen: Seen) -> Option<Capture> {
        let next = if seen.heard {
            self.zeros_since = None;
            Capture::Allowed
        } else if self.state == Capture::Allowed {
            Capture::Allowed
        } else if seen.ran && seen.others_playing {
            let since = *self.zeros_since.get_or_insert(now);
            if now.duration_since(since) >= DENIED_AFTER {
                Capture::Denied
            } else {
                self.state
            }
        } else {
            self.zeros_since = None;
            self.state
        };
        (next != self.state).then(|| {
            self.state = next;
            next
        })
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

/// Follows the output device's volume and mute into [`Shared::gain`].
struct VolumeWatch {
    dev: AudioObjectID,
    queue: DispatchRetained<DispatchQueue>,
    block: SendBlock,
    added: Vec<AudioObjectPropertyAddress>,
}

impl VolumeWatch {
    fn start(dev: AudioObjectID, shared: Arc<Shared>) -> VolumeWatch {
        let block: Listener =
            RcBlock::new(move |_: u32, _: NonNull<AudioObjectPropertyAddress>| {
                let gain = read_gain(dev);
                shared.gain.store(gain.to_bits(), Relaxed);
                debug!("Mac sound: gain {gain:.3}");
            });
        let mut watch = VolumeWatch {
            dev,
            queue: DispatchQueue::new("com.cursorjump.audio.volume", DispatchQueueAttr::SERIAL),
            block: SendBlock(block),
            added: Vec::new(),
        };
        for selector in [kAudioDevicePropertyVolumeScalar, kAudioDevicePropertyMute] {
            for element in ELEMENTS {
                let mut address = output(selector, element);
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
                    watch.added.push(address);
                } else {
                    warn!("Mac sound: listening to the volume ({selector:#x}): OSStatus {st}");
                }
            }
        }
        watch
    }
}

impl Drop for VolumeWatch {
    fn drop(&mut self) {
        for address in &mut self.added {
            // SAFETY: as added: the same object, address, queue and block.
            unsafe {
                AudioObjectRemovePropertyListenerBlock(
                    self.dev,
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

fn output(
    selector: AudioObjectPropertySelector,
    element: AudioObjectPropertyElement,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeOutput,
        mElement: element,
    }
}

fn has(dev: AudioObjectID, address: AudioObjectPropertyAddress) -> bool {
    let mut address = address;
    // SAFETY: `address` outlives the call.
    unsafe { AudioObjectHasProperty(dev, NonNull::from(&mut address)) }
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

    const T: Duration = Duration::from_millis(500);

    fn seen(ran: bool, heard: bool, others_playing: bool) -> Seen {
        Seen {
            ran,
            heard,
            others_playing,
        }
    }

    /// Feeds `seen` every 500 ms from `t0` for `n` looks; returns the changes.
    fn run(w: &mut CaptureWatch, t0: Instant, n: u32, s: Seen) -> Vec<Capture> {
        (1..=n).filter_map(|i| w.observe(t0 + T * i, s)).collect()
    }

    #[test]
    fn capture_is_unknown_through_silence() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        // Exact zeros for a minute, but nobody else plays: plain silence.
        assert_eq!(run(&mut w, t0, 120, seen(true, false, false)), []);
        assert_eq!(w.state, Capture::Unknown);
    }

    #[test]
    fn the_first_sound_means_allowed() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        assert_eq!(
            w.observe(t0, seen(true, true, false)),
            Some(Capture::Allowed)
        );
        assert_eq!(w.observe(t0 + T, seen(true, true, true)), None);
        assert_eq!(w.state, Capture::Allowed);
    }

    #[test]
    fn three_seconds_of_zeros_while_another_app_plays_means_denied() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        // The first look starts the stretch; 2.5 s later it isn't 3 s yet.
        assert_eq!(run(&mut w, t0, 6, seen(true, false, true)), []);
        assert_eq!(w.state, Capture::Unknown);
        assert_eq!(
            w.observe(t0 + T * 7, seen(true, false, true)),
            Some(Capture::Denied)
        );
        // Reported once.
        assert_eq!(run(&mut w, t0 + T * 7, 10, seen(true, false, true)), []);
        assert_eq!(w.state, Capture::Denied);
    }

    #[test]
    fn the_stretch_restarts_when_the_other_app_stops() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        assert_eq!(run(&mut w, t0, 5, seen(true, false, true)), []);
        assert_eq!(w.observe(t0 + T * 6, seen(true, false, false)), None);
        // A new stretch: 3 s from its own start.
        assert_eq!(run(&mut w, t0 + T * 6, 6, seen(true, false, true)), []);
        assert_eq!(
            w.observe(t0 + T * 13, seen(true, false, true)),
            Some(Capture::Denied)
        );
    }

    #[test]
    fn no_callbacks_is_no_evidence() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        // The IOProc stalled (say, during a rebuild): zeros prove nothing.
        assert_eq!(run(&mut w, t0, 20, seen(false, false, true)), []);
        assert_eq!(w.state, Capture::Unknown);
    }

    #[test]
    fn allowed_stays_allowed_through_zeros() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        w.observe(t0, seen(true, true, true));
        // An app that keeps its output running but plays silence.
        assert_eq!(run(&mut w, t0, 20, seen(true, false, true)), []);
        assert_eq!(w.state, Capture::Allowed);
    }

    #[test]
    fn sound_after_denied_means_allowed() {
        let mut w = CaptureWatch::new();
        let t0 = Instant::now();
        assert_eq!(
            run(&mut w, t0, 8, seen(true, false, true)),
            [Capture::Denied]
        );
        assert_eq!(
            w.observe(t0 + T * 9, seen(true, true, true)),
            Some(Capture::Allowed)
        );
    }

    fn ring(slots: usize) -> (Producer<f32>, rtrb::Consumer<f32>) {
        rtrb::RingBuffer::new(slots)
    }

    fn drain(c: &mut rtrb::Consumer<f32>) -> Vec<f32> {
        std::iter::from_fn(|| c.pop().ok()).collect()
    }

    #[test]
    fn frames_are_pushed_as_stereo_with_the_gain() {
        let shared = Shared::new();
        shared.gain.store(0.5f32.to_bits(), Relaxed);
        let (mut p, mut c) = ring(64);
        push_frames(&mut p, &[0.2, -0.4, 0.6, 0.8], 2, &shared);
        assert_eq!(drain(&mut c), [0.1, -0.2, 0.3, 0.4]);
        // Mono is doubled; more than two channels keep the first two.
        push_frames(&mut p, &[0.2, 0.4], 1, &shared);
        assert_eq!(drain(&mut c), [0.1, 0.1, 0.2, 0.2]);
        push_frames(&mut p, &[0.2, 0.4, 0.9, 0.9], 4, &shared);
        assert_eq!(drain(&mut c), [0.1, 0.2]);
        assert_eq!(shared.dropped.load(Relaxed), 0);
    }

    #[test]
    fn a_full_ring_drops_whole_frames_and_counts_them() {
        let shared = Shared::new();
        let (mut p, mut c) = ring(5);
        push_frames(&mut p, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 2, &shared);
        // Two frames fit in five slots; the third is dropped.
        assert_eq!(drain(&mut c), [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(shared.dropped.load(Relaxed), 1);
    }

    #[test]
    fn heard_means_a_non_zero_sample_before_the_gain() {
        let shared = Shared::new();
        let (mut p, _c) = ring(64);
        push_frames(&mut p, &[0.0; 8], 2, &shared);
        assert!(!shared.heard.load(Relaxed));
        // Muted: the samples sent are zeros, but capture works.
        shared.gain.store(0.0f32.to_bits(), Relaxed);
        push_frames(&mut p, &[0.0, 0.0, 1e-6, 0.0], 2, &shared);
        assert!(shared.heard.load(Relaxed));
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
    #[ignore = "hands-on: mutes this Mac for about 10 s while it plays a sound; no sandbox"]
    fn the_tap_captures_and_the_speakers_stay_quiet() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        println!(
            "For 10 s the Mac plays the Submarine sound in a loop into a muted tap.
             LISTEN: nothing should come from the Mac's speakers until the test ends."
        );
        let lb = Loopback::new();
        let states = Arc::new(Mutex::new(Vec::new()));
        let tap = {
            let states = Arc::clone(&states);
            SoundTap::start(lb.out.clone(), move |c| states.lock().unwrap().push(c))
                .expect("the tap starts")
        };
        let player = Player::start();
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs(5));
        // The denial check's other input (T12-c): afplay plays.
        let others = others_playing(own_process_object().unwrap());
        std::thread::sleep(Duration::from_secs(5));
        let capture = tap.capture();
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
             {audible} packets not silent; capture {capture:?}, changes {:?}; \
             another process playing: {others}",
            packets.len(),
            dbfs(mean),
            states.lock().unwrap()
        );
        // 200 packets a second.
        assert!(n > 1500, "only {n} packets in 9 s");
        assert!(mean > 0.0, "the tap heard nothing");
        assert_eq!(capture, Capture::Allowed);
        assert!(others, "afplay counts as another process playing");
    }

    #[test]
    #[ignore = "hands-on: someone connects AirPods or wired headphones during it; no sandbox"]
    fn the_tap_stays_muted_across_an_output_switch() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        println!(
            "The Mac plays the Submarine sound in a loop into a muted tap.
             Within 30 s, CONNECT AirPods or wired headphones so they become the Mac's output.
             LISTEN: nothing should play from them, or from the Mac's speakers."
        );
        let lb = Loopback::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let _watch = AudioWatch::start(move |ev| {
            let _ = tx.send(ev);
        })
        .unwrap();
        let mut tap = SoundTap::start(lb.out.clone(), |_| {}).expect("the tap starts");
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
