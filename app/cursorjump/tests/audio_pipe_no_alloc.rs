//! `AudioRx::pull` and `reset` run on a sound card's real-time thread, so
//! they must never allocate or free. This binary counts every heap call made
//! on the test's thread, around the pulls only (`AudioTx::push` runs on the
//! network thread and may allocate).
#![allow(dead_code, reason = "only part of the module is used here")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use cj_core::audio::{AudioStream, encode_pcm};

#[path = "../src/common/audio_pipe.rs"]
mod audio_pipe;

thread_local! {
    static HEAP_CALLS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn count() {
    HEAP_CALLS.with(|c| c.set(c.get() + 1));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count();
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

#[test]
fn pull_and_reset_never_touch_the_heap() {
    for stream in [AudioStream::MacSound, AudioStream::PcMic] {
        let (tx, mut rx) = audio_pipe::audio_pipe(stream);
        let bytes = encode_pcm(&vec![0.25; stream.packet_samples()]);
        let mut out = vec![0.0; 441 * stream.channels()];
        let (mut seq, mut owed, mut heap_calls) = (0u32, 0.0f64, 0u64);
        let frames = (out.len() / stream.channels()) as f64;
        for i in 0..600 {
            owed += frames;
            while owed >= stream.packet_frames() as f64 {
                // A loss now and then.
                if seq % 97 != 5 {
                    tx.push(seq, &bytes);
                }
                seq = seq.wrapping_add(1);
                owed -= stream.packet_frames() as f64;
            }
            let before = HEAP_CALLS.with(Cell::get);
            if i == 400 {
                rx.reset();
            }
            rx.pull(&mut out);
            heap_calls += HEAP_CALLS.with(Cell::get) - before;
        }
        assert!(rx.stats().snapshot().received > 100, "{stream:?}");
        assert_eq!(heap_calls, 0, "{stream:?}");
    }
}
