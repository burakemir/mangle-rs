//! Fact I/O: reading `.mgr` (SimpleRow) bytes into the engine.
//!
//! `mangle_load_facts_mgr` accepts an in-memory byte slice that may be
//! uncompressed SimpleRow, gzipped, or zstd-compressed. Compression
//! magic bytes are sniffed; the decompressed payload is parsed by
//! [`mangle_db::simplerow::read_from_bytes`], and the resulting tuples
//! are bulk-inserted into the engine's `MemStore`.
//!
//! M6 reads only. The write side (`mangle_save_facts_mgr` etc.) lands
//! in M7.

use anyhow::{Context, Result};
use std::io::{Read, Write};

use crate::buffer::{MangleBuffer, write_buffer};
use crate::engine::MangleEngine;
use crate::error::{panic_boundary, set_error_msg};
use crate::query::{filter_tuples, parse_query_lenient};
use crate::{MANGLE_ERR, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK};

/// Compression mode: no compression.
pub const MANGLE_COMPRESSION_NONE: i32 = 0;
/// Compression mode: gzip.
pub const MANGLE_COMPRESSION_GZIP: i32 = 1;
/// Compression mode: zstd. Reserved; **not currently supported on the
/// write side** — `ruzstd` is decode-only and we don't pull in a
/// libzstd dependency just for this. The read side accepts
/// zstd-compressed `.mgr` input via `ruzstd::decoding::StreamingDecoder`,
/// so consumers that only need to *load* zstd blobs continue to work.
pub const MANGLE_COMPRESSION_ZSTD: i32 = 2;

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Encode `bytes` with the requested compression. Returns the bytes
/// verbatim for [`MANGLE_COMPRESSION_NONE`], gzip-compressed for
/// [`MANGLE_COMPRESSION_GZIP`], or an error otherwise (including
/// [`MANGLE_COMPRESSION_ZSTD`] which is not yet supported on write).
fn compress(bytes: Vec<u8>, compression: i32) -> Result<Vec<u8>> {
    match compression {
        MANGLE_COMPRESSION_NONE => Ok(bytes),
        MANGLE_COMPRESSION_GZIP => {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            let mut enc = GzEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&bytes).context("gzip encode")?;
            enc.finish().context("gzip finish")
        }
        MANGLE_COMPRESSION_ZSTD => Err(anyhow::anyhow!(
            "zstd compression is not supported on the write side; \
             use gzip (1) or no compression (0)"
        )),
        n => Err(anyhow::anyhow!("invalid compression mode: {n}")),
    }
}

/// Encode the given tables as SimpleRow and apply optional
/// compression. Shared between save_facts_mgr, save_relation_mgr, and
/// query_dump_mgr.
fn encode_tables(tables: crate::engine::RelationTables, compression: i32) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    mangle_db::simplerow::write_simple_row(&mut buf, &tables).context("write_simple_row")?;
    compress(buf, compression)
}

