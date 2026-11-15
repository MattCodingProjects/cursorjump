//! THROWAWAY spike for audio sharing on the Mac (plan Task 1). It proves the Core Audio
//! process tap before the real build (Task 12) and is deleted in Task 15.
//!
//! It creates a global stereo tap of every process except this one, muted and private,
//! wraps it in a private aggregate device whose main sub-device is the default output,
//! and runs an IOProc on it. Every second it prints one line:
//!
//! ```text
//! [+12.0] rms -23.4 dBFS peak 0.412 | cb 94 frames 512..512 | zero-cb 0 | out "MacBook Pro Speakers" vol 0.50 mute 0 running 1
//! ```
//!
//! Flags:
//! - `--seconds N` (default 60): run for N seconds, then tear the tap down.
//! - `--rebuild-on-switch`: rebuild the aggregate when the default output changes (S1).
//! - `--buffer-frames N`: ask the aggregate for an IO buffer of N frames (S4).
//! - `--unmuted`: make the tap unmuted (to compare levels for S2).
//!
//! Ctrl+C tears down cleanly. A kill -9 leaves nothing behind: the tap and the private
//! aggregate die with the process (that is what A-R2 relies on).
//!
//! ```text
//! cargo run -p cursorjump --example audio_spike_mac -- --seconds 60
//! ```

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::AnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_core_audio::*;
    use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
    use objc2_core_foundation::CFDictionary;
    use objc2_foundation::{NSArray, NSMutableDictionary, NSNumber, NSString};

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(
            mode: *const c_void,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
    }

    /// Turns the main thread's run loop (and so the main dispatch queue) for `secs`.
    /// Core Audio finishes starting a new aggregate there; a sleeping main thread never starts it.
    fn pump(secs: f64) {
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, secs, 0) };
    }

    static STOP: AtomicBool = AtomicBool::new(false);
    static OUTPUT_CHANGED: AtomicBool = AtomicBool::new(false);

    /// Written only by the IOProc (one writer); read by the main thread once a second.
    struct Stats {
        callbacks: AtomicU64,
        samples: AtomicU64,
        sumsq_bits: AtomicU64, // f64 bits, running total
        peak_bits: AtomicU32,  // f32 bits, max since the last read (reset by the reader)
        zero_callbacks: AtomicU64,
        min_frames: AtomicU32,
        max_frames: AtomicU32,
    }

    static STATS: Stats = Stats {
        callbacks: AtomicU64::new(0),
        samples: AtomicU64::new(0),
        sumsq_bits: AtomicU64::new(0),
        peak_bits: AtomicU32::new(0),
        zero_callbacks: AtomicU64::new(0),
        min_frames: AtomicU32::new(u32::MAX),
        max_frames: AtomicU32::new(0),
    };

    struct Args {
        seconds: u64,
        rebuild_on_switch: bool,
        buffer_frames: Option<u32>,
        unmuted: bool,
        no_autostart: bool,
    }

    fn parse_args() -> Args {
        let mut a = Args {
            seconds: 60,
            rebuild_on_switch: false,
            buffer_frames: None,
            unmuted: false,
            no_autostart: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--seconds" => {
                    a.seconds = it.next().and_then(|s| s.parse().ok()).expect("--seconds N")
                }
                "--rebuild-on-switch" => a.rebuild_on_switch = true,
                "--buffer-frames" => {
                    a.buffer_frames = Some(
                        it.next()
                            .and_then(|s| s.parse().ok())
                            .expect("--buffer-frames N"),
                    )
                }
                "--unmuted" => a.unmuted = true,
                "--no-autostart" => a.no_autostart = true,
                other => panic!("unknown flag {other}"),
            }
        }
        a
    }

    fn addr(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: element,
        }
    }

    fn global(selector: u32) -> AudioObjectPropertyAddress {
        addr(
            selector,
            kAudioObjectPropertyScopeGlobal,
            kAudioObjectPropertyElementMain,
        )
    }

    /// Reads a fixed-size property. Returns the OSStatus on failure.
    fn get<T: Copy>(
        obj: AudioObjectID,
        mut a: AudioObjectPropertyAddress,
        qualifier: Option<&[u8]>,
    ) -> Result<T, i32> {
        let mut out = std::mem::MaybeUninit::<T>::uninit();
        let mut size = size_of::<T>() as u32;
        let (qsize, qptr) = match qualifier {
            Some(q) => (q.len() as u32, q.as_ptr().cast::<c_void>()),
            None => (0, std::ptr::null()),
        };
        let st = unsafe {
            AudioObjectGetPropertyData(
                obj,
                NonNull::from(&mut a),
                qsize,
                qptr,
                NonNull::from(&mut size),
                NonNull::new(out.as_mut_ptr().cast()).unwrap(),
            )
        };
        if st != 0 {
            Err(st)
        } else {
            Ok(unsafe { out.assume_init() })
        }
    }

    fn set<T: Copy>(obj: AudioObjectID, mut a: AudioObjectPropertyAddress, value: T) -> i32 {
        let mut v = value;
        unsafe {
            AudioObjectSetPropertyData(
                obj,
                NonNull::from(&mut a),
                0,
                std::ptr::null(),
                size_of::<T>() as u32,
                NonNull::from(&mut v).cast(),
            )
        }
    }

    /// Reads a CFString property (returned +1 by Core Audio; toll-free bridged to NSString).
    fn get_string(obj: AudioObjectID, a: AudioObjectPropertyAddress) -> Result<String, i32> {
        let p: *mut NSString = get(obj, a, None)?;
        match unsafe { Retained::from_raw(p) } {
            Some(s) => Ok(s.to_string()),
            None => Err(-1),
        }
    }

    fn fourcc(st: i32) -> String {
        let b = st.to_be_bytes();
        if b.iter().all(|c| c.is_ascii_graphic()) {
            format!("{st} ('{}')", String::from_utf8_lossy(&b))
        } else {
            format!("{st}")
        }
    }

    fn default_output() -> AudioObjectID {
        get(
            kAudioObjectSystemObject as AudioObjectID,
            global(kAudioHardwarePropertyDefaultOutputDevice),
            None,
        )
        .unwrap_or(0)
    }

    fn device_name(dev: AudioObjectID) -> String {
        get_string(dev, global(kAudioObjectPropertyName))
            .unwrap_or_else(|e| format!("<name err {}>", fourcc(e)))
    }

    /// Volume and mute of an output device: the main element first, else channel 1.
    fn volume(dev: AudioObjectID) -> (Option<f32>, Option<u32>) {
        let scope = kAudioObjectPropertyScopeOutput;
        let vol = [kAudioObjectPropertyElementMain, 1].iter().find_map(|&el| {
            get::<f32>(dev, addr(kAudioDevicePropertyVolumeScalar, scope, el), None).ok()
        });
        let mute = [kAudioObjectPropertyElementMain, 1]
            .iter()
            .find_map(|&el| get::<u32>(dev, addr(kAudioDevicePropertyMute, scope, el), None).ok());
        (vol, mute)
    }

    fn format_line(f: &AudioStreamBasicDescription) -> String {
        format!(
            "{} Hz, {} ch, {} bits, flags {:#x} (float {}, non-interleaved {}), {} bytes/frame",
            f.mSampleRate,
            f.mChannelsPerFrame,
            f.mBitsPerChannel,
            f.mFormatFlags,
            f.mFormatFlags & 1 != 0,
            f.mFormatFlags & (1 << 5) != 0,
            f.mBytesPerFrame
        )
    }

    unsafe extern "C-unwind" fn io_proc(
        _dev: AudioObjectID,
        _now: NonNull<AudioTimeStamp>,
        input: NonNull<AudioBufferList>,
        _input_time: NonNull<AudioTimeStamp>,
        _output: NonNull<AudioBufferList>,
        _output_time: NonNull<AudioTimeStamp>,
        _client: *mut c_void,
    ) -> i32 {
        // The tap's float32 samples arrive as the aggregate's input.
        let list = unsafe { input.as_ref() };
        let n = list.mNumberBuffers as usize;
        let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), n) };
        let mut sumsq = 0.0f64;
        let mut count = 0u64;
        let mut peak = 0.0f32;
        let mut frames = 0u32;
        let mut all_zero = true;
        for b in bufs {
            if b.mData.is_null() {
                continue;
            }
            let samples = b.mDataByteSize as usize / 4;
            let ch = b.mNumberChannels.max(1);
            frames = frames.max(samples as u32 / ch);
            let s = unsafe { std::slice::from_raw_parts(b.mData.cast::<f32>(), samples) };
            for &x in s {
                if x != 0.0 {
                    all_zero = false;
                }
                sumsq += (x as f64) * (x as f64);
                peak = peak.max(x.abs());
            }
            count += samples as u64;
        }
        let st = &STATS;
        st.callbacks.store(st.callbacks.load(Relaxed) + 1, Relaxed);
        st.samples.store(st.samples.load(Relaxed) + count, Relaxed);
        st.sumsq_bits.store(
            (f64::from_bits(st.sumsq_bits.load(Relaxed)) + sumsq).to_bits(),
            Relaxed,
        );
        if peak > f32::from_bits(st.peak_bits.load(Relaxed)) {
            st.peak_bits.store(peak.to_bits(), Relaxed);
        }
        if all_zero {
            st.zero_callbacks
                .store(st.zero_callbacks.load(Relaxed) + 1, Relaxed);
        }
        if frames < st.min_frames.load(Relaxed) {
            st.min_frames.store(frames, Relaxed);
        }
        if frames > st.max_frames.load(Relaxed) {
            st.max_frames.store(frames, Relaxed);
        }
        0
    }

    struct Aggregate {
        dev: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
    }

    fn ns(s: &str) -> Retained<NSString> {
        NSString::from_str(s)
    }

    fn obj<T: objc2::Message>(r: Retained<T>) -> Retained<AnyObject> {
        unsafe { Retained::cast_unchecked(r) }
    }

    fn dict(
        pairs: Vec<(&str, Retained<AnyObject>)>,
    ) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
        let d = NSMutableDictionary::<NSString, AnyObject>::new();
        for (k, v) in pairs {
            unsafe { d.setObject_forKey(&v, objc2::runtime::ProtocolObject::from_ref(&*ns(k))) };
        }
        d
    }

    fn key(k: &std::ffi::CStr) -> &str {
        k.to_str().unwrap()
    }

    fn build_aggregate(
        tap_uid: &str,
        n: u32,
        buffer_frames: Option<u32>,
        autostart: bool,
        t0: Instant,
    ) -> Result<Aggregate, String> {
        let out = default_output();
        let out_uid = get_string(out, global(kAudioDevicePropertyDeviceUID))
            .map_err(|e| format!("default output UID: {}", fourcc(e)))?;
        let sub = dict(vec![(key(kAudioSubDeviceUIDKey), obj(ns(&out_uid)))]);
        let tap = dict(vec![
            (key(kAudioSubTapUIDKey), obj(ns(tap_uid))),
            (
                key(kAudioSubTapDriftCompensationKey),
                obj(NSNumber::new_bool(true)),
            ),
        ]);
        let desc = dict(vec![
            (
                key(kAudioAggregateDeviceNameKey),
                obj(ns("CursorJump spike tap")),
            ),
            (
                key(kAudioAggregateDeviceUIDKey),
                obj(ns(&format!(
                    "com.cursorjump.spike.aggregate.{}.{n}",
                    std::process::id()
                ))),
            ),
            (
                key(kAudioAggregateDeviceIsPrivateKey),
                obj(NSNumber::new_bool(true)),
            ),
            (
                key(kAudioAggregateDeviceMainSubDeviceKey),
                obj(ns(&out_uid)),
            ),
            (
                key(kAudioAggregateDeviceSubDeviceListKey),
                obj(NSArray::from_retained_slice(&[obj(sub)])),
            ),
            (
                key(kAudioAggregateDeviceTapListKey),
                obj(NSArray::from_retained_slice(&[obj(tap)])),
            ),
            (
                key(kAudioAggregateDeviceTapAutoStartKey),
                obj(NSNumber::new_bool(autostart)),
            ),
        ]);
        let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&desc).cast::<CFDictionary>()) };
        let mut dev: AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut dev)) };
        if st != 0 {
            return Err(format!(
                "AudioHardwareCreateAggregateDevice: {}",
                fourcc(st)
            ));
        }
        log(
            t0,
            &format!(
                "aggregate {dev} built on \"{}\" ({out_uid})",
                device_name(out)
            ),
        );

        if let Some(frames) = buffer_frames {
            let st = set(dev, global(kAudioDevicePropertyBufferFrameSize), frames);
            let now: Result<u32, i32> = get(dev, global(kAudioDevicePropertyBufferFrameSize), None);
            log(
                t0,
                &format!(
                    "asked for buffer {frames} frames: set {} -> now {now:?}",
                    fourcc(st)
                ),
            );
        }
        let bf: Result<u32, i32> = get(dev, global(kAudioDevicePropertyBufferFrameSize), None);
        let rate: Result<f64, i32> = get(dev, global(kAudioDevicePropertyNominalSampleRate), None);
        log(
            t0,
            &format!("aggregate buffer frame size {bf:?}, nominal rate {rate:?}"),
        );

        let mut proc_id: AudioDeviceIOProcID = None;
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                dev,
                Some(io_proc),
                std::ptr::null_mut(),
                NonNull::from(&mut proc_id),
            )
        };
        if st != 0 {
            unsafe { AudioHardwareDestroyAggregateDevice(dev) };
            return Err(format!("AudioDeviceCreateIOProcID: {}", fourcc(st)));
        }
        log(
            t0,
            &format!("ioproc id {:?}", proc_id.map(|f| f as *const ())),
        );
        let ts = Instant::now();
        let st = unsafe { AudioDeviceStart(dev, proc_id) };
        log(
            t0,
            &format!("AudioDeviceStart -> {} in {:?}", fourcc(st), ts.elapsed()),
        );
        if st != 0 {
            return Err(format!("AudioDeviceStart: {}", fourcc(st)));
        }
        pump(0.2);
        diagnose(dev, t0);
        Ok(Aggregate { dev, proc_id })
    }

    fn prop_size(obj: AudioObjectID, mut a: AudioObjectPropertyAddress) -> Result<u32, i32> {
        let mut size = 0u32;
        let st = unsafe {
            AudioObjectGetPropertyDataSize(
                obj,
                NonNull::from(&mut a),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            )
        };
        if st != 0 { Err(st) } else { Ok(size) }
    }

    fn diagnose(dev: AudioObjectID, t0: Instant) {
        let alive: Result<u32, i32> = get(dev, global(kAudioDevicePropertyDeviceIsAlive), None);
        let running: Result<u32, i32> = get(dev, global(kAudioDevicePropertyDeviceIsRunning), None);
        let in_streams = prop_size(
            dev,
            addr(
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeInput,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map(|s| s / 4);
        let out_streams = prop_size(
            dev,
            addr(
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
                kAudioObjectPropertyElementMain,
            ),
        )
        .map(|s| s / 4);
        let subs = prop_size(
            dev,
            global(kAudioAggregateDevicePropertyActiveSubDeviceList),
        )
        .map(|s| s / 4);
        let taps = prop_size(dev, global(kAudioAggregateDevicePropertyTapList)).map(|s| s / 4);
        let subtaps =
            prop_size(dev, global(kAudioAggregateDevicePropertySubTapList)).map(|s| s / 4);
        log(
            t0,
            &format!(
                "DIAG agg {dev}: alive {alive:?} running {running:?} in-streams {in_streams:?} out-streams {out_streams:?} active-subdevs {subs:?} taps {taps:?} subtaps {subtaps:?}"
            ),
        );
    }

    fn destroy_aggregate(a: Aggregate, t0: Instant) {
        unsafe {
            let s1 = AudioDeviceStop(a.dev, a.proc_id);
            let s2 = AudioDeviceDestroyIOProcID(a.dev, a.proc_id);
            let s3 = AudioHardwareDestroyAggregateDevice(a.dev);
            log(
                t0,
                &format!(
                    "aggregate {} destroyed (stop {s1}, ioproc {s2}, destroy {s3})",
                    a.dev
                ),
            );
        }
    }

    fn log(t0: Instant, msg: &str) {
        println!("[+{:.1}] {msg}", t0.elapsed().as_secs_f64());
    }

    extern "C" fn on_signal(_: libc::c_int) {
        STOP.store(true, Relaxed);
    }

    pub fn main() {
        let args = parse_args();
        let t0 = Instant::now();
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        let system = kAudioObjectSystemObject as AudioObjectID;

        // This process never runs a CFRunLoop on its main thread, so tell the HAL to use
        // its own thread for notifications (kAudioHardwarePropertyRunLoop = NULL);
        // otherwise a new aggregate device may never finish configuring.
        const RUN_LOOP: u32 = 0x726e_6c70; // 'rnlp', kAudioHardwarePropertyRunLoop
        let st = set::<*const c_void>(system, global(RUN_LOOP), std::ptr::null());
        log(t0, &format!("HAL run loop -> own thread: {}", fourcc(st)));

        // Our own process object, to exclude from the tap.
        let pid = std::process::id() as i32;
        let me: AudioObjectID = match get(
            system,
            global(kAudioHardwarePropertyTranslatePIDToProcessObject),
            Some(&pid.to_ne_bytes()),
        ) {
            Ok(id) => id,
            Err(e) => {
                log(
                    t0,
                    &format!(
                        "TranslatePIDToProcessObject failed: {} (tapping without an exclusion)",
                        fourcc(e)
                    ),
                );
                0
            }
        };
        log(t0, &format!("pid {pid}, process object {me}"));

        let out = default_output();
        let (vol, mute) = volume(out);
        log(
            t0,
            &format!(
                "default output {out} \"{}\" vol {vol:?} mute {mute:?}",
                device_name(out)
            ),
        );

        // The tap.
        let excluded: Vec<Retained<NSNumber>> = if me != 0 {
            vec![NSNumber::new_u32(me)]
        } else {
            vec![]
        };
        let excluded = NSArray::from_retained_slice(&excluded);
        let desc = unsafe {
            CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &excluded,
            )
        };
        unsafe {
            desc.setName(&ns("CursorJump spike"));
            desc.setPrivate(true);
            desc.setMuteBehavior(if args.unmuted {
                CATapMuteBehavior::Unmuted
            } else {
                CATapMuteBehavior::Muted
            });
        }
        let mut tap: AudioObjectID = 0;
        let st = unsafe { AudioHardwareCreateProcessTap(Some(&desc), &mut tap) };
        if st != 0 {
            log(
                t0,
                &format!("AudioHardwareCreateProcessTap FAILED: {}", fourcc(st)),
            );
            return;
        }
        let tap_uid = get_string(tap, global(kAudioTapPropertyUID)).expect("tap UID");
        log(
            t0,
            &format!("tap {tap} created ({tap_uid}), muted {}", !args.unmuted),
        );
        match get::<AudioStreamBasicDescription>(tap, global(kAudioTapPropertyFormat), None) {
            Ok(f) => log(t0, &format!("tap format: {}", format_line(&f))),
            Err(e) => log(t0, &format!("tap format: error {}", fourcc(e))),
        }

        // Default-output listener (always logged; rebuilds only with --rebuild-on-switch).
        let block = RcBlock::new(|_n: u32, _a: NonNull<AudioObjectPropertyAddress>| {
            OUTPUT_CHANGED.store(true, Relaxed);
        });
        let mut a = global(kAudioHardwarePropertyDefaultOutputDevice);
        let st = unsafe {
            AudioObjectAddPropertyListenerBlock(
                system,
                NonNull::from(&mut a),
                None,
                RcBlock::as_ptr(&block),
            )
        };
        if st != 0 {
            log(t0, &format!("listener add failed: {}", fourcc(st)));
        }

        let mut n = 0;
        let mut agg = match build_aggregate(&tap_uid, n, args.buffer_frames, !args.no_autostart, t0)
        {
            Ok(a) => Some(a),
            Err(e) => {
                log(t0, &format!("FAILED: {e}"));
                None
            }
        };

        let end = t0 + Duration::from_secs(args.seconds);
        let (mut last_cb, mut last_samples, mut last_sumsq, mut last_zero) =
            (0u64, 0u64, 0.0f64, 0u64);
        let mut next = t0 + Duration::from_secs(1);
        log(t0, &format!("loop start, stop={}", STOP.load(Relaxed)));
        while Instant::now() < end && !STOP.load(Relaxed) {
            pump(0.02);
            if OUTPUT_CHANGED.swap(false, Relaxed) {
                let out = default_output();
                log(
                    t0,
                    &format!("DEFAULT OUTPUT CHANGED -> {out} \"{}\"", device_name(out)),
                );
                if args.rebuild_on_switch {
                    if let Some(a) = agg.take() {
                        destroy_aggregate(a, t0);
                    }
                    n += 1;
                    agg = match build_aggregate(
                        &tap_uid,
                        n,
                        args.buffer_frames,
                        !args.no_autostart,
                        t0,
                    ) {
                        Ok(a) => Some(a),
                        Err(e) => {
                            log(t0, &format!("rebuild FAILED: {e}"));
                            None
                        }
                    };
                }
            }
            if Instant::now() < next {
                continue;
            }
            next += Duration::from_secs(1);
            let st = &STATS;
            let cb = st.callbacks.load(Relaxed);
            let samples = st.samples.load(Relaxed);
            let sumsq = f64::from_bits(st.sumsq_bits.load(Relaxed));
            let zero = st.zero_callbacks.load(Relaxed);
            let peak = f32::from_bits(st.peak_bits.swap(0, Relaxed));
            let (minf, maxf) = (
                st.min_frames.swap(u32::MAX, Relaxed),
                st.max_frames.swap(0, Relaxed),
            );
            let ds = samples - last_samples;
            let rms = if ds > 0 {
                ((sumsq - last_sumsq) / ds as f64).sqrt()
            } else {
                0.0
            };
            let db = if rms > 0.0 {
                format!("{:.1} dBFS", 20.0 * rms.log10())
            } else {
                "silent (exact 0)".into()
            };
            let out = default_output();
            let (vol, mute) = volume(out);
            let running: Result<u32, i32> = get(
                out,
                global(kAudioDevicePropertyDeviceIsRunningSomewhere),
                None,
            );
            log(
                t0,
                &format!(
                    "rms {db} peak {peak:.3} | cb {} frames {}..{} | zero-cb {} | out \"{}\" vol {} mute {} running {}",
                    cb - last_cb,
                    if minf == u32::MAX { 0 } else { minf },
                    maxf,
                    zero - last_zero,
                    device_name(out),
                    vol.map_or("-".into(), |v| format!("{v:.2}")),
                    mute.map_or("-".into(), |m| m.to_string()),
                    running.map_or("?".into(), |r| r.to_string()),
                ),
            );
            (last_cb, last_samples, last_sumsq, last_zero) = (cb, samples, sumsq, zero);
        }

        log(
            t0,
            &format!(
                "loop end, stop={}, callbacks={}",
                STOP.load(Relaxed),
                STATS.callbacks.load(Relaxed)
            ),
        );
        if let Some(a) = agg.take() {
            destroy_aggregate(a, t0);
        }
        let st = unsafe { AudioHardwareDestroyProcessTap(tap) };
        log(t0, &format!("tap destroyed ({st}); done"));
    }
}
