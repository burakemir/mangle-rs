// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Context, Result, anyhow};
use mangle_ast as ast;
use mangle_parse::Parser;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};

/// A simple in-memory representation of the loaded data.
pub struct SimpleColumnData<'a> {
    /// Map predicate name -> List of facts (args)
    pub tables: HashMap<String, Vec<Vec<&'a ast::BaseTerm<'a>>>>,
}

pub fn read_from_bytes<'a>(arena: &'a ast::Arena, data: &[u8]) -> Result<SimpleColumnData<'a>> {
    let reader = sniff_decompress(BufReader::new(data))?;
    read_simple_column(arena, reader)
}

pub fn read_from_reader<'a, R: Read + 'static>(
    arena: &'a ast::Arena,
    reader: R,
) -> Result<SimpleColumnData<'a>> {
    let reader = sniff_decompress(BufReader::new(reader))?;
    read_simple_column(arena, reader)
}

/// Peek at the first four bytes of `reader`. If they match a known
/// compression magic number, wrap the reader in the appropriate streaming
/// decoder. Otherwise return the reader unchanged.
///
/// Supported formats (feature-gated):
/// - gzip (`1F 8B ..`) — via `flate2`, feature `gzip` (default on).
/// - zstd (`28 B5 2F FD`) — via `ruzstd`, feature `zstd` (default on).
///
/// When both features are disabled this is a pass-through.
fn sniff_decompress<'r, R: BufRead + 'r>(mut reader: R) -> Result<Box<dyn BufRead + 'r>> {
    let magic: [u8; 4] = {
        let buf = reader.fill_buf()?;
        let mut m = [0u8; 4];
        let n = buf.len().min(4);
        m[..n].copy_from_slice(&buf[..n]);
        m
    };
    match magic {
        #[cfg(feature = "gzip")]
        [0x1f, 0x8b, _, _] => {
            let dec = flate2::read::GzDecoder::new(reader);
            Ok(Box::new(BufReader::new(dec)))
        }
        #[cfg(feature = "zstd")]
        [0x28, 0xb5, 0x2f, 0xfd] => {
            let dec = ruzstd::decoding::StreamingDecoder::new(reader)
                .map_err(|e| anyhow!("zstd decoder init failed: {e}"))?;
            Ok(Box::new(BufReader::new(dec)))
        }
        _ => Ok(Box::new(reader)),
    }
}

fn read_simple_column<'a, R: BufRead>(
    arena: &'a ast::Arena,
    mut reader: R,
) -> Result<SimpleColumnData<'a>> {
    let mut line = String::new();

    // 1. Num Predicates
    reader.read_line(&mut line)?;
    let num_preds: usize = line.trim().parse().context("parsing num_preds")?;
    line.clear();

    struct PredInfo {
        name: String,
        arity: usize,
        num_facts: usize,
    }
    let mut preds = Vec::with_capacity(num_preds);

    // 2. Predicate Headers
    for _ in 0..num_preds {
        reader.read_line(&mut line)?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(anyhow!("Invalid predicate header: {line}"));
        }
        let name = parts[0].to_string();
        let arity: usize = parts[1].parse().context("parsing arity")?;
        let num_facts: usize = parts[2].parse().context("parsing num_facts")?;
        preds.push(PredInfo {
            name,
            arity,
            num_facts,
        });
        line.clear();
    }

    let mut tables = HashMap::new();

    // 3. Columns
    for pred in preds {
        if pred.arity == 0 {
            // Flag fact
            if pred.num_facts > 0 {
                tables.insert(pred.name, vec![vec![]]);
            }
            continue;
        }

        // Initialize facts with empty rows
        let mut facts: Vec<Vec<&'a ast::BaseTerm<'a>>> =
            vec![Vec::with_capacity(pred.arity); pred.num_facts];

        // Read columns
        for col_idx in 0..pred.arity {
            for row_idx in 0..pred.num_facts {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    return Err(anyhow!(
                        "Unexpected EOF reading column {col_idx} row {row_idx}",
                    ));
                }

                let text = line.trim();
                let term_str = if text.starts_with('/') {
                    percent_unescape(text)?
                } else {
                    text.to_string()
                };

                // Parse term
                // We need a fresh parser for each term?
                // Parser::new takes full input. `parse_base_term` parses one term.
                // It should stop after the term.
                // Since line contains just the term, it should work.
                let mut parser = Parser::new(arena, term_str.as_bytes(), "simplecolumn");
                parser.next_token()?;
                let term = parser
                    .parse_base_term()
                    .context(format!("parsing term: {term_str}"))?;

                facts[row_idx].push(term);
            }
        }
        tables.insert(pred.name, facts);
    }

    Ok(SimpleColumnData { tables })
}

fn percent_unescape(s: &str) -> Result<String> {
    // Go implementation uses url.QueryUnescape which handles %XX and + -> space.
    // Rust `url` crate? I don't have it in dependencies.
    // I can do basic unescaping manually or assume basic format.
    // The previous implementation used `url` crate in Go.
    // I'll check if I can use a crate or implement simple unescape.
    // Given constraints, I'll implement a simple one handling %XX.

    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(anyhow!("Invalid escape sequence"));
            }
            let hex = &s[i + 1..i + 3];
            let byte = u8::from_str_radix(hex, 16)?;
            out.push(byte);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b' '); // url decoding usually treats + as space
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}

