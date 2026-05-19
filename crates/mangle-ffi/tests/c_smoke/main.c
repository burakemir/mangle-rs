/* C smoke test for mangle-ffi.
 *
 * Compiled by build.rs via `cc::Build` into a static archive that the Rust
 * integration test (`tests/c_smoke.rs`) links against. The point of going
 * through a real C compiler — rather than declaring extern "C" signatures
 * in Rust — is to verify that the cbindgen-generated header is consistent
 * with the actual exported symbols. If a function signature drifts, this
 * file won't compile.
 */

#include "mangle.h"

#include <stddef.h>
#include <stdint.h>
#include <string.h>

/* Returns 0 on success; nonzero on any check failure (with the failure code
 * indicating which step failed, for diagnostic clarity from the Rust side).
 */
int32_t c_smoke_run(void) {
    /* 1. mangle_version writes a non-empty buffer. */
    MangleBuffer buf = {0};
    int32_t rc = mangle_version(&buf);
    if (rc != 0) {
        return 10;
    }
    if (buf.data == NULL) {
        return 11;
    }
    if (buf.len == 0) {
        return 12;
    }
    if (buf.cap < buf.len) {
        return 13;
    }

    /* 2. The contents look like a semver-ish string (at minimum: starts
     *    with a digit and contains at least one '.'). We don't pin the
     *    exact version here so version bumps don't churn the test. */
    if (buf.data[0] < '0' || buf.data[0] > '9') {
        mangle_buffer_free(&buf);
        return 14;
    }
    if (memchr(buf.data, '.', buf.len) == NULL) {
        mangle_buffer_free(&buf);
        return 15;
    }

    /* 3. mangle_buffer_free zeroes the struct. */
    mangle_buffer_free(&buf);
    if (buf.data != NULL || buf.len != 0 || buf.cap != 0) {
        return 16;
    }

    /* 4. Double-free on the now-zeroed struct is a no-op. */
    mangle_buffer_free(&buf);

    /* 5. Freeing NULL is a no-op. */
    mangle_buffer_free(NULL);

    /* 6. Null-out behavior on mangle_version. */
    rc = mangle_version(NULL);
    if (rc == 0) {
        return 17;
    }

    return 0;
}
