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
use std::io::Read;

use crate::engine::MangleEngine;
use crate::error::{panic_boundary, set_error_msg};
use crate::{MANGLE_ERR, MANGLE_ERR_INVALID_ARG, MANGLE_ERR_NO_RULES, MANGLE_ERR_PARSE, MANGLE_OK};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

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