// --- Edge Mode Support ---
#[cfg(feature = "edge")]
pub mod store {
    use super::*;
    use mangle_ast as ast;
    use mangle_common::{Store, Value};
    use mangle_interpreter::MemStore;

    pub struct SimpleColumnStore {
        mem: MemStore,
    }

    impl SimpleColumnStore {
        pub fn from_bytes(arena: &ast::Arena, data: &[u8]) -> Result<Self> {
            let sc_data = read_from_bytes(arena, data)?;
            let mut mem = MemStore::new();

            for (pred, facts) in sc_data.tables {
                mem.create_relation(&pred);
                for row in facts {
                    let tuple: Vec<Value> = row.iter().map(|t| term_to_value(t)).collect();
                    mem.insert(&pred, tuple)?;
                }
            }
            Ok(Self { mem })
        }
    }

    impl Store for SimpleColumnStore {
        fn scan(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
            self.mem.scan(relation)
        }

        fn scan_delta(&self, relation: &str) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
            self.mem.scan_delta(relation)
        }

        fn scan_next_delta(
            &self,
            relation: &str,
        ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
            self.mem.scan_next_delta(relation)
        }

        fn scan_index(
            &self,
            relation: &str,
            col_idx: usize,
            key: &Value,
        ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
            self.mem.scan_index(relation, col_idx, key)
        }

        fn scan_delta_index(
            &self,
            relation: &str,
            col_idx: usize,
            key: &Value,
        ) -> Result<Box<dyn Iterator<Item = Vec<Value>> + '_>> {
            self.mem.scan_delta_index(relation, col_idx, key)
        }

        fn insert(&mut self, relation: &str, tuple: Vec<Value>) -> Result<bool> {
            self.mem.insert(relation, tuple)
        }

        fn merge_deltas(&mut self) {
            self.mem.merge_deltas()
        }

        fn create_relation(&mut self, relation: &str) {
            self.mem.create_relation(relation)
        }

        fn retract(&mut self, relation: &str, tuple: &[Value]) -> Result<bool> {
            self.mem.retract(relation, tuple)
        }

        fn clear(&mut self, relation: &str) {
            self.mem.clear(relation)
        }

        fn relation_names(&self) -> Vec<String> {
            self.mem.relation_names()
        }
    }

    fn term_to_value(term: &ast::BaseTerm) -> Value {
        match term {
            ast::BaseTerm::Const(ast::Const::Number(n)) => Value::Number(*n),
            ast::BaseTerm::Const(ast::Const::String(s)) => Value::String(s.to_string()),
            // TODO: Handle other types mapping
            _ => Value::String(format!("{:?}", term)), // Fallback
        }
    }
}

// --- Server Mode Support ---
#[cfg(feature = "server")]
pub mod host {
    use super::*;
    use mangle_ast::Arena;
    use mangle_common::{Host, HostVal};
    use std::collections::HashMap;
    use std::fs::File;
    use std::path::Path;

    pub struct SimpleColumnHost {
        arena: Arena,
        data: HashMap<i32, Vec<Vec<i64>>>,
        iters: HashMap<i32, (i32, usize)>,
        next_iter_id: i32,
        /// Value slab for HostVal handles
        values: Vec<i64>,
    }

    impl SimpleColumnHost {
        pub fn new() -> Self {
            Self {
                arena: Arena::new_with_global_interner(),
                data: HashMap::new(),
                iters: HashMap::new(),
                next_iter_id: 1,
                values: Vec::new(),
            }
        }

        fn alloc_number(&mut self, n: i64) -> HostVal {
            let idx = self.values.len() as u32;
            self.values.push(n);
            HostVal(idx)
        }

        fn get_number(&self, hv: HostVal) -> i64 {
            self.values[hv.0 as usize]
        }

        pub fn load_file(&mut self, _rel_name: &str, path: &Path) -> Result<()> {
            let file = File::open(path)?;
            let reader = super::sniff_decompress(std::io::BufReader::new(file))?;
            let sc_data = read_simple_column(&self.arena, reader)?;

            for (pred, facts) in sc_data.tables {
                let id = hash_name(&pred);
                let mut numeric_facts = Vec::new();
                for row in facts {
                    let mut tuple = Vec::new();
                    for term in row {
                        if let ast::BaseTerm::Const(ast::Const::Number(n)) = term {
                            tuple.push(*n);
                        } else {
                            tuple.push(0);
                        }
                    }
                    numeric_facts.push(tuple);
                }
                self.data.insert(id, numeric_facts);
            }
            Ok(())
        }
    }

