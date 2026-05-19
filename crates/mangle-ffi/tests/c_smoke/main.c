/* C smoke test for mangle-ffi.
 *
 * Compiled by build.rs via `cc::Build` into a static archive that the Rust
 * integration test (`tests/c_smoke.rs`) links against. The point of going
 * through a real C compiler — rather than declaring extern "C" signatures
 * in Rust — is to verify that the cbindgen-generated header is consistent
 * with the actual exported symbols. If a function signature drifts, this
 * file won't compile.
 *
 * Each step that can fail returns a distinct nonzero code so the Rust
 * driver can pinpoint where the smoke test broke.
 */

#include "mangle.h"

#include <stddef.h>
#include <stdint.h>
#include <string.h>

int32_t c_smoke_run(void) {
    /* ---- mangle_version --------------------------------------------- */

    /* 1. mangle_version writes a non-empty buffer. */
    MangleBuffer buf = {0};
    int32_t rc = mangle_version(&buf);
    if (rc != MANGLE_OK) return 10;
    if (buf.data == NULL) return 11;
    if (buf.len == 0) return 12;
    if (buf.cap < buf.len) return 13;

    /* 2. The contents look like a semver-ish string. */
    if (buf.data[0] < '0' || buf.data[0] > '9') {
        mangle_buffer_free(&buf);
        return 14;
    }
    if (memchr(buf.data, '.', buf.len) == NULL) {
        mangle_buffer_free(&buf);
        return 15;
    }

    /* 3. mangle_buffer_free zeroes the struct, is idempotent, and
     *    tolerates NULL. */
    mangle_buffer_free(&buf);
    if (buf.data != NULL || buf.len != 0 || buf.cap != 0) return 16;
    mangle_buffer_free(&buf);
    mangle_buffer_free(NULL);

    /* 4. Null-out behavior on mangle_version. */
    rc = mangle_version(NULL);
    if (rc == MANGLE_OK) return 17;
    if (rc != MANGLE_ERR_INVALID_ARG) return 18;

    /* ---- mangle_last_error after a failing call -------------------- */

    /* 5. last_error after a failure populates the buffer. */
    MangleBuffer errbuf = {0};
    rc = mangle_last_error(&errbuf);
    if (rc != MANGLE_OK) return 20;
    if (errbuf.data == NULL || errbuf.len == 0) return 21;
    mangle_buffer_free(&errbuf);

    /* 6. last_error has take semantics: a second call returns empty. */
    MangleBuffer errbuf2 = {0};
    rc = mangle_last_error(&errbuf2);
    if (rc != MANGLE_OK) return 22;
    if (errbuf2.len != 0) return 23;
    mangle_buffer_free(&errbuf2);

    /* 7. last_error with NULL out returns invalid arg. */
    rc = mangle_last_error(NULL);
    if (rc != MANGLE_ERR_INVALID_ARG) return 24;
    /* That call itself set an error; drain it. */
    MangleBuffer drain = {0};
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* ---- mangle_engine_new / mangle_engine_free -------------------- */

    /* 8. Construct an engine with provenance disabled. */
    MangleEngine* eng = NULL;
    rc = mangle_engine_new(0, &eng);
    if (rc != MANGLE_OK) return 30;
    if (eng == NULL) return 31;
    mangle_engine_free(eng);

    /* 9. last_error after a successful call is empty. */
    MangleBuffer post_ok = {0};
    mangle_last_error(&post_ok);
    if (post_ok.len != 0) {
        mangle_buffer_free(&post_ok);
        return 32;
    }
    mangle_buffer_free(&post_ok);

    /* 10. Construct with provenance enabled and free. */
    rc = mangle_engine_new(1, &eng);
    if (rc != MANGLE_OK) return 33;
    if (eng == NULL) return 34;
    mangle_engine_free(eng);

    /* 11. engine_new with null out returns invalid arg. */
    rc = mangle_engine_new(0, NULL);
    if (rc != MANGLE_ERR_INVALID_ARG) return 35;
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 12. engine_free on NULL is a no-op (must not crash). */
    mangle_engine_free(NULL);

    /* 13. Multiple engines coexist. */
    MangleEngine* eng_a = NULL;
    MangleEngine* eng_b = NULL;
    if (mangle_engine_new(0, &eng_a) != MANGLE_OK) return 36;
    if (mangle_engine_new(0, &eng_b) != MANGLE_OK) return 37;
    if (eng_a == eng_b) return 38;
    mangle_engine_free(eng_a);
    mangle_engine_free(eng_b);

    /* ---- mangle_load_rules ----------------------------------------- */

    /* 14. Load a small valid program. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 40;
    const char* src1 = "edge(1, 2).\nedge(2, 3).\nreachable(X,Y) :- edge(X,Y).\n";
    const uint8_t* sources1[1] = { (const uint8_t*)src1 };
    size_t lens1[1] = { strlen(src1) };
    rc = mangle_load_rules(eng, sources1, lens1, 1);
    if (rc != MANGLE_OK) {
        /* Drain the error so it doesn't bleed into the next step. */
        mangle_last_error(&drain);
        mangle_buffer_free(&drain);
        mangle_engine_free(eng);
        return 41;
    }

    /* 15. Reload (engine should accept it). */
    rc = mangle_load_rules(eng, sources1, lens1, 1);
    if (rc != MANGLE_OK) {
        mangle_engine_free(eng);
        return 42;
    }

    /* 16. Parse error → MANGLE_ERR_PARSE + non-empty last_error. */
    const char* bad = "@@@ this is not mangle @@@";
    const uint8_t* sources_bad[1] = { (const uint8_t*)bad };
    size_t lens_bad[1] = { strlen(bad) };
    rc = mangle_load_rules(eng, sources_bad, lens_bad, 1);
    if (rc != MANGLE_ERR_PARSE) {
        mangle_engine_free(eng);
        return 43;
    }
    MangleBuffer err_load = {0};
    mangle_last_error(&err_load);
    if (err_load.len == 0) {
        mangle_buffer_free(&err_load);
        mangle_engine_free(eng);
        return 44;
    }
    mangle_buffer_free(&err_load);

    /* 17. null engine → MANGLE_ERR_INVALID_ARG. */
    rc = mangle_load_rules(NULL, sources1, lens1, 1);
    if (rc != MANGLE_ERR_INVALID_ARG) {
        mangle_engine_free(eng);
        return 45;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 18. Zero sources → MANGLE_ERR_INVALID_ARG. */
    rc = mangle_load_rules(eng, NULL, NULL, 0);
    if (rc != MANGLE_ERR_INVALID_ARG) {
        mangle_engine_free(eng);
        return 46;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    mangle_engine_free(eng);

    return 0;
}