/// Sniff compression magic bytes and return the decompressed payload.
/// Uncompressed input is returned via a copy (cheap relative to the
/// downstream parse cost).
fn decompress_if_needed(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() >= 2 && bytes[..2] == GZIP_MAGIC {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).context("gzip decode")?;
        Ok(out)
    } else if bytes.len() >= 4 && bytes[..4] == ZSTD_MAGIC {
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes)
            .map_err(|e| anyhow::anyhow!("zstd decoder init: {e}"))?;
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).context("zstd decode")?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Bulk-load facts from a `.mgr` byte slice into the engine.
///
/// `bytes` is a SimpleRow byte stream as produced by
/// `mangle_db::simplerow::write_simple_row` (M7), optionally
/// compressed with gzip or zstd (auto-detected via magic bytes).
///
/// `source_name` is a human-readable identifier (e.g. a file path)
/// included verbatim in error messages. May be empty.
///
/// On success, returns [`MANGLE_OK`] and writes the number of
/// inserted tuples (including duplicates) into `*n_inserted_out` if
/// non-null. The store's scan-visible set is updated atomically;
/// subsequent queries see all loaded tuples. Returns
/// [`MANGLE_ERR_NO_RULES`] when the engine has no program loaded
/// (load rules first), [`MANGLE_ERR_PARSE`] when the byte stream
/// isn't a valid `.mgr`, [`MANGLE_ERR_INVALID_ARG`] for null pointers
/// or invalid UTF-8 in `source_name`, or [`MANGLE_ERR`] for other
/// failures (decompression error, store-level insert failure).
///
/// IDB relations are not re-derived — see [`mangle_insert_fact`] for
/// the same caveat.
///
/// # Safety
/// `engine` must be a live handle. `bytes` must point to `len`
/// readable bytes (or be null with `len == 0`). `source_name` must
/// point to `name_len` readable UTF-8 bytes (or be null with
/// `name_len == 0`). `n_inserted_out` is nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_load_facts_mgr(
    engine: *mut MangleEngine,
    bytes: *const u8,
    len: usize,
    source_name: *const u8,
    name_len: usize,
    n_inserted_out: *mut usize,
) -> i32 {
    panic_boundary!(engine, {
        let payload = if len == 0 {
            &[][..]
        } else if bytes.is_null() {
            set_error_msg("mangle_load_facts_mgr: bytes pointer is null but len > 0");
            return MANGLE_ERR_INVALID_ARG;
        } else {
            // SAFETY: caller's contract.
            unsafe { std::slice::from_raw_parts(bytes, len) }
        };

        let name_slice = if name_len == 0 {
            ""
        } else if source_name.is_null() {
            set_error_msg("mangle_load_facts_mgr: source_name is null but name_len > 0");
            return MANGLE_ERR_INVALID_ARG;
        } else {
            // SAFETY: caller's contract.
            let slice = unsafe { std::slice::from_raw_parts(source_name, name_len) };
            match std::str::from_utf8(slice) {
                Ok(s) => s,
                Err(e) => {
                    set_error_msg(format!(
                        "mangle_load_facts_mgr: source_name is not valid UTF-8: {e}"
                    ));
                    return MANGLE_ERR_INVALID_ARG;
                }
            }
        };

        // SAFETY: non-null and not poisoned per panic_boundary.
        let eng = unsafe { &mut *engine };
        // Decompress + parse outside the engine's ouroboros borrow,
        // then hand the parsed tables off for bulk insert.
        let decoded = match decompress_if_needed(payload) {
            Ok(d) => d,
            Err(e) => {
                set_error_msg(format!("mangle_load_facts_mgr({name_slice}): {e:#}"));
                return MANGLE_ERR;
            }
        };
        let data = match mangle_db::simplerow::read_from_bytes(&decoded) {
            Ok(d) => d,
            Err(e) => {
                set_error_msg(format!("mangle_load_facts_mgr({name_slice}): {e:#}"));
                return MANGLE_ERR_PARSE;
            }
        };
        match eng.bulk_insert_tables(data.tables) {
            Ok(Some(n)) => {
                if !n_inserted_out.is_null() {
                    // SAFETY: caller's contract.
                    unsafe { *n_inserted_out = n };
                }
                MANGLE_OK
            }
            Ok(None) => {
                set_error_msg("mangle_load_facts_mgr: engine has no rules loaded");
                MANGLE_ERR_NO_RULES
            }
            Err(e) => {
                set_error_msg(format!("mangle_load_facts_mgr({name_slice}): {e:#}"));
                MANGLE_ERR
            }
        }
    })
}

// ---- Write side (M7) ---------------------------------------------------