    fn hash_name(name: &str) -> i32 {
        let mut hash: u32 = 5381;
        for c in name.bytes() {
            hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u32);
        }
        hash as i32
    }

    impl Host for SimpleColumnHost {
        fn scan_start(&mut self, rel_id: i32) -> i32 {
            let id = self.next_iter_id;
            self.next_iter_id += 1;
            self.iters.insert(id, (rel_id, 0));
            id
        }

        fn scan_next(&mut self, iter_id: i32) -> i32 {
            if let Some((rel_id, idx)) = self.iters.get_mut(&iter_id) {
                if let Some(tuples) = self.data.get(rel_id) {
                    if *idx < tuples.len() {
                        let ptr = (iter_id << 16) | (*idx as i32 + 1);
                        *idx += 1;
                        return ptr;
                    }
                }
            }
            0
        }

        fn get_col(&mut self, ptr: i32, col_idx: i32) -> HostVal {
            let iter_id = ptr >> 16;
            let tuple_idx = (ptr & 0xFFFF) - 1;

            if let Some((rel_id, _)) = self.iters.get(&iter_id) {
                if let Some(tuples) = self.data.get(rel_id) {
                    if let Some(row) = tuples.get(tuple_idx as usize) {
                        let n = row.get(col_idx as usize).copied().unwrap_or(0);
                        return self.alloc_number(n);
                    }
                }
            }
            HostVal(0)
        }

        fn insert_begin(&mut self, _rel_id: i32) {}
        fn insert_push(&mut self, _val: HostVal) {}
        fn insert_end(&mut self) {}

        fn scan_delta_start(&mut self, _rel_id: i32) -> i32 { 0 }
        fn scan_index_start(&mut self, _rel_id: i32, _col_idx: i32, _val: HostVal) -> i32 { 0 }
        fn scan_aggregate_start(&mut self, _rel_id: i32, _description: Vec<i32>) -> i32 { 0 }
        fn merge_deltas(&mut self) -> i32 { 0 }

        fn const_number(&mut self, n: i64) -> HostVal { self.alloc_number(n) }
        fn const_float(&mut self, _bits: i64) -> HostVal { HostVal(0) }
        fn const_string(&mut self, _id: i32) -> HostVal { HostVal(0) }
        fn const_name(&mut self, _id: i32) -> HostVal { HostVal(0) }
        fn const_time(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
        fn const_duration(&mut self, _nanos: i64) -> HostVal { HostVal(0) }
        fn val_add(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
        fn val_sub(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
        fn val_mul(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
        fn val_div(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
        fn val_sqrt(&mut self, _a: HostVal) -> HostVal { HostVal(0) }
        fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 { (self.get_number(a) == self.get_number(b)) as i32 }
        fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 { (self.get_number(a) != self.get_number(b)) as i32 }
        fn val_lt(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
        fn val_le(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
        fn val_gt(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
        fn val_ge(&mut self, _a: HostVal, _b: HostVal) -> i32 { 0 }
        fn str_concat(&mut self, _a: HostVal, _b: HostVal) -> HostVal { HostVal(0) }
        fn str_replace(&mut self, _s: HostVal, _old: HostVal, _new: HostVal, _count: HostVal) -> HostVal { HostVal(0) }
        fn val_to_string(&mut self, _val: HostVal) -> HostVal { HostVal(0) }
        fn compound_begin(&mut self, _kind: i32) {}
        fn compound_push(&mut self, _val: HostVal) {}
        fn compound_end(&mut self) -> HostVal { HostVal(0) }
        fn compound_get(&mut self, _compound: HostVal, _key: HostVal) -> HostVal { HostVal(0) }
        fn compound_len(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
        fn pair_first(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
        fn pair_second(&mut self, _compound: HostVal) -> HostVal { HostVal(0) }
        fn debuglog(&mut self, _val: HostVal) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny but complete SimpleColumn document: one predicate `r` of arity
    /// 1 with a single fact.
    const SAMPLE: &str = "1\nr 1 1\n42\n";

    fn assert_sample(data: &SimpleColumnData) {
        let rows = data.tables.get("r").expect("relation r missing");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        match rows[0][0] {
            ast::BaseTerm::Const(ast::Const::Number(42)) => {}
            other => panic!("unexpected term: {other:?}"),
        }
    }

    #[test]
    fn reads_plain_bytes() {
        let arena = ast::Arena::new_with_global_interner();
        let data = read_from_bytes(&arena, SAMPLE.as_bytes()).unwrap();
        assert_sample(&data);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn reads_gzip_compressed() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;

        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(SAMPLE.as_bytes()).unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(
            &compressed[..2],
            &[0x1f, 0x8b],
            "encoder must produce a gzip magic header"
        );

        let arena = ast::Arena::new_with_global_interner();
        let data = read_from_bytes(&arena, &compressed).unwrap();
        assert_sample(&data);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn reads_zstd_compressed() {
        use ruzstd::encoding::{CompressionLevel, compress_to_vec};

        let compressed = compress_to_vec(SAMPLE.as_bytes(), CompressionLevel::Uncompressed);
        assert_eq!(
            &compressed[..4],
            &[0x28, 0xb5, 0x2f, 0xfd],
            "encoder must produce a zstd magic header"
        );

        let arena = ast::Arena::new_with_global_interner();
        let data = read_from_bytes(&arena, &compressed).unwrap();
        assert_sample(&data);
    }
}
