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

//! Name trie for Mangle's name constant hierarchy.
//!
//! Maps name constants (like `/animal/dog`) to their most precise type
//! via longest-prefix matching. Used by the bounds checker to infer types
//! for name constants appearing in facts.

use rustc_hash::FxHashMap;
use mangle_ir::{Inst, InstId, Ir};

use crate::type_expr;

/// A trie over the `/`-separated segments of Mangle name constants.
#[derive(Debug, Default)]
pub struct NameTrie {
    children: FxHashMap<String, NameTrie>,
    is_terminal: bool,
}

impl NameTrie {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a name path (e.g. "/foo/bar") into the trie.
    pub fn add(&mut self, name: &str) {
        let parts = split_name(name);
        let mut node = self;
        for part in parts {
            node = node
                .children
                .entry(part.to_string())
                .or_default();
        }
        node.is_terminal = true;
    }

    /// Returns true if the exact name is in the trie.
    pub fn contains(&self, name: &str) -> bool {
        let parts = split_name(name);
        let mut node = self;
        for part in parts {
            match node.children.get(part) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.is_terminal
    }

    /// Finds the longest prefix of `name` that exists in the trie.
    /// Returns the prefix as a name string, or `/name` if no prefix found.
    ///
    /// Example: if trie contains `/animal` and `/animal/dog`, and we look up
    /// `/animal/dog/poodle`, returns `/animal/dog`.
    pub fn prefix_name(&self, name: &str) -> String {
        let parts = split_name(name);
        let mut node = self;
        let mut last_terminal_idx: Option<usize> = None;

        for (i, part) in parts.iter().enumerate() {
            match node.children.get(*part) {
                Some(child) => {
                    if child.is_terminal {
                        last_terminal_idx = Some(i);
                    }
                    node = child;
                }
                None => break,
            }
        }

        match last_terminal_idx {
            Some(idx) => {
                let prefix_parts = &parts[..=idx];
                format!("/{}", prefix_parts.join("/"))
            }
            None => "/name".to_string(),
        }
    }

    /// Collects all name constants from a type expression into this trie.
    ///
    /// Special handling for `fn:TaggedUnion`: skips the tag field (arg 0)
    /// and tag values (odd-indexed args), only recurses into variant
    /// struct types (even-indexed args from index 2).
    pub fn collect(&mut self, ir: &Ir, id: InstId) {
        match ir.get(id) {
            Inst::Name(n) => {
                let name = ir.resolve_name(*n);
                if !type_expr::is_base_type(ir, id) && name.starts_with('/') {
                    self.add(name);
                }
            }
            Inst::ApplyFn { function, args } => {
                let fname = ir.resolve_name(*function);
                if fname == type_expr::FN_TAGGED_UNION && args.len() >= 3 {
                    // Only collect from variant struct types, not tag
                    // field name or variant tag values.
                    for i in (2..args.len()).step_by(2) {
                        self.collect(ir, args[i]);
                    }
                } else {
                    for arg in args {
                        self.collect(ir, *arg);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Splits a name like "/foo/bar" into segments ["foo", "bar"].
fn split_name(name: &str) -> Vec<&str> {
    name.split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_trie_operations() {
        let mut trie = NameTrie::new();
        trie.add("/animal");
        trie.add("/animal/dog");
        trie.add("/color");

        assert!(trie.contains("/animal"));
        assert!(trie.contains("/animal/dog"));
        assert!(trie.contains("/color"));
        assert!(!trie.contains("/animal/cat"));
        assert!(!trie.contains("/plant"));
    }

    #[test]
    fn prefix_name_lookup() {
        let mut trie = NameTrie::new();
        trie.add("/animal");
        trie.add("/animal/dog");

        assert_eq!(trie.prefix_name("/animal/dog/poodle"), "/animal/dog");
        assert_eq!(trie.prefix_name("/animal/cat"), "/animal");
        assert_eq!(trie.prefix_name("/animal"), "/animal");
        assert_eq!(trie.prefix_name("/plant/rose"), "/name");
    }

    #[test]
    fn collect_from_type_expr() {
        let mut ir = Ir::new();
        let mut trie = NameTrie::new();

        // Build: fn:Struct(/x, /animal, /y, /color)
        let x = {
            let n = ir.intern_name("/x");
            ir.add_inst(Inst::Name(n))
        };
        let animal = {
            let n = ir.intern_name("/animal");
            ir.add_inst(Inst::Name(n))
        };
        let y = {
            let n = ir.intern_name("/y");
            ir.add_inst(Inst::Name(n))
        };
        let color = {
            let n = ir.intern_name("/color");
            ir.add_inst(Inst::Name(n))
        };
        let struct_name = ir.intern_name("fn:Struct");
        let struct_type = ir.add_inst(Inst::ApplyFn {
            function: struct_name,
            args: vec![x, animal, y, color],
        });

        trie.collect(&ir, struct_type);
        assert!(trie.contains("/x"));
        assert!(trie.contains("/animal"));
        assert!(trie.contains("/y"));
        assert!(trie.contains("/color"));
    }

    #[test]
    fn collect_from_tagged_union_skips_tags() {
        let mut ir = Ir::new();
        let mut trie = NameTrie::new();

        // fn:TaggedUnion(/kind, /move, fn:Struct(/x, /number), /quit, fn:Struct())
        let kind = {
            let n = ir.intern_name("/kind");
            ir.add_inst(Inst::Name(n))
        };
        let move_ = {
            let n = ir.intern_name("/move");
            ir.add_inst(Inst::Name(n))
        };
        let x = {
            let n = ir.intern_name("/x");
            ir.add_inst(Inst::Name(n))
        };
        let number = {
            let n = ir.intern_name("/number");
            ir.add_inst(Inst::Name(n))
        };
        let struct_name = ir.intern_name("fn:Struct");
        let move_struct = ir.add_inst(Inst::ApplyFn {
            function: struct_name,
            args: vec![x, number],
        });
        let quit = {
            let n = ir.intern_name("/quit");
            ir.add_inst(Inst::Name(n))
        };
        let quit_struct = ir.add_inst(Inst::ApplyFn {
            function: struct_name,
            args: vec![],
        });
        let tu_name = ir.intern_name("fn:TaggedUnion");
        let tu = ir.add_inst(Inst::ApplyFn {
            function: tu_name,
            args: vec![kind, move_, move_struct, quit, quit_struct],
        });

        trie.collect(&ir, tu);

        // Should collect /x and /number from variant structs.
        assert!(trie.contains("/x"));
        // /number is a base type, so it should NOT be collected.
        assert!(!trie.contains("/number"));
        // Should NOT collect /kind (tag field), /move, /quit (tag values).
        assert!(!trie.contains("/kind"));
        assert!(!trie.contains("/move"));
        assert!(!trie.contains("/quit"));
    }
}
