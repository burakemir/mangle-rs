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

use crate::{Host, HostVal};
use std::collections::HashMap;

/// A Host implementation that aggregates multiple sub-hosts.
///
/// It routes relation scans and inserts to the appropriate backend based on
/// explicit routing rules. Values from sub-hosts are remapped through a
/// composite value table so that HostVal handles remain unique across sub-hosts.
pub struct CompositeHost {
    hosts: Vec<Box<dyn Host + Send>>,

    /// rel_id -> index in `hosts`
    routes: HashMap<i32, usize>,

    /// iter_id -> (host_index, real_iter_id)
    active_iters: HashMap<i32, (usize, i32)>,

    next_iter_id: i32,

    /// Maps composite HostVal -> (host_index, sub_host HostVal).
    val_map: Vec<(usize, HostVal)>,
}

impl Default for CompositeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl CompositeHost {
    pub fn new() -> Self {
        Self {
            hosts: Vec::new(),
            routes: HashMap::new(),
            active_iters: HashMap::new(),
            next_iter_id: 1,
            val_map: Vec::new(),
        }
    }

    /// Adds a sub-host and returns its index.
    pub fn add_host(&mut self, host: Box<dyn Host + Send>) -> usize {
        let idx = self.hosts.len();
        self.hosts.push(host);
        idx
    }

    /// Routes a relation to a specific sub-host.
    pub fn route_relation(&mut self, rel_name: &str, host_index: usize) {
        let id = hash_name(rel_name);
        self.routes.insert(id, host_index);
    }

    /// Wraps a sub-host HostVal into a composite HostVal.
    fn wrap(&mut self, host_idx: usize, sub_hv: HostVal) -> HostVal {
        let composite_id = self.val_map.len() as u32;
        self.val_map.push((host_idx, sub_hv));
        HostVal(composite_id)
    }

    /// Unwraps a composite HostVal into (host_index, sub_host HostVal).
    fn unwrap(&self, hv: HostVal) -> (usize, HostVal) {
        self.val_map[hv.0 as usize]
    }

    /// The default host index (0) for value operations.
    fn default_host(&self) -> usize {
        0
    }
}

fn hash_name(name: &str) -> i32 {
    let mut hash: u32 = 5381;
    for c in name.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u32);
    }
    hash as i32
}

impl Host for CompositeHost {
    fn scan_start(&mut self, rel_id: i32) -> i32 {
        if let Some(&h_idx) = self.routes.get(&rel_id) {
            let real_id = self.hosts[h_idx].scan_start(rel_id);
            if real_id != 0 {
                let id = self.next_iter_id;
                self.next_iter_id += 1;
                self.active_iters.insert(id, (h_idx, real_id));
                return id;
            }
        }
        0
    }

    fn scan_next(&mut self, iter_id: i32) -> i32 {
        if let Some(&(h_idx, real_id)) = self.active_iters.get(&iter_id) {
            let ptr = self.hosts[h_idx].scan_next(real_id);
            if ptr == 0 {
                return 0;
            }
            // Tag pointer with host index (using top 6 bits)
            return ptr | ((h_idx as i32 + 1) << 26);
        }
        0
    }

    fn get_col(&mut self, tuple_ptr: i32, col_idx: i32) -> HostVal {
        let h_idx_plus_1 = (tuple_ptr >> 26) & 0x3F;
        if h_idx_plus_1 == 0 {
            return HostVal(0);
        }

        let h_idx = (h_idx_plus_1 - 1) as usize;
        let real_ptr = tuple_ptr & !(0x3F << 26);

        if h_idx < self.hosts.len() {
            let sub_hv = self.hosts[h_idx].get_col(real_ptr, col_idx);
            return self.wrap(h_idx, sub_hv);
        }
        HostVal(0)
    }

    fn insert_begin(&mut self, rel_id: i32) {
        if let Some(&h_idx) = self.routes.get(&rel_id) {
            self.hosts[h_idx].insert_begin(rel_id);
        }
    }

    fn insert_push(&mut self, val: HostVal) {
        let (h_idx, sub_hv) = self.unwrap(val);
        self.hosts[h_idx].insert_push(sub_hv);
    }

    fn insert_end(&mut self) {
        // End insert on all hosts that might have a pending insert.
        for host in &mut self.hosts {
            host.insert_end();
        }
    }

    fn scan_delta_start(&mut self, rel_id: i32) -> i32 {
        if let Some(&h_idx) = self.routes.get(&rel_id) {
            let real_id = self.hosts[h_idx].scan_delta_start(rel_id);
            if real_id != 0 {
                let id = self.next_iter_id;
                self.next_iter_id += 1;
                self.active_iters.insert(id, (h_idx, real_id));
                return id;
            }
        }
        0
    }

    fn scan_index_start(&mut self, rel_id: i32, col_idx: i32, val: HostVal) -> i32 {
        if let Some(&h_idx) = self.routes.get(&rel_id) {
            let (_, sub_hv) = self.unwrap(val);
            let real_id = self.hosts[h_idx].scan_index_start(rel_id, col_idx, sub_hv);
            if real_id != 0 {
                let id = self.next_iter_id;
                self.next_iter_id += 1;
                self.active_iters.insert(id, (h_idx, real_id));
                return id;
            }
        }
        0
    }

