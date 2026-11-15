/*
 * A lock-free ring of audio frames indexed by absolute sample time.
 *
 * The hidden "CursorJump Mic Feed" output device writes the PC's mic audio
 * into it at its output sample time; the "CursorJump Microphone" input device
 * reads it at its input sample time. Both devices share one clock, so the two
 * times are on the same time base and need no offset.
 *
 * Each slot carries a stamp, sample_time + 1 (0 means "never written"),
 * stored with release order after the slot's data. A read checks the stamp
 * with acquire order before and after copying the data; a stamp that does not
 * match the requested time (never written, an older lap, or overwritten
 * mid-read) reads as silence. One writer and one reader may run at once on
 * different realtime threads: no locks, no allocation.
 */
#ifndef CJ_RING_H
#define CJ_RING_H

#include <stdatomic.h>
#include <stdint.h>

#define CJ_RING_FRAMES 48000

typedef struct {
    float data[CJ_RING_FRAMES];
    _Atomic uint64_t stamp[CJ_RING_FRAMES];
} cj_ring;

/* Marks every slot as never written. */
void cj_ring_reset(cj_ring *r);
/* Stores n frames for sample times sample_time .. sample_time + n - 1. */
void cj_ring_write(cj_ring *r, uint64_t sample_time, const float *frames, uint32_t n);
/* frames never written for this exact time read as 0 */
void cj_ring_read(cj_ring *r, uint64_t sample_time, float *out, uint32_t n);

#endif