/// Encode every relation in the engine's store as a single SimpleRow
/// `.mgr` blob, optionally compressed.
///
/// Use for "Save Project" — write the resulting buffer to `facts.mgr`
/// (or `facts.mgr.gz` if `compression == MANGLE_COMPRESSION_GZIP`).
///
/// Returns [`MANGLE_OK`] on success; the resulting buffer is owned by
/// the caller and must be freed with [`mangle_buffer_free`]. Returns
/// [`MANGLE_ERR_NO_RULES`] when no program is loaded,
/// [`MANGLE_ERR_INVALID_ARG`] for null `out` or an unsupported
/// compression mode (currently `MANGLE_COMPRESSION_ZSTD`), or
/// [`MANGLE_ERR`] for store-side or encoding failures.
///
/// # Safety
/// `engine` must be a live handle. `out` must be non-null and point to
/// a writable [`MangleBuffer`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_save_facts_mgr(
    engine: *mut MangleEngine,
    compression: i32,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_save_facts_mgr: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        let tables = match eng.all_relations_materialized() {
            Ok(Some(t)) => t,
            Ok(None) => {
                set_error_msg("mangle_save_facts_mgr: engine has no rules loaded");
                return MANGLE_ERR_NO_RULES;
            }
            Err(e) => {
                set_error_msg(format!("mangle_save_facts_mgr: {e:#}"));
                return MANGLE_ERR;
            }
        };
        match encode_tables(tables, compression) {
            Ok(bytes) => {
                // SAFETY: out non-null per the precondition.
                unsafe { write_buffer(out, bytes) };
                MANGLE_OK
            }
            Err(e) => {
                let msg = format!("mangle_save_facts_mgr: {e:#}");
                set_error_msg(msg);
                // Compression-mode validation errors map to invalid arg;
                // everything else is generic. Both share the same Vec<u8>
                // payload path, so the variant is the only thing we
                // can discriminate on cheaply — done via the error
                // text below.
                classify_encode_err(compression)
            }
        }
    })
}

/// Encode a single named relation as a SimpleRow `.mgr` blob,
/// optionally compressed. Useful for per-relation backups or exports.
///
/// Returns [`MANGLE_ERR`] if the relation does not exist in the store;
/// other error codes match [`mangle_save_facts_mgr`].
///
/// # Safety
/// `engine` must be a live handle. `relation` must point to
/// `relation_len` readable UTF-8 bytes. `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_save_relation_mgr(
    engine: *mut MangleEngine,
    relation: *const u8,
    relation_len: usize,
    compression: i32,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_save_relation_mgr: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let relation_str = match read_utf8(relation, relation_len, "mangle_save_relation_mgr") {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        let tuples = match eng.materialize_relation(&relation_str) {
            Ok(Some(t)) => t,
            Ok(None) => {
                set_error_msg("mangle_save_relation_mgr: engine has no rules loaded");
                return MANGLE_ERR_NO_RULES;
            }
            Err(e) => {
                set_error_msg(format!("mangle_save_relation_mgr({relation_str}): {e:#}"));
                return MANGLE_ERR;
            }
        };
        let tables = vec![(relation_str, tuples)];
        match encode_tables(tables, compression) {
            Ok(bytes) => {
                unsafe { write_buffer(out, bytes) };
                MANGLE_OK
            }
            Err(e) => {
                set_error_msg(format!("mangle_save_relation_mgr: {e:#}"));
                classify_encode_err(compression)
            }
        }
    })
}

