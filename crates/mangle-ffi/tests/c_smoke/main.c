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

/* Portable byte-buffer substring search. `memmem` is a GNU/BSD
 * extension and is not available on Windows MSVC, so the smoke test
 * uses this drop-in replacement with the same signature/semantics
 * (returns a pointer to the first occurrence, or NULL). */
static const void* c_memmem(const void* hay, size_t hlen,
                            const void* needle, size_t nlen) {
    const uint8_t* h = (const uint8_t*)hay;
    const uint8_t* n = (const uint8_t*)needle;
    if (nlen == 0) return hay;
    if (hlen < nlen) return NULL;
    for (size_t i = 0; i + nlen <= hlen; i++) {
        if (memcmp(h + i, n, nlen) == 0) {
            return h + i;
        }
    }
    return NULL;
}

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

    /* ---- mangle_val_builder + value accessors ---------------------- */

    /* 19. New builder, build each scalar kind, read back. */
    MangleValBuilder* vb = mangle_val_builder_new();
    if (vb == NULL) return 50;

    const MangleVal* vnull = mangle_val_build_null(vb);
    if (vnull == NULL || mangle_val_kind(vnull) != MANGLE_VAL_NULL) {
        mangle_val_builder_free(vb);
        return 51;
    }

    const MangleVal* vnum = mangle_val_build_i64(vb, 42);
    int64_t i_out = 0;
    if (mangle_val_kind(vnum) != MANGLE_VAL_NUMBER
        || mangle_val_as_i64(vnum, &i_out) != MANGLE_OK
        || i_out != 42) {
        mangle_val_builder_free(vb);
        return 52;
    }

    const MangleVal* vflt = mangle_val_build_f64(vb, 1.5);
    double f_out = 0.0;
    if (mangle_val_kind(vflt) != MANGLE_VAL_FLOAT
        || mangle_val_as_f64(vflt, &f_out) != MANGLE_OK
        || f_out != 1.5) {
        mangle_val_builder_free(vb);
        return 53;
    }

    const char* str = "hi";
    const MangleVal* vstr = mangle_val_build_string(vb, (const uint8_t*)str, strlen(str));
    MangleBuffer sbuf = {0};
    if (mangle_val_kind(vstr) != MANGLE_VAL_STRING
        || mangle_val_as_str(vstr, &sbuf) != MANGLE_OK
        || sbuf.len != 2
        || sbuf.data[0] != 'h' || sbuf.data[1] != 'i') {
        mangle_buffer_free(&sbuf);
        mangle_val_builder_free(vb);
        return 54;
    }
    mangle_buffer_free(&sbuf);

    /* 20. Name validation: requires leading '/'. */
    const char* bad_name = "admin";
    const MangleVal* vnbad = mangle_val_build_name(vb, (const uint8_t*)bad_name, strlen(bad_name));
    if (vnbad != NULL) {
        mangle_val_builder_free(vb);
        return 55;
    }
    /* Drain the error from the failed name. */
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    const char* good_name = "/admin";
    const MangleVal* vname = mangle_val_build_name(vb, (const uint8_t*)good_name, strlen(good_name));
    if (vname == NULL || mangle_val_kind(vname) != MANGLE_VAL_NAME) {
        mangle_val_builder_free(vb);
        return 56;
    }

    /* 21. Compound list [1, 2, 3] and walk. */
    const MangleVal* one = mangle_val_build_i64(vb, 1);
    const MangleVal* two = mangle_val_build_i64(vb, 2);
    const MangleVal* three = mangle_val_build_i64(vb, 3);
    const MangleVal* elems[3] = { one, two, three };
    const MangleVal* list = mangle_val_build_compound(vb, MANGLE_COMPOUND_LIST, elems, 3);
    int32_t subkind = -1;
    size_t clen = 0;
    if (list == NULL
        || mangle_val_kind(list) != MANGLE_VAL_COMPOUND
        || mangle_val_compound_kind(list, &subkind) != MANGLE_OK
        || subkind != MANGLE_COMPOUND_LIST
        || mangle_val_compound_len(list, &clen) != MANGLE_OK
        || clen != 3) {
        mangle_val_builder_free(vb);
        return 57;
    }
    for (size_t i = 0; i < 3; i++) {
        const MangleVal* el = mangle_val_compound_get(list, i);
        int64_t n = 0;
        if (el == NULL || mangle_val_as_i64(el, &n) != MANGLE_OK || n != (int64_t)(i + 1)) {
            mangle_val_builder_free(vb);
            return 58;
        }
    }

    /* 22. Out-of-range compound_get returns NULL. */
    if (mangle_val_compound_get(list, 99) != NULL) {
        mangle_val_builder_free(vb);
        return 59;
    }

    /* 23. mangle_val_kind(NULL) returns -1. */
    if (mangle_val_kind(NULL) != -1) {
        mangle_val_builder_free(vb);
        return 60;
    }

    mangle_val_builder_free(vb);
    mangle_val_builder_free(NULL);

    /* ---- mangle_query + cursor ------------------------------------ */

    /* 24. Open engine, load rules, query, iterate. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 70;
    const char* edges =
        "edge(1, 2).\n"
        "edge(2, 3).\n"
        "edge(3, 4).\n";
    const uint8_t* edge_sources[1] = { (const uint8_t*)edges };
    size_t edge_lens[1] = { strlen(edges) };
    if (mangle_load_rules(eng, edge_sources, edge_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 71;
    }

    const char* q = "edge";
    MangleCursor* cur = NULL;
    if (mangle_query(eng, (const uint8_t*)q, strlen(q), &cur) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 72;
    }
    if (cur == NULL) {
        mangle_engine_free(eng);
        return 73;
    }

    /* Iterate; expect 3 rows of arity 2 with i64 columns. */
    int rows = 0;
    for (;;) {
        int32_t rcn = mangle_cursor_next(cur);
        if (rcn == 1) break;
        if (rcn != MANGLE_OK) {
            mangle_cursor_free(cur);
            mangle_engine_free(eng);
            return 74;
        }
        if (mangle_cursor_arity(cur) != 2) {
            mangle_cursor_free(cur);
            mangle_engine_free(eng);
            return 75;
        }
        const MangleVal* c0 = mangle_cursor_col(cur, 0);
        const MangleVal* c1 = mangle_cursor_col(cur, 1);
        if (c0 == NULL || c1 == NULL) {
            mangle_cursor_free(cur);
            mangle_engine_free(eng);
            return 76;
        }
        if (mangle_val_kind(c0) != MANGLE_VAL_NUMBER
            || mangle_val_kind(c1) != MANGLE_VAL_NUMBER) {
            mangle_cursor_free(cur);
            mangle_engine_free(eng);
            return 77;
        }
        rows++;
    }
    if (rows != 3) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 78;
    }

    /* End-of-stream is sticky. */
    if (mangle_cursor_next(cur) != 1) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 79;
    }

    /* cursor_col after end-of-stream returns NULL. */
    if (mangle_cursor_col(cur, 0) != NULL) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 80;
    }

    mangle_cursor_free(cur);

    /* 25. Query with engine that has no rules → MANGLE_ERR_NO_RULES. */
    MangleEngine* eng2 = NULL;
    mangle_engine_new(0, &eng2);
    MangleCursor* cur2 = NULL;
    int32_t rc_nr = mangle_query(eng2, (const uint8_t*)q, strlen(q), &cur2);
    if (rc_nr != MANGLE_ERR_NO_RULES) {
        mangle_engine_free(eng2);
        mangle_engine_free(eng);
        return 81;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_engine_free(eng2);

    /* 26. Reload invalidates the cursor. */
    if (mangle_query(eng, (const uint8_t*)q, strlen(q), &cur) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 82;
    }
    /* Read one row (warms up the cursor). */
    if (mangle_cursor_next(cur) != MANGLE_OK) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 83;
    }
    /* Reload — bumps the generation. */
    if (mangle_load_rules(eng, edge_sources, edge_lens, 1) != MANGLE_OK) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 84;
    }
    /* Next call sees the invalidation. */
    if (mangle_cursor_next(cur) != MANGLE_ERR_CURSOR_INVALIDATED) {
        mangle_cursor_free(cur);
        mangle_engine_free(eng);
        return 85;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_cursor_free(cur);

    /* 27. cursor_free(NULL) and cursor accessors on NULL. */
    mangle_cursor_free(NULL);
    if (mangle_cursor_arity(NULL) != -1) {
        mangle_engine_free(eng);
        return 86;
    }
    if (mangle_cursor_col(NULL, 0) != NULL) {
        mangle_engine_free(eng);
        return 87;
    }

    mangle_engine_free(eng);

    /* ---- mangle_insert_fact / mangle_retract_fact ----------------- */

    /* 28. Insert + query sees new tuple; retract + query doesn't. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 90;
    if (mangle_load_rules(eng, edge_sources, edge_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 91;
    }

    MangleValBuilder* ib = mangle_val_builder_new();
    const MangleVal* v1 = mangle_val_build_i64(ib, 7);
    const MangleVal* v2 = mangle_val_build_i64(ib, 8);
    const MangleVal* tuple[2] = { v1, v2 };
    int32_t added = -1;
    int32_t rci = mangle_insert_fact(
        eng, (const uint8_t*)"edge", 4, tuple, 2, &added);
    if (rci != MANGLE_OK || added != 1) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 92;
    }

    /* Duplicate insert reports added=0. */
    rci = mangle_insert_fact(
        eng, (const uint8_t*)"edge", 4, tuple, 2, &added);
    if (rci != MANGLE_OK || added != 0) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 93;
    }

    /* Fresh cursor sees 4 edges now (3 original + 1 inserted). */
    MangleCursor* cur3 = NULL;
    if (mangle_query(eng, (const uint8_t*)"edge", 4, &cur3) != MANGLE_OK) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 94;
    }
    int n_edges = 0;
    while (mangle_cursor_next(cur3) == MANGLE_OK) n_edges++;
    mangle_cursor_free(cur3);
    if (n_edges != 4) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 95;
    }

    /* Retract the inserted edge. */
    int32_t found = -1;
    int32_t rcr = mangle_retract_fact(
        eng, (const uint8_t*)"edge", 4, tuple, 2, &found);
    if (rcr != MANGLE_OK || found != 1) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 96;
    }

    /* Retract missing tuple reports found=0. */
    rcr = mangle_retract_fact(
        eng, (const uint8_t*)"edge", 4, tuple, 2, &found);
    if (rcr != MANGLE_OK || found != 0) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 97;
    }

    /* Cursor now sees 3 edges again. */
    if (mangle_query(eng, (const uint8_t*)"edge", 4, &cur3) != MANGLE_OK) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 98;
    }
    n_edges = 0;
    while (mangle_cursor_next(cur3) == MANGLE_OK) n_edges++;
    mangle_cursor_free(cur3);
    if (n_edges != 3) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 99;
    }

    mangle_val_builder_free(ib);

    /* 29. insert with NULL added_out is allowed. */
    ib = mangle_val_builder_new();
    v1 = mangle_val_build_i64(ib, 9);
    v2 = mangle_val_build_i64(ib, 10);
    const MangleVal* tuple2[2] = { v1, v2 };
    rci = mangle_insert_fact(
        eng, (const uint8_t*)"edge", 4, tuple2, 2, NULL);
    if (rci != MANGLE_OK) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng);
        return 100;
    }
    mangle_val_builder_free(ib);

    /* 30. insert into engine with no rules → MANGLE_ERR_NO_RULES. */
    MangleEngine* eng3 = NULL;
    mangle_engine_new(0, &eng3);
    ib = mangle_val_builder_new();
    v1 = mangle_val_build_i64(ib, 1);
    v2 = mangle_val_build_i64(ib, 2);
    const MangleVal* tuple3[2] = { v1, v2 };
    rci = mangle_insert_fact(
        eng3, (const uint8_t*)"edge", 4, tuple3, 2, NULL);
    if (rci != MANGLE_ERR_NO_RULES) {
        mangle_val_builder_free(ib);
        mangle_engine_free(eng3);
        mangle_engine_free(eng);
        return 101;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_val_builder_free(ib);
    mangle_engine_free(eng3);

    mangle_engine_free(eng);

    /* ---- mangle_load_facts_mgr ------------------------------------ */

    /* 31. Load a small uncompressed SimpleRow blob. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 110;
    /* Declare the relation via rules first. */
    const char* decl = "edge(0, 0).";
    const uint8_t* decl_src[1] = { (const uint8_t*)decl };
    size_t decl_lens[1] = { strlen(decl) };
    if (mangle_load_rules(eng, decl_src, decl_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 111;
    }

    /* Build a minimal SimpleRow blob in-line:
     *   1 predicate
     *   edge 2 2  (name arity num_facts)
     *   edge(7, 8).
     *   edge(9, 10).
     */
    const char* mgr_blob =
        "1\n"
        "edge 2 2\n"
        "edge(7, 8).\n"
        "edge(9, 10).\n";
    const char* mgr_name = "inline.mgr";
    size_t n_inserted = 0;
    int32_t rc_load = mangle_load_facts_mgr(
        eng,
        (const uint8_t*)mgr_blob,
        strlen(mgr_blob),
        (const uint8_t*)mgr_name,
        strlen(mgr_name),
        &n_inserted);
    if (rc_load != MANGLE_OK) {
        /* Drain & report the load error. */
        mangle_last_error(&drain);
        mangle_buffer_free(&drain);
        mangle_engine_free(eng);
        return 112;
    }
    if (n_inserted != 2) {
        mangle_engine_free(eng);
        return 113;
    }

    /* Query to confirm — 1 baseline + 2 loaded = 3 edges. */
    MangleCursor* cur4 = NULL;
    if (mangle_query(eng, (const uint8_t*)"edge", 4, &cur4) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 114;
    }
    int n_edges_2 = 0;
    while (mangle_cursor_next(cur4) == MANGLE_OK) n_edges_2++;
    mangle_cursor_free(cur4);
    if (n_edges_2 != 3) {
        mangle_engine_free(eng);
        return 115;
    }

    /* 32. Empty SimpleRow (header only) loads zero tuples. */
    const char* empty_blob = "0\n";
    rc_load = mangle_load_facts_mgr(
        eng,
        (const uint8_t*)empty_blob,
        strlen(empty_blob),
        NULL, 0,
        &n_inserted);
    if (rc_load != MANGLE_OK || n_inserted != 0) {
        mangle_engine_free(eng);
        return 116;
    }

    /* 33. Garbage payload returns MANGLE_ERR_PARSE. */
    const char* garbage = "99\nbroken header\n";
    rc_load = mangle_load_facts_mgr(
        eng,
        (const uint8_t*)garbage,
        strlen(garbage),
        NULL, 0,
        NULL);
    if (rc_load != MANGLE_ERR_PARSE) {
        mangle_last_error(&drain);
        mangle_buffer_free(&drain);
        mangle_engine_free(eng);
        return 117;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 34. NULL bytes with nonzero len → INVALID_ARG. */
    rc_load = mangle_load_facts_mgr(eng, NULL, 10, NULL, 0, NULL);
    if (rc_load != MANGLE_ERR_INVALID_ARG) {
        mangle_engine_free(eng);
        return 118;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 35. NULL n_inserted_out is accepted. */
    rc_load = mangle_load_facts_mgr(
        eng,
        (const uint8_t*)empty_blob,
        strlen(empty_blob),
        NULL, 0,
        NULL);
    if (rc_load != MANGLE_OK) {
        mangle_engine_free(eng);
        return 119;
    }

    mangle_engine_free(eng);

    /* ---- mangle_save_facts_mgr / mangle_save_relation_mgr / ----- */
    /* ---- mangle_query_dump_mgr ---------------------------------- */

    /* 36. Save-and-reload round-trip: save all → reload → query. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 130;
    const char* sr_src =
        "edge(1, 2).\n"
        "edge(2, 3).\n"
        "route(\"GET\", \"/\").\n"
        "route(\"POST\", \"/login\").\n";
    const uint8_t* sr_sources[1] = { (const uint8_t*)sr_src };
    size_t sr_lens[1] = { strlen(sr_src) };
    if (mangle_load_rules(eng, sr_sources, sr_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 131;
    }

    MangleBuffer saved = {0};
    int32_t rc_save = mangle_save_facts_mgr(eng, MANGLE_COMPRESSION_NONE, &saved);
    if (rc_save != MANGLE_OK) {
        mangle_engine_free(eng);
        return 132;
    }
    if (saved.data == NULL || saved.len == 0) {
        mangle_buffer_free(&saved);
        mangle_engine_free(eng);
        return 133;
    }

    /* Reload into a fresh engine. */
    MangleEngine* eng4 = NULL;
    if (mangle_engine_new(0, &eng4) != MANGLE_OK) {
        mangle_buffer_free(&saved);
        mangle_engine_free(eng);
        return 134;
    }
    const char* baseline = "edge(0, 0). route(\"X\", \"X\").";
    const uint8_t* baseline_src[1] = { (const uint8_t*)baseline };
    size_t baseline_lens[1] = { strlen(baseline) };
    if (mangle_load_rules(eng4, baseline_src, baseline_lens, 1) != MANGLE_OK) {
        mangle_buffer_free(&saved);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 135;
    }
    size_t n_loaded = 0;
    int32_t rc_load2 = mangle_load_facts_mgr(
        eng4, saved.data, saved.len,
        (const uint8_t*)"saved.mgr", 9,
        &n_loaded);
    mangle_buffer_free(&saved);
    if (rc_load2 != MANGLE_OK || n_loaded != 4) {
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 136;
    }

    /* 37. Gzip save produces a smaller buffer (sometimes) but always */
    /* starts with the gzip magic bytes 1F 8B. */
    MangleBuffer gz = {0};
    if (mangle_save_facts_mgr(eng, MANGLE_COMPRESSION_GZIP, &gz) != MANGLE_OK) {
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 137;
    }
    if (gz.len < 2 || gz.data[0] != 0x1f || gz.data[1] != 0x8b) {
        mangle_buffer_free(&gz);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 138;
    }
    mangle_buffer_free(&gz);

    /* 38. save_relation_mgr round-trip for a single named relation. */
    MangleBuffer rel = {0};
    int32_t rc_rel = mangle_save_relation_mgr(
        eng, (const uint8_t*)"edge", 4,
        MANGLE_COMPRESSION_NONE, &rel);
    if (rc_rel != MANGLE_OK) {
        mangle_buffer_free(&rel);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 139;
    }
    if (rel.data == NULL || rel.len == 0) {
        mangle_buffer_free(&rel);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 140;
    }
    mangle_buffer_free(&rel);

    /* 39. query_dump_mgr produces a renamed blob. */
    MangleBuffer dump = {0};
    int32_t rc_dump = mangle_query_dump_mgr(
        eng,
        (const uint8_t*)"route(\"GET\", X)", strlen("route(\"GET\", X)"),
        (const uint8_t*)"get_routes", 10,
        MANGLE_COMPRESSION_NONE,
        &dump);
    if (rc_dump != MANGLE_OK) {
        mangle_buffer_free(&dump);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 141;
    }
    /* Output begins with "1\n" (one predicate) and contains the new name. */
    if (dump.len < 3 || dump.data[0] != '1' || dump.data[1] != '\n') {
        mangle_buffer_free(&dump);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 142;
    }
    if (c_memmem(dump.data, dump.len, "get_routes", 10) == NULL) {
        mangle_buffer_free(&dump);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 143;
    }
    mangle_buffer_free(&dump);

    /* 40. zstd compression on write is not supported → INVALID_ARG. */
    MangleBuffer zstd_attempt = {0};
    int32_t rc_zstd = mangle_save_facts_mgr(eng, MANGLE_COMPRESSION_ZSTD, &zstd_attempt);
    if (rc_zstd != MANGLE_ERR_INVALID_ARG) {
        mangle_buffer_free(&zstd_attempt);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 144;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 41. query_dump_mgr with empty out_relation → INVALID_ARG. */
    MangleBuffer empty_dump = {0};
    int32_t rc_qd = mangle_query_dump_mgr(
        eng,
        (const uint8_t*)"edge", 4,
        (const uint8_t*)"", 0,
        MANGLE_COMPRESSION_NONE,
        &empty_dump);
    if (rc_qd != MANGLE_ERR_INVALID_ARG) {
        mangle_buffer_free(&empty_dump);
        mangle_engine_free(eng);
        mangle_engine_free(eng4);
        return 145;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    mangle_engine_free(eng);
    mangle_engine_free(eng4);

    /* ---- mangle_schema_snapshot / mangle_relation_names + */
    /*      unknown-relation errors (M8) ----------------------------- */

    /* 42. Schema snapshot of a small program produces non-empty JSON. */
    if (mangle_engine_new(0, &eng) != MANGLE_OK) return 150;
    const char* m8_src =
        "edge(1, 2).\n"
        "reachable(X, Y) :- edge(X, Y).\n";
    const uint8_t* m8_sources[1] = { (const uint8_t*)m8_src };
    size_t m8_lens[1] = { strlen(m8_src) };
    if (mangle_load_rules(eng, m8_sources, m8_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 151;
    }
    MangleBuffer schema_buf = {0};
    if (mangle_schema_snapshot(eng, &schema_buf) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 152;
    }
    if (schema_buf.len == 0
        || c_memmem(schema_buf.data, schema_buf.len, "\"edge\"", 6) == NULL
        || c_memmem(schema_buf.data, schema_buf.len, "\"reachable\"", 11) == NULL) {
        mangle_buffer_free(&schema_buf);
        mangle_engine_free(eng);
        return 153;
    }
    mangle_buffer_free(&schema_buf);

    /* 43. relation_names returns a JSON array containing both names. */
    MangleBuffer names_buf = {0};
    if (mangle_relation_names(eng, &names_buf) != MANGLE_OK) {
        mangle_engine_free(eng);
        return 154;
    }
    if (names_buf.len < 2 || names_buf.data[0] != '['
        || c_memmem(names_buf.data, names_buf.len, "edge", 4) == NULL
        || c_memmem(names_buf.data, names_buf.len, "reachable", 9) == NULL) {
        mangle_buffer_free(&names_buf);
        mangle_engine_free(eng);
        return 155;
    }
    mangle_buffer_free(&names_buf);

    /* 44. query() on unknown relation returns MANGLE_ERR_UNKNOWN_RELATION. */
    MangleCursor* cur_un = NULL;
    int32_t rc_un =
        mangle_query(eng, (const uint8_t*)"nope", 4, &cur_un);
    if (rc_un != MANGLE_ERR_UNKNOWN_RELATION) {
        mangle_cursor_free(cur_un);
        mangle_engine_free(eng);
        return 156;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);

    /* 45. schema_snapshot on engine with no rules returns NO_RULES. */
    MangleEngine* eng_empty = NULL;
    mangle_engine_new(0, &eng_empty);
    MangleBuffer empty_schema = {0};
    int32_t rc_es = mangle_schema_snapshot(eng_empty, &empty_schema);
    if (rc_es != MANGLE_ERR_NO_RULES) {
        mangle_buffer_free(&empty_schema);
        mangle_engine_free(eng_empty);
        mangle_engine_free(eng);
        return 157;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_engine_free(eng_empty);

    mangle_engine_free(eng);

    /* ---- mangle_derivation_tree (M9) ----------------------------- */

    /* 46. Engine with provenance enabled returns a derivation tree
     *     for a derived fact. */
    MangleEngine* eng_prov = NULL;
    if (mangle_engine_new(1, &eng_prov) != MANGLE_OK) return 170;
    const char* m9_src =
        "edge(1, 2).\n"
        "edge(2, 3).\n"
        "reachable(X, Y) :- edge(X, Y).\n"
        "reachable(X, Z) :- edge(X, Y), reachable(Y, Z).\n";
    const uint8_t* m9_sources[1] = { (const uint8_t*)m9_src };
    size_t m9_lens[1] = { strlen(m9_src) };
    if (mangle_load_rules(eng_prov, m9_sources, m9_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng_prov);
        return 171;
    }
    MangleBuffer tree_buf = {0};
    int32_t rc_tree = mangle_derivation_tree(
        eng_prov,
        (const uint8_t*)"reachable(1, 3)", strlen("reachable(1, 3)"),
        100,
        &tree_buf);
    if (rc_tree != MANGLE_OK) {
        mangle_buffer_free(&tree_buf);
        mangle_engine_free(eng_prov);
        return 172;
    }
    /* The JSON should mention the queried relation name. */
    if (c_memmem(tree_buf.data, tree_buf.len, "reachable", 9) == NULL) {
        mangle_buffer_free(&tree_buf);
        mangle_engine_free(eng_prov);
        return 173;
    }
    mangle_buffer_free(&tree_buf);

    /* 47. Engine without provenance → MANGLE_ERR_NO_PROVENANCE. */
    MangleEngine* eng_noprov = NULL;
    mangle_engine_new(0, &eng_noprov);
    if (mangle_load_rules(eng_noprov, m9_sources, m9_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng_noprov);
        mangle_engine_free(eng_prov);
        return 174;
    }
    MangleBuffer noprov_buf = {0};
    int32_t rc_noprov = mangle_derivation_tree(
        eng_noprov,
        (const uint8_t*)"reachable(1, 3)", strlen("reachable(1, 3)"),
        100,
        &noprov_buf);
    if (rc_noprov != MANGLE_ERR_NO_PROVENANCE) {
        mangle_buffer_free(&noprov_buf);
        mangle_engine_free(eng_noprov);
        mangle_engine_free(eng_prov);
        return 175;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_buffer_free(&noprov_buf);
    mangle_engine_free(eng_noprov);

    /* 48. Variable in fact → MANGLE_ERR_PARSE. */
    MangleBuffer pe_buf = {0};
    int32_t rc_pe = mangle_derivation_tree(
        eng_prov,
        (const uint8_t*)"reachable(X, 3)", strlen("reachable(X, 3)"),
        100,
        &pe_buf);
    if (rc_pe != MANGLE_ERR_PARSE) {
        mangle_buffer_free(&pe_buf);
        mangle_engine_free(eng_prov);
        return 176;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_buffer_free(&pe_buf);

    mangle_engine_free(eng_prov);

    /* ---- mangle_facts_snapshot (M10) ----------------------------- */

    /* 49. Facts snapshot includes declared relations with counts. */
    MangleEngine* eng_snap = NULL;
    if (mangle_engine_new(0, &eng_snap) != MANGLE_OK) return 180;
    const char* m10_src = "edge(1, 2). edge(2, 3). node(1).";
    const uint8_t* m10_sources[1] = { (const uint8_t*)m10_src };
    size_t m10_lens[1] = { strlen(m10_src) };
    if (mangle_load_rules(eng_snap, m10_sources, m10_lens, 1) != MANGLE_OK) {
        mangle_engine_free(eng_snap);
        return 181;
    }

    MangleBuffer facts_buf = {0};
    if (mangle_facts_snapshot(eng_snap, 100, &facts_buf) != MANGLE_OK) {
        mangle_buffer_free(&facts_buf);
        mangle_engine_free(eng_snap);
        return 182;
    }
    /* JSON should mention both relations + the relations container. */
    if (c_memmem(facts_buf.data, facts_buf.len, "\"relations\"", 11) == NULL
        || c_memmem(facts_buf.data, facts_buf.len, "\"edge\"", 6) == NULL
        || c_memmem(facts_buf.data, facts_buf.len, "\"node\"", 6) == NULL
        || c_memmem(facts_buf.data, facts_buf.len, "\"count\"", 7) == NULL) {
        mangle_buffer_free(&facts_buf);
        mangle_engine_free(eng_snap);
        return 183;
    }
    mangle_buffer_free(&facts_buf);

    /* 50. No rules loaded → NO_RULES. */
    MangleEngine* eng_snap_empty = NULL;
    mangle_engine_new(0, &eng_snap_empty);
    MangleBuffer snap_err = {0};
    int32_t rc_se = mangle_facts_snapshot(eng_snap_empty, 10, &snap_err);
    if (rc_se != MANGLE_ERR_NO_RULES) {
        mangle_buffer_free(&snap_err);
        mangle_engine_free(eng_snap_empty);
        mangle_engine_free(eng_snap);
        return 184;
    }
    mangle_last_error(&drain);
    mangle_buffer_free(&drain);
    mangle_engine_free(eng_snap_empty);

    mangle_engine_free(eng_snap);

    return 0;
}
