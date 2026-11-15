/*
 * Tests for the sample-time ring (src/ring.h).
 *
 *   clang -std=c11 -O2 app/cursorjump-audio-driver/src/ring.c \
 *       app/cursorjump-audio-driver/tests/ring_test.c -o target/ring_test && target/ring_test
 *
 * Exits non-zero if any case fails.
 */
#include "../src/ring.h"

#include <stdio.h>

static cj_ring ring;
static int failures;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (!(cond)) {                                                         \
            fprintf(stderr, "  FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);  \
            failures++;                                                        \
            return;                                                            \
        }                                                                      \
    } while (0)

/* A distinct, non-zero value for each sample time. */
static float value_at(uint64_t t) { return (float)(t % 1000003) + 0.5f; }

static void fill(float *buf, uint64_t start, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) buf[i] = value_at(start + i);
}

static void poison(float *buf, uint32_t n) {
    for (uint32_t i = 0; i < n; i++) buf[i] = -1.0f;
}

static void write_then_read_round_trips(void) {
    float in[256], out[256];
    cj_ring_reset(&ring);
    fill(in, 1000, 256);
    cj_ring_write(&ring, 1000, in, 256);
    poison(out, 256);
    cj_ring_read(&ring, 1000, out, 256);
    for (uint32_t i = 0; i < 256; i++) CHECK(out[i] == in[i]);
    /* A read offset inside the written span sees the same frames. */
    poison(out, 100);
    cj_ring_read(&ring, 1100, out, 100);
    for (uint32_t i = 0; i < 100; i++) CHECK(out[i] == value_at(1100 + i));
}

static void unwritten_time_reads_zero(void) {
    float in[64], out[64];
    cj_ring_reset(&ring);
    poison(out, 64);
    cj_ring_read(&ring, 5000, out, 64);
    for (uint32_t i = 0; i < 64; i++) CHECK(out[i] == 0.0f);
    /* Next to written frames, the unwritten ones still read 0. */
    fill(in, 5000, 32);
    cj_ring_write(&ring, 5000, in, 32);
    poison(out, 64);
    cj_ring_read(&ring, 5000, out, 64);
    for (uint32_t i = 0; i < 32; i++) CHECK(out[i] == value_at(5000 + i));
    for (uint32_t i = 32; i < 64; i++) CHECK(out[i] == 0.0f);
}

static void overwritten_one_lap_later_reads_zero_for_old_time(void) {
    float in[128], out[128];
    const uint64_t old_time = 7000, new_time = 7000 + CJ_RING_FRAMES;
    cj_ring_reset(&ring);
    fill(in, old_time, 128);
    cj_ring_write(&ring, old_time, in, 128);
    fill(in, new_time, 128);
    cj_ring_write(&ring, new_time, in, 128);
    poison(out, 128);
    cj_ring_read(&ring, old_time, out, 128);
    for (uint32_t i = 0; i < 128; i++) CHECK(out[i] == 0.0f);
    poison(out, 128);
    cj_ring_read(&ring, new_time, out, 128);
    for (uint32_t i = 0; i < 128; i++) CHECK(out[i] == value_at(new_time + i));
}

static void write_across_wrap_round_trips(void) {
    float in[512], out[512];
    /* Straddles slot CJ_RING_FRAMES - 1 / slot 0, a few laps in. */
    const uint64_t start = 3 * (uint64_t)CJ_RING_FRAMES - 200;
    cj_ring_reset(&ring);
    fill(in, start, 512);
    cj_ring_write(&ring, start, in, 512);
    poison(out, 512);
    cj_ring_read(&ring, start, out, 512);
    for (uint32_t i = 0; i < 512; i++) CHECK(out[i] == in[i]);
}

static void reset_reads_all_zero(void) {
    static float in[CJ_RING_FRAMES], out[CJ_RING_FRAMES];
    cj_ring_reset(&ring);
    fill(in, 0, CJ_RING_FRAMES);
    cj_ring_write(&ring, 0, in, CJ_RING_FRAMES);
    cj_ring_reset(&ring);
    poison(out, CJ_RING_FRAMES);
    cj_ring_read(&ring, 0, out, CJ_RING_FRAMES);
    for (uint32_t i = 0; i < CJ_RING_FRAMES; i++) CHECK(out[i] == 0.0f);
}

int main(void) {
    struct {
        const char *name;
        void (*run)(void);
    } cases[] = {
        {"write then read at the same time round-trips", write_then_read_round_trips},
        {"a time never written reads 0", unwritten_time_reads_zero},
        {"a time overwritten one lap later reads 0 for the old time",
         overwritten_one_lap_later_reads_zero_for_old_time},
        {"a write across the wrap point round-trips", write_across_wrap_round_trips},
        {"reset reads as all 0", reset_reads_all_zero},
    };
    const int count = (int)(sizeof cases / sizeof cases[0]);
    int failed = 0;
    for (int i = 0; i < count; i++) {
        int before = failures;
        cases[i].run();
        int ok = failures == before;
        failed += !ok;
        printf("%s: %s\n", ok ? "ok  " : "FAIL", cases[i].name);
    }
    printf("ring_test: %d/%d passed\n", count - failed, count);
    return failed ? 1 : 0;
}
