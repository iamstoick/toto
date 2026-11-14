/* Toto FFI smoke-test C file (tens of lines, written for this test).
 *
 * Exercises: plain call, byte read through a caller pointer, bounded
 * copy with NUL termination, invalid-UTF-8 source, a static callback,
 * and an error-code + last-error slot. The C side never allocates,
 * never retains pointers, and never calls back into Rust except
 * through the one function pointer it is given.
 */
#include <stddef.h>
#include <stdint.h>
#include <string.h>

int32_t ffi_add(int32_t a, int32_t b) {
    return a + b;
}

/* Read through a caller-owned pointer. The caller guarantees the
 * range is alive (rooted on the Rust side) for the call duration. */
uint8_t ffi_read_first(const uint8_t *p, size_t len) {
    if (p == NULL || len == 0) {
        return 0;
    }
    return p[0];
}

/* Bounded copy + NUL termination. Returns bytes written excluding NUL.
 * Never writes past out_cap; out_cap == 0 writes nothing. */
size_t ffi_copy_greet(const char *in, size_t in_len, char *out, size_t out_cap) {
    size_t n;
    if (out == NULL || out_cap == 0) {
        return 0;
    }
    n = in_len < out_cap - 1 ? in_len : out_cap - 1;
    if (n > 0 && in != NULL) {
        memcpy(out, in, n);
    }
    out[n] = '\0';
    return n;
}

/* deliberately invalid UTF-8 (0xFF, 0xFE are never valid) */
static const uint8_t kBadBytes[] = { 0xFF, 0xFE, 'x' };

const uint8_t *ffi_bad_bytes(size_t *out_len) {
    if (out_len != NULL) {
        *out_len = sizeof(kBadBytes);
    }
    return kBadBytes;
}

typedef void (*event_cb)(int32_t code, const char *msg);

void ffi_emit_all(event_cb cb) {
    if (cb == NULL) {
        return;
    }
    cb(1, "start");
    cb(2, "stop");
}

static char g_last_error[128] = { 0 };

int32_t ffi_fallible(int32_t x) {
    if (x < 0) {
        strncpy(g_last_error, "negative input", sizeof(g_last_error) - 1);
        g_last_error[sizeof(g_last_error) - 1] = '\0';
        return -1;
    }
    g_last_error[0] = '\0';
    return x * 2;
}

const char *ffi_last_error(void) {
    return g_last_error;
}
