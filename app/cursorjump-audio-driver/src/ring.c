/*
 * The sample-time ring: see ring.h. Realtime safe: no locks, no allocation,
 * no system calls.
 *
 * Writer, per slot: clear the stamp (so a reader that is copying this slot
 * notices the overwrite), release fence, store the data, then store the new
 * stamp with release order. Reader, per slot: load the stamp with acquire
 * order, copy the data, acquire fence, load the stamp again. Both loads must
 * equal sample_time + 1, or the frame reads as 0.
 */
#include "ring.h"

void cj_ring_reset(cj_ring *r) {
    for (uint32_t i = 0; i < CJ_RING_FRAMES; i++) {
        atomic_store_explicit(&r->stamp[i], 0, memory_order_relaxed);
        r->data[i] = 0.0f;
    }
    atomic_thread_fence(memory_order_release);
}

void cj_ring_write(cj_ring *r, uint64_t sample_time, const float *frames, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) {
        uint64_t t = sample_time + i;
        uint32_t slot = (uint32_t)(t % CJ_RING_FRAMES);
        atomic_store_explicit(&r->stamp[slot], 0, memory_order_relaxed);
        atomic_thread_fence(memory_order_release);
        r->data[slot] = frames[i];
        atomic_store_explicit(&r->stamp[slot], t + 1, memory_order_release);
    }
}

void cj_ring_read(cj_ring *r, uint64_t sample_time, float *out, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) {
        uint64_t t = sample_time + i;
        uint32_t slot = (uint32_t)(t % CJ_RING_FRAMES);
        uint64_t before = atomic_load_explicit(&r->stamp[slot], memory_order_acquire);
        float v = r->data[slot];
        atomic_thread_fence(memory_order_acquire);
        uint64_t after = atomic_load_explicit(&r->stamp[slot], memory_order_relaxed);
        out[i] = (before == t + 1 && after == t + 1) ? v : 0.0f;
    }
}