    fn scan_aggregate_start(&mut self, rel_id: i32, description: Vec<i32>) -> i32 {
        if let Some(&h_idx) = self.routes.get(&rel_id) {
            let real_id = self.hosts[h_idx].scan_aggregate_start(rel_id, description);
            if real_id != 0 {
                let id = self.next_iter_id;
                self.next_iter_id += 1;
                self.active_iters.insert(id, (h_idx, real_id));
                return id;
            }
        }
        0
    }

    fn merge_deltas(&mut self) -> i32 {
        let mut changes = 0;
        for host in &mut self.hosts {
            changes |= host.merge_deltas();
        }
        changes
    }

    // --- Constants: delegate to default host ---

    fn const_number(&mut self, n: i64) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_number(n);
        self.wrap(h, sub_hv)
    }

    fn const_float(&mut self, bits: i64) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_float(bits);
        self.wrap(h, sub_hv)
    }

    fn const_string(&mut self, id: i32) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_string(id);
        self.wrap(h, sub_hv)
    }

    fn const_name(&mut self, id: i32) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_name(id);
        self.wrap(h, sub_hv)
    }

    fn const_time(&mut self, nanos: i64) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_time(nanos);
        self.wrap(h, sub_hv)
    }

    fn const_duration(&mut self, nanos: i64) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].const_duration(nanos);
        self.wrap(h, sub_hv)
    }

    // --- Arithmetic: delegate to host of first operand ---

    fn val_add(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        let sub_hv = self.hosts[h].val_add(a_sub, b_sub);
        self.wrap(h, sub_hv)
    }

    fn val_sub(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        let sub_hv = self.hosts[h].val_sub(a_sub, b_sub);
        self.wrap(h, sub_hv)
    }

    fn val_mul(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        let sub_hv = self.hosts[h].val_mul(a_sub, b_sub);
        self.wrap(h, sub_hv)
    }

    fn val_div(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        let sub_hv = self.hosts[h].val_div(a_sub, b_sub);
        self.wrap(h, sub_hv)
    }

    fn val_sqrt(&mut self, a: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let sub_hv = self.hosts[h].val_sqrt(a_sub);
        self.wrap(h, sub_hv)
    }

    // --- Comparisons ---

    fn val_eq(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_eq(a_sub, b_sub)
    }

    fn val_neq(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_neq(a_sub, b_sub)
    }

    fn val_lt(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_lt(a_sub, b_sub)
    }

    fn val_le(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_le(a_sub, b_sub)
    }

    fn val_gt(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_gt(a_sub, b_sub)
    }

    fn val_ge(&mut self, a: HostVal, b: HostVal) -> i32 {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        self.hosts[h].val_ge(a_sub, b_sub)
    }

    fn str_concat(&mut self, a: HostVal, b: HostVal) -> HostVal {
        let (h, a_sub) = self.unwrap(a);
        let (_, b_sub) = self.unwrap(b);
        let sub_hv = self.hosts[h].str_concat(a_sub, b_sub);
        self.wrap(h, sub_hv)
    }

    fn str_replace(&mut self, s: HostVal, old: HostVal, new: HostVal, count: HostVal) -> HostVal {
        let (h, s_sub) = self.unwrap(s);
        let (_, old_sub) = self.unwrap(old);
        let (_, new_sub) = self.unwrap(new);
        let (_, count_sub) = self.unwrap(count);
        let sub_hv = self.hosts[h].str_replace(s_sub, old_sub, new_sub, count_sub);
        self.wrap(h, sub_hv)
    }

    fn val_to_string(&mut self, val: HostVal) -> HostVal {
        let (h, sub_hv) = self.unwrap(val);
        let result = self.hosts[h].val_to_string(sub_hv);
        self.wrap(h, result)
    }

    fn compound_begin(&mut self, kind: i32) {
        let h = self.default_host();
        self.hosts[h].compound_begin(kind);
    }

    fn compound_push(&mut self, val: HostVal) {
        let (h, sub_hv) = self.unwrap(val);
        self.hosts[h].compound_push(sub_hv);
    }

    fn compound_end(&mut self) -> HostVal {
        let h = self.default_host();
        let sub_hv = self.hosts[h].compound_end();
        self.wrap(h, sub_hv)
    }

    fn compound_get(&mut self, compound: HostVal, key: HostVal) -> HostVal {
        let (h, c_sub) = self.unwrap(compound);
        let (_, k_sub) = self.unwrap(key);
        let sub_hv = self.hosts[h].compound_get(c_sub, k_sub);
        self.wrap(h, sub_hv)
    }

    fn compound_len(&mut self, compound: HostVal) -> HostVal {
        let (h, c_sub) = self.unwrap(compound);
        let sub_hv = self.hosts[h].compound_len(c_sub);
        self.wrap(h, sub_hv)
    }

    fn pair_first(&mut self, compound: HostVal) -> HostVal {
        let (h, c_sub) = self.unwrap(compound);
        let sub_hv = self.hosts[h].pair_first(c_sub);
        self.wrap(h, sub_hv)
    }

    fn pair_second(&mut self, compound: HostVal) -> HostVal {
        let (h, c_sub) = self.unwrap(compound);
        let sub_hv = self.hosts[h].pair_second(c_sub);
        self.wrap(h, sub_hv)
    }

    fn debuglog(&mut self, val: HostVal) {
        let (h, sub_hv) = self.unwrap(val);
        self.hosts[h].debuglog(sub_hv);
    }
}