/// Run a query and encode the matching tuples as a SimpleRow `.mgr`
/// blob under the caller-supplied relation name `out_relation`.
///
/// Useful for "Export Query Result" — the user types `route("GET",
/// Path)`, the workbench produces a downloadable `.mgr` file where
/// the matching tuples live under a relation named e.g.
/// `get_routes`. The output relation name is independent of the
/// queried predicate, so it can be anything the consumer wants.
///
/// Query syntax matches [`mangle_query`].
///
/// # Safety
/// `engine` must be a live handle. `query` must point to `query_len`
/// readable UTF-8 bytes. `out_relation` must point to
/// `out_relation_len` readable UTF-8 bytes (must be non-empty). `out`
/// must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mangle_query_dump_mgr(
    engine: *mut MangleEngine,
    query: *const u8,
    query_len: usize,
    out_relation: *const u8,
    out_relation_len: usize,
    compression: i32,
    out: *mut MangleBuffer,
) -> i32 {
    panic_boundary!(engine, {
        if out.is_null() {
            set_error_msg("mangle_query_dump_mgr: out pointer is null");
            return MANGLE_ERR_INVALID_ARG;
        }
        let query_str = match read_utf8(query, query_len, "mangle_query_dump_mgr: query") {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        let out_relation_str = match read_utf8(
            out_relation,
            out_relation_len,
            "mangle_query_dump_mgr: out_relation",
        ) {
            Ok(s) => s,
            Err(rc) => return rc,
        };
        if out_relation_str.is_empty() {
            set_error_msg("mangle_query_dump_mgr: out_relation must be non-empty");
            return MANGLE_ERR_INVALID_ARG;
        }
        let parsed = match parse_query_lenient(&query_str) {
            Ok(p) => p,
            Err(e) => {
                set_error_msg(format!("mangle_query_dump_mgr: {e:#}"));
                return MANGLE_ERR_PARSE;
            }
        };
        // SAFETY: engine non-null and not poisoned per panic_boundary.
        let eng = unsafe { &*engine };
        let materialized = match eng.materialize_relation(&parsed.predicate) {
            Ok(Some(t)) => t,
            Ok(None) => {
                set_error_msg("mangle_query_dump_mgr: engine has no rules loaded");
                return MANGLE_ERR_NO_RULES;
            }
            Err(e) => {
                set_error_msg(format!("mangle_query_dump_mgr: {e:#}"));
                return MANGLE_ERR;
            }
        };
        let filtered = filter_tuples(materialized, &parsed);
        let tables = vec![(out_relation_str, filtered)];
        match encode_tables(tables, compression) {
            Ok(bytes) => {
                unsafe { write_buffer(out, bytes) };
                MANGLE_OK
            }
            Err(e) => {
                set_error_msg(format!("mangle_query_dump_mgr: {e:#}"));
                classify_encode_err(compression)
            }
        }
    })
}

/// Read `len` bytes from `ptr` and decode as UTF-8. Returns
/// `Err(error_code)` on failure with `last_error` populated.
fn read_utf8(ptr: *const u8, len: usize, who: &str) -> std::result::Result<String, i32> {
    if len == 0 {
        return Ok(String::new());
    }
    if ptr.is_null() {
        set_error_msg(format!("{who}: pointer is null but length is {len}"));
        return Err(MANGLE_ERR_INVALID_ARG);
    }
    // SAFETY: caller's contract.
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    match std::str::from_utf8(slice) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            set_error_msg(format!("{who}: invalid UTF-8: {e}"));
            Err(MANGLE_ERR_INVALID_ARG)
        }
    }
}

/// Map an encode/compress error to the right status code. Bad
/// compression modes (including the unsupported zstd) → INVALID_ARG;
/// everything else → generic ERR. The message in `last_error` carries
/// the full detail.
fn classify_encode_err(compression: i32) -> i32 {
    // Anything outside the gzip-or-none valid set is treated as a bad
    // compression argument: out-of-range values and zstd (which is a
    // reserved-but-unimplemented mode on write) both belong here.
    let supported = MANGLE_COMPRESSION_NONE..=MANGLE_COMPRESSION_GZIP;
    if supported.contains(&compression) {
        MANGLE_ERR
    } else {
        MANGLE_ERR_INVALID_ARG
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompress_passthrough_for_uncompressed() {
        let data = b"hello world".to_vec();
        let out = decompress_if_needed(&data).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn decompress_gzip_roundtrip() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let payload = b"some uncompressed bytes\nmaybe a SimpleRow\n".to_vec();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let decoded = decompress_if_needed(&compressed).unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn decompress_empty_passthrough() {
        let out = decompress_if_needed(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn decompress_short_input_passthrough() {
        // Inputs shorter than the gzip magic shouldn't be misread.
        let out = decompress_if_needed(&[0x1f]).unwrap();
        assert_eq!(out, vec![0x1f]);
    }
}
