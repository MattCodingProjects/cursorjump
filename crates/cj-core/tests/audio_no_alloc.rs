//! `AudioReceiver::push`, `pull` and `reset` run on a sound card's real-time
//! thread, so once a receiver is built they must never allocate or free.
//! This binary counts every heap call made on the test's thread.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use cj_core::audio::{AudioReceiver, AudioStream};

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

/// A sender in step with a sound card that pulls `out` at a time.
struct Feed {
    seq: u32,
    owed: f64,
}

impl Feed {
    fn run(
        &mut self,
        rx: &mut AudioReceiver,
        stream: AudioStream,
        packet: &[f32],
        out: &mut [f32],
        pulls: usize,
    ) {
        let frames = (out.len() / stream.channels()) as f64;
        for _ in 0..pulls {
            self.owed += frames;
            while self.owed >= stream.packet_frames() as f64 {
                rx.push(self.seq, packet);
                self.seq = self.seq.wrapping_add(1);
                self.owed -= stream.packet_frames() as f64;
            }
            rx.pull(out);
        }
    }
}

#[test]
fn push_pull_and_reset_never_touch_the_heap() {
    for stream in [AudioStream::MacSound, AudioStream::PcMic] {
        let mut rx = AudioReceiver::new(stream);
        let packet = vec![0.25; stream.packet_samples()];
        // An odd device period, so pulls and packets never line up.
        let mut out = vec![0.0; 441 * stream.channels()];
        let mut feed = Feed { seq: 0, owed: 0.0 };
        feed.run(&mut rx, stream, &packet, &mut out, 100);

        let before = HEAP_CALLS.with(Cell::get);
        feed.run(&mut rx, stream, &packet, &mut out, 200);
        // A loss.
        feed.seq += 1;
        feed.run(&mut rx, stream, &packet, &mut out, 50);
        // A swapped pair, then a duplicate.
        rx.push(feed.seq + 1, &packet);
        rx.push(feed.seq, &packet);
        rx.push(feed.seq, &packet);
        feed.seq += 2;
        feed.run(&mut rx, stream, &packet, &mut out, 50);
        // A burst big enough to skip.
        for _ in 0..40 {
            rx.push(feed.seq, &packet);
            feed.seq += 1;
        }
        feed.run(&mut rx, stream, &packet, &mut out, 50);
        // A starve, long enough to underrun.
        for _ in 0..20 {
            rx.pull(&mut out);
        }
        feed.run(&mut rx, stream, &packet, &mut out, 100);
        // A new sequence.
        feed.seq += 50_000;
        feed.run(&mut rx, stream, &packet, &mut out, 100);
        let stats = rx.stats();
        rx.reset();
        feed.run(&mut rx, stream, &packet, &mut out, 100);
        let heap_calls = HEAP_CALLS.with(Cell::get) - before;

        assert!(stats.lost > 0 && stats.late > 0, "{stream:?}: {stats:?}");
        assert!(
            stats.skips > 0 && stats.underruns > 0,
            "{stream:?}: {stats:?}"
        );
        assert_eq!(heap_calls, 0, "{stream:?}");
    }
}
