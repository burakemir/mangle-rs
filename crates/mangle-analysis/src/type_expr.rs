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

//! Type expression utilities for Mangle's structural type system.
//!
//! Type expressions are represented as IR instructions (`Inst::ApplyFn` for
//! compound types, `Inst::Name` for base types). This module provides:
//!
//! - Predicates (`is_struct_type`, `is_union_type`, etc.)
//! - Accessors (`struct_type_field`, `list_type_arg`, etc.)
//! - Wellformedness validation (`wellformed_type`)
//! - Type conformance (`set_conforms`, `type_conforms`)
//! - HasType runtime validation (`has_type`)
//! - TaggedUnion expansion

use anyhow::{Result, anyhow, bail};
use rustc_hash::{FxHashMap, FxHashSet};
use mangle_ir::{Inst, InstId, Ir, NameId};

// Type constructor names.
pub const FN_STRUCT: &str = "fn:Struct";
pub const FN_UNION: &str = "fn:Union";
pub const FN_TAGGED_UNION: &str = "fn:TaggedUnion";
pub const FN_SINGLETON: &str = "fn:Singleton";
pub const FN_LIST: &str = "fn:List";
pub const FN_MAP: &str = "fn:Map";
pub const FN_PAIR: &str = "fn:Pair";
pub const FN_TUPLE: &str = "fn:Tuple";
pub const FN_FUN: &str = "fn:Fun";
pub const FN_REL: &str = "fn:Rel";
pub const FN_OPTION: &str = "fn:Option";
pub const FN_OPT: &str = "fn:opt";
pub const FN_EMPTY_TYPE: &str = "fn:EmptyType";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the function name of an `ApplyFn` instruction, or `None`.
pub fn apply_fn_name<'a>(ir: &'a Ir, id: InstId) -> Option<&'a str> {
    if let Inst::ApplyFn { function, .. } = ir.get(id) {
        Some(ir.resolve_name(*function))
    } else {
        None
    }
}

/// Returns the args of an `ApplyFn` instruction, or `None`.
pub fn apply_fn_args(ir: &Ir, id: InstId) -> Option<&[InstId]> {
    if let Inst::ApplyFn { args, .. } = ir.get(id) {
        Some(args.as_slice())
    } else {
        None
    }
}

/// Returns the name string of a `Name` instruction, or `None`.
fn name_str<'a>(ir: &'a Ir, id: InstId) -> Option<&'a str> {
    if let Inst::Name(n) = ir.get(id) {
        Some(ir.resolve_name(*n))
    } else {
        None
    }
}

/// Returns the `NameId` of a `Name` instruction, or `None`.
fn name_id(ir: &Ir, id: InstId) -> Option<NameId> {
    if let Inst::Name(n) = ir.get(id) {
        Some(*n)
    } else {
        None
    }
}

/// True if this is the empty type sentinel `fn:EmptyType()`.
pub fn is_empty_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_EMPTY_TYPE)
}

/// True if this is `/any`.
pub fn is_any(ir: &Ir, id: InstId) -> bool {
    name_str(ir, id) == Some("/any")
}

/// Finds or creates a `Name` instruction in the IR.
pub fn find_or_create_name(ir: &mut Ir, name: &str) -> InstId {
    if let Some(name_id) = ir.name_store.lookup(name) {
        for (idx, inst) in ir.insts.iter().enumerate() {
            if let Inst::Name(n) = inst {
                if *n == name_id {
                    return InstId::new(idx);
                }
            }
        }
    }
    let n = ir.intern_name(name);
    ir.add_inst(Inst::Name(n))
}

/// Creates or finds the empty type sentinel `fn:EmptyType()`.
pub fn empty_type(ir: &mut Ir) -> InstId {
    for (idx, inst) in ir.insts.iter().enumerate() {
        if let Inst::ApplyFn { function, args } = inst {
            if ir.resolve_name(*function) == FN_EMPTY_TYPE && args.is_empty() {
                return InstId::new(idx);
            }
        }
    }
    let fn_name = ir.intern_name(FN_EMPTY_TYPE);
    ir.add_inst(Inst::ApplyFn {
        function: fn_name,
        args: vec![],
    })
}

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

pub fn is_struct_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_STRUCT)
}

pub fn is_union_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_UNION)
}

pub fn is_singleton_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_SINGLETON)
}

pub fn is_tagged_union_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_TAGGED_UNION)
}

pub fn is_list_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_LIST)
}

pub fn is_map_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_MAP)
}

pub fn is_pair_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_PAIR)
}

pub fn is_tuple_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_TUPLE)
}

pub fn is_fun_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_FUN)
}

pub fn is_rel_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_REL)
}

pub fn is_option_type(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_OPTION)
}

pub fn is_opt_field(ir: &Ir, id: InstId) -> bool {
    apply_fn_name(ir, id) == Some(FN_OPT)
}

/// True if this is a base type name constant (e.g. `/number`, `/string`, `/any`).
pub fn is_base_type(ir: &Ir, id: InstId) -> bool {
    if let Some(name) = name_str(ir, id) {
        matches!(
            name,
            "/any"
                | "/bot"
                | "/number"
                | "/float64"
                | "/string"
                | "/bytes"
                | "/name"
                | "/bool"
                | "/time"
                | "/duration"
                | "/unit"
        )
    } else {
        false
    }
}

/// True if this is a `Inst::Name` (any name constant, including base types
/// and user-defined name hierarchy members like `/animal/dog`).
pub fn is_name_const(ir: &Ir, id: InstId) -> bool {
    matches!(ir.get(id), Inst::Name(_))
}

/// True if this is a type variable (`Inst::Var`).
pub fn is_type_var(ir: &Ir, id: InstId) -> bool {
    matches!(ir.get(id), Inst::Var(_))
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

/// Returns the element type of a `fn:List` type expression.
pub fn list_type_arg(ir: &Ir, id: InstId) -> Option<InstId> {
    let args = apply_fn_args(ir, id)?;
    if apply_fn_name(ir, id) != Some(FN_LIST) {
        return None;
    }
    args.first().copied()
}

/// Returns `(key_type, value_type)` of a `fn:Map` type expression.
pub fn map_type_args(ir: &Ir, id: InstId) -> Option<(InstId, InstId)> {
    let args = apply_fn_args(ir, id)?;
    if apply_fn_name(ir, id) != Some(FN_MAP) || args.len() != 2 {
        return None;
    }
    Some((args[0], args[1]))
}

/// Returns the alternative type expressions of a `fn:Union`.
pub fn union_type_args(ir: &Ir, id: InstId) -> Option<&[InstId]> {
    if apply_fn_name(ir, id) != Some(FN_UNION) {
        return None;
    }
    apply_fn_args(ir, id)
}

/// Returns the tag field `InstId` (a Name) of a `fn:TaggedUnion`.
pub fn tagged_union_tag_field(ir: &Ir, id: InstId) -> Option<InstId> {
    if apply_fn_name(ir, id) != Some(FN_TAGGED_UNION) {
        return None;
    }
    apply_fn_args(ir, id).and_then(|args| args.first().copied())
}

/// Returns `(tags, variant_struct_types)` for a `fn:TaggedUnion`.
/// Tags are at odd indices starting from 1, variant structs at even indices from 2.
pub fn tagged_union_variants(ir: &Ir, id: InstId) -> Option<(Vec<InstId>, Vec<InstId>)> {
    if apply_fn_name(ir, id) != Some(FN_TAGGED_UNION) {
        return None;
    }
    let args = apply_fn_args(ir, id)?;
    if args.len() < 3 || args.len() % 2 == 0 {
        return None;
    }
    let mut tags = Vec::new();
    let mut structs = Vec::new();
    for i in (1..args.len()).step_by(2) {
        tags.push(args[i]);
        structs.push(args[i + 1]);
    }
    Some((tags, structs))
}

/// Iterates struct type args, yielding `(field_name_id, field_type_id, is_optional)`.
///
/// Struct args are a flat list where:
/// - Required fields contribute 2 consecutive args: `[Name(field), type_expr]`
/// - Optional fields contribute 1 arg: `[ApplyFn("fn:opt", [Name(field), type_expr])]`
pub fn struct_type_fields(ir: &Ir, id: InstId) -> Vec<(InstId, InstId, bool)> {
    let args = match apply_fn_args(ir, id) {
        Some(a) if is_struct_type(ir, id) => a,
        _ => return Vec::new(),
    };
    let mut result = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if is_opt_field(ir, args[i]) {
            // Optional field: fn:opt(field_name, field_type)
            if let Some(opt_args) = apply_fn_args(ir, args[i]) {
                if opt_args.len() == 2 {
                    result.push((opt_args[0], opt_args[1], true));
                }
            }
            i += 1;
        } else {
            // Required field: field_name, field_type
            if i + 1 < args.len() {
                result.push((args[i], args[i + 1], false));
            }
            i += 2;
        }
    }
    result
}

/// Returns the type of a struct field by name (simple struct only).
pub fn struct_type_field(ir: &Ir, type_id: InstId, field: NameId) -> Option<InstId> {
    if !is_struct_type(ir, type_id) {
        return None;
    }
    let field_name = ir.resolve_name(field);
    for (fname_id, ftype_id, _optional) in struct_type_fields(ir, type_id) {
        if let Some(n) = name_str(ir, fname_id) {
            if n == field_name {
                return Some(ftype_id);
            }
        }
    }
    None
}

/// Returns the type of a struct field by name. Handles Struct, Union, and TaggedUnion
/// by projecting the field from each alternative and returning their upper bound.
pub fn struct_type_field_deep(ir: &mut Ir, type_id: InstId, field: NameId) -> Option<InstId> {
    if is_struct_type(ir, type_id) {
        return struct_type_field(ir, type_id, field);
    }
    if is_tagged_union_type(ir, type_id) {
        let tag_field = tagged_union_tag_field(ir, type_id)?;
        let (tags, structs) = tagged_union_variants(ir, type_id)?;
        let field_name = ir.resolve_name(field).to_string();
        let tag_field_name = name_str(ir, tag_field)?.to_string();

        if field_name == tag_field_name {
            // Tag field type = union of singleton types for each tag value.
            let singleton_fn = ir.intern_name(FN_SINGLETON);
            let mut tag_types = Vec::new();
            for tag in &tags {
                let s = ir.add_inst(Inst::ApplyFn {
                    function: singleton_fn,
                    args: vec![*tag],
                });
                tag_types.push(s);
            }
            return Some(new_union_or_single(ir, tag_types));
        }
        let mut field_types = Vec::new();
        for variant_struct in &structs {
            let vfield_name = ir.intern_name(&field_name);
            if let Some(ft) = struct_type_field(ir, *variant_struct, vfield_name) {
                field_types.push(ft);
            }
        }
        if field_types.is_empty() {
            return None;
        }
        let ctx = TypeContext::default();
        return Some(upper_bound(ir, &ctx, &field_types));
    }
    if is_union_type(ir, type_id) {
        let args = union_type_args(ir, type_id)?.to_vec();
        let mut field_types = Vec::new();
        for alt in &args {
            if let Some(ft) = struct_type_field_deep(ir, *alt, field) {
                field_types.push(ft);
            } else {
                return None; // Field not present in all alternatives.
            }
        }
        if field_types.is_empty() {
            return None;
        }
        let ctx = TypeContext::default();
        return Some(upper_bound(ir, &ctx, &field_types));
    }
    None
}

// ---------------------------------------------------------------------------
// TaggedUnion expansion
// ---------------------------------------------------------------------------

/// Expands `fn:TaggedUnion(tag_field, tag1, struct1, tag2, struct2, ...)`
/// into `fn:Union(fn:Struct(tag_field, fn:Singleton(tag1), ...fields1), ...)`.
///
/// Uses `fn:Singleton` for tag field values (precise expansion for HasType).
pub fn expand_tagged_union_type(ir: &mut Ir, tu_id: InstId) -> Result<InstId> {
    let (tag_field_id, tags, structs) = {
        let args = apply_fn_args(ir, tu_id)
            .ok_or_else(|| anyhow!("not a TaggedUnion"))?
            .to_vec();
        if args.len() < 3 || args.len() % 2 == 0 {
            bail!("TaggedUnion: bad arity");
        }
        let tag_field = args[0];
        let mut tags = Vec::new();
        let mut structs = Vec::new();
        for i in (1..args.len()).step_by(2) {
            tags.push(args[i]);
            structs.push(args[i + 1]);
        }
        (tag_field, tags, structs)
    };

    let singleton_name = ir.intern_name(FN_SINGLETON);
    let struct_name = ir.intern_name(FN_STRUCT);
    let union_name = ir.intern_name(FN_UNION);

    let mut variant_structs = Vec::new();
    for (tag_id, struct_id) in tags.iter().zip(structs.iter()) {
        // Build fn:Singleton(tag_value)
        let singleton = ir.add_inst(Inst::ApplyFn {
            function: singleton_name,
            args: vec![*tag_id],
        });

        // Collect fields from variant struct
        let variant_fields: Vec<InstId> =
            apply_fn_args(ir, *struct_id).unwrap_or(&[]).to_vec();

        // Build new struct: [tag_field, Singleton(tag), ...variant_fields]
        let mut new_args = vec![tag_field_id, singleton];
        new_args.extend_from_slice(&variant_fields);

        let new_struct = ir.add_inst(Inst::ApplyFn {
            function: struct_name,
            args: new_args,
        });
        variant_structs.push(new_struct);
    }

    let union_id = ir.add_inst(Inst::ApplyFn {
        function: union_name,
        args: variant_structs,
    });
    Ok(union_id)
}

/// Like `expand_tagged_union_type` but uses `/name` instead of `fn:Singleton`
/// for the tag field type. This is less precise but compatible with the bounds
/// checker's type inference (which infers `/name` for name constants in facts).
pub fn expand_tagged_union_for_bounds(ir: &mut Ir, tu_id: InstId) -> Result<InstId> {
    let (tag_field_id, tags, structs) = {
        let args = apply_fn_args(ir, tu_id)
            .ok_or_else(|| anyhow!("not a TaggedUnion"))?
            .to_vec();
        if args.len() < 3 || args.len() % 2 == 0 {
            bail!("TaggedUnion: bad arity");
        }
        let tag_field = args[0];
        let mut tags = Vec::new();
        let mut structs = Vec::new();
        for i in (1..args.len()).step_by(2) {
            tags.push(args[i]);
            structs.push(args[i + 1]);
        }
        (tag_field, tags, structs)
    };

    let name_type = ir.intern_name("/name");
    let name_type_id = ir.add_inst(Inst::Name(name_type));
    let struct_name = ir.intern_name(FN_STRUCT);
    let union_name = ir.intern_name(FN_UNION);

    let mut variant_structs = Vec::new();
    for (_tag_id, struct_id) in tags.iter().zip(structs.iter()) {
        let variant_fields: Vec<InstId> =
            apply_fn_args(ir, *struct_id).unwrap_or(&[]).to_vec();

        let mut new_args = vec![tag_field_id, name_type_id];
        new_args.extend_from_slice(&variant_fields);

        let new_struct = ir.add_inst(Inst::ApplyFn {
            function: struct_name,
            args: new_args,
        });
        variant_structs.push(new_struct);
    }

    let union_id = ir.add_inst(Inst::ApplyFn {
        function: union_name,
        args: variant_structs,
    });
    Ok(union_id)
}

// ---------------------------------------------------------------------------
// Wellformedness
// ---------------------------------------------------------------------------

/// Type variable context: maps variable NameId to its bound (an InstId type expr).
pub type TypeContext = FxHashMap<NameId, InstId>;

/// Validates that a type expression is well-formed.
pub fn wellformed_type(ir: &Ir, ctx: &TypeContext, id: InstId) -> Result<()> {
    match ir.get(id) {
        Inst::Name(n) => {
            let name = ir.resolve_name(*n);
            // Base types and name constants are always well-formed.
            if is_base_type(ir, id) || name.starts_with('/') {
                return Ok(());
            }
            bail!("unknown type name: {}", name)
        }
        Inst::Var(v) => {
            if ctx.contains_key(v) {
                Ok(())
            } else {
                bail!(
                    "type variable {} not in context",
                    ir.resolve_name(*v)
                )
            }
        }
        Inst::ApplyFn { function, args } => {
            let fname = ir.resolve_name(*function);
            match fname {
                FN_STRUCT => check_struct_type_expr(ir, ctx, args),
                FN_UNION => {
                    if args.len() < 2 {
                        bail!("fn:Union requires at least 2 alternatives");
                    }
                    for arg in args {
                        wellformed_type(ir, ctx, *arg)?;
                    }
                    Ok(())
                }
                FN_SINGLETON => {
                    if args.len() != 1 {
                        bail!("fn:Singleton requires exactly 1 argument");
                    }
                    // Argument must be a constant.
                    match ir.get(args[0]) {
                        Inst::Name(_) | Inst::Number(_) | Inst::String(_)
                        | Inst::Float(_) | Inst::Bool(_) | Inst::Time(_)
                        | Inst::Duration(_) => Ok(()),
                        _ => bail!("fn:Singleton argument must be a constant"),
                    }
                }
                FN_LIST => {
                    if args.len() != 1 {
                        bail!("fn:List requires exactly 1 argument");
                    }
                    wellformed_type(ir, ctx, args[0])
                }
                FN_MAP => {
                    if args.len() != 2 {
                        bail!("fn:Map requires exactly 2 arguments");
                    }
                    wellformed_type(ir, ctx, args[0])?;
                    wellformed_type(ir, ctx, args[1])
                }
                FN_PAIR => {
                    if args.len() != 2 {
                        bail!("fn:Pair requires exactly 2 arguments");
                    }
                    wellformed_type(ir, ctx, args[0])?;
                    wellformed_type(ir, ctx, args[1])
                }
                FN_TUPLE => {
                    if args.len() < 2 {
                        bail!("fn:Tuple requires at least 2 arguments");
                    }
                    for arg in args {
                        wellformed_type(ir, ctx, *arg)?;
                    }
                    Ok(())
                }
                FN_OPTION => {
                    if args.len() != 1 {
                        bail!("fn:Option requires exactly 1 argument");
                    }
                    wellformed_type(ir, ctx, args[0])
                }
                FN_FUN => check_fun_type_expr(ir, ctx, args),
                FN_REL => {
                    if args.is_empty() {
                        bail!("fn:Rel requires at least 1 argument");
                    }
                    for arg in args {
                        wellformed_type(ir, ctx, *arg)?;
                    }
                    Ok(())
                }
                FN_TAGGED_UNION => check_tagged_union_type_expr(ir, ctx, args),
                FN_OPT => {
                    // fn:opt is only valid inside fn:Struct, checked there.
                    if args.len() != 2 {
                        bail!("fn:opt requires exactly 2 arguments");
                    }
                    if !is_name_const(ir, args[0]) {
                        bail!("fn:opt first argument must be a name constant");
                    }
                    wellformed_type(ir, ctx, args[1])
                }
                other => bail!("unknown type constructor: {}", other),
            }
        }
        _ => bail!("not a valid type expression"),
    }
}

fn check_struct_type_expr(ir: &Ir, ctx: &TypeContext, args: &[InstId]) -> Result<()> {
    let mut i = 0;
    let mut seen_fields = FxHashSet::default();
    while i < args.len() {
        if is_opt_field(ir, args[i]) {
            // Optional field: fn:opt(field_name, field_type)
            let opt_args = apply_fn_args(ir, args[i])
                .ok_or_else(|| anyhow!("malformed fn:opt"))?;
            if opt_args.len() != 2 {
                bail!("fn:opt requires 2 arguments");
            }
            let field_name = name_str(ir, opt_args[0])
                .ok_or_else(|| anyhow!("fn:opt field name must be a name constant"))?;
            if !seen_fields.insert(field_name.to_string()) {
                bail!("duplicate struct field: {}", field_name);
            }
            wellformed_type(ir, ctx, opt_args[1])?;
            i += 1;
        } else {
            // Required field: field_name, field_type
            if i + 1 >= args.len() {
                bail!("fn:Struct: odd number of args (missing type for last field)");
            }
            let field_name = name_str(ir, args[i])
                .ok_or_else(|| anyhow!("fn:Struct field name must be a name constant"))?;
            if !seen_fields.insert(field_name.to_string()) {
                bail!("duplicate struct field: {}", field_name);
            }
            wellformed_type(ir, ctx, args[i + 1])?;
            i += 2;
        }
    }
    Ok(())
}

fn check_tagged_union_type_expr(
    ir: &Ir,
    ctx: &TypeContext,
    args: &[InstId],
) -> Result<()> {
    if args.len() < 3 {
        bail!("fn:TaggedUnion requires at least 3 arguments (tag_field, tag, struct)");
    }
    if args.len() % 2 == 0 {
        bail!("fn:TaggedUnion requires odd number of arguments");
    }

    // Arg 0: tag field must be a name constant.
    let tag_field_name = name_str(ir, args[0])
        .ok_or_else(|| anyhow!("fn:TaggedUnion tag field must be a name constant"))?;

    let mut seen_tags = FxHashSet::default();
    for i in (1..args.len()).step_by(2) {
        // Tag value must be a name constant.
        let tag_name = name_str(ir, args[i])
            .ok_or_else(|| anyhow!("fn:TaggedUnion variant tag must be a name constant"))?;
        if !seen_tags.insert(tag_name.to_string()) {
            bail!("fn:TaggedUnion duplicate variant tag: {}", tag_name);
        }

        // Variant type must be a struct type expression.
        let variant = args[i + 1];
        if !is_struct_type(ir, variant) {
            bail!(
                "fn:TaggedUnion variant type for {} must be fn:Struct",
                tag_name
            );
        }
        wellformed_type(ir, ctx, variant)?;

        // Tag field must NOT appear in variant struct.
        for (fname_id, _ftype_id, _optional) in struct_type_fields(ir, variant) {
            if let Some(n) = name_str(ir, fname_id) {
                if n == tag_field_name {
                    bail!(
                        "fn:TaggedUnion tag field {} must not appear in variant struct for {}",
                        tag_field_name,
                        tag_name
                    );
                }
            }
        }
    }

    Ok(())
}

fn check_fun_type_expr(ir: &Ir, ctx: &TypeContext, args: &[InstId]) -> Result<()> {
    if args.is_empty() {
        bail!("fn:Fun requires at least 1 argument (the result type)");
    }
    for arg in args {
        wellformed_type(ir, ctx, *arg)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Type Conformance
// ---------------------------------------------------------------------------

/// Checks if `left` set-conforms to `right`.
///
/// - Union on left: ALL alternatives must conform to `right`.
/// - Union on right: `left` must conform to SOME alternative.
/// - TaggedUnion: expanded before checking.
pub fn set_conforms(ir: &Ir, ctx: &TypeContext, left: InstId, right: InstId) -> bool {
    // Identity.
    if left == right {
        return true;
    }

    // /any is top type.
    if name_str(ir, right) == Some("/any") {
        return true;
    }
    // /bot is bottom type.
    if name_str(ir, left) == Some("/bot") {
        return true;
    }

    // Expand tagged unions on both sides (conceptually—we can't mutate ir here,
    // so we handle them by delegating to their variant structure).
    // TaggedUnion on left: each variant struct (with tag field) must conform.
    if is_tagged_union_type(ir, left) {
        return tagged_union_set_conforms_left(ir, ctx, left, right);
    }
    // TaggedUnion on right: left must conform to some expanded variant.
    if is_tagged_union_type(ir, right) {
        return tagged_union_set_conforms_right(ir, ctx, left, right);
    }

    // Union on left: ALL alternatives must conform.
    if let Some(left_args) = union_type_args(ir, left) {
        let left_args = left_args.to_vec();
        return left_args
            .iter()
            .all(|alt| set_conforms(ir, ctx, *alt, right));
    }

    // Union on right: left must conform to SOME alternative.
    if let Some(right_args) = union_type_args(ir, right) {
        let right_args = right_args.to_vec();
        return right_args
            .iter()
            .any(|alt| set_conforms(ir, ctx, left, *alt));
    }

    type_conforms(ir, ctx, left, right)
}

/// Checks structural type conformance (non-union, non-tagged-union).
pub fn type_conforms(ir: &Ir, ctx: &TypeContext, left: InstId, right: InstId) -> bool {
    if left == right {
        return true;
    }
    if name_str(ir, right) == Some("/any") {
        return true;
    }
    if name_str(ir, left) == Some("/bot") {
        return true;
    }

    // Name hierarchy: /foo/bar conforms to /foo, /name, /any.
    if let (Some(left_name), Some(right_name)) = (name_str(ir, left), name_str(ir, right)) {
        if right_name == "/name" && left_name.starts_with('/') {
            return true;
        }
        return left_name.starts_with(right_name)
            && (left_name.len() == right_name.len()
                || left_name.as_bytes().get(right_name.len()) == Some(&b'/'));
    }

    // Singleton conformance: fn:Singleton(c) <: T if c has type T.
    if is_singleton_type(ir, left) {
        if let Some(args) = apply_fn_args(ir, left) {
            if args.len() == 1 {
                return const_has_base_type(ir, args[0], right);
            }
        }
    }

    // Type variable: look up bound in context.
    if let Inst::Var(v) = ir.get(left) {
        if let Some(&bound) = ctx.get(v) {
            return set_conforms(ir, ctx, bound, right);
        }
    }
    if let Inst::Var(v) = ir.get(right) {
        if let Some(&bound) = ctx.get(v) {
            return set_conforms(ir, ctx, left, bound);
        }
    }

    let left_fn = apply_fn_name(ir, left);
    let right_fn = apply_fn_name(ir, right);
    if left_fn != right_fn {
        return false;
    }

    match left_fn {
        Some(FN_LIST) => {
            // Covariant.
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == 1 && ra.len() == 1 && set_conforms(ir, ctx, la[0], ra[0])
        }
        Some(FN_MAP) => {
            // Covariant in both key and value.
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == 2
                && ra.len() == 2
                && set_conforms(ir, ctx, la[0], ra[0])
                && set_conforms(ir, ctx, la[1], ra[1])
        }
        Some(FN_PAIR) => {
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == 2
                && ra.len() == 2
                && set_conforms(ir, ctx, la[0], ra[0])
                && set_conforms(ir, ctx, la[1], ra[1])
        }
        Some(FN_TUPLE) => {
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == ra.len()
                && la
                    .iter()
                    .zip(ra.iter())
                    .all(|(l, r)| set_conforms(ir, ctx, *l, *r))
        }
        Some(FN_STRUCT) => struct_type_conforms(ir, ctx, left, right),
        Some(FN_SINGLETON) => {
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == 1 && ra.len() == 1 && ir_eq(ir, la[0], ra[0])
        }
        Some(FN_FUN) => {
            // Covariant in codomain, contravariant in domain.
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            if la.len() != ra.len() || la.is_empty() {
                return false;
            }
            // Result type (first arg): covariant.
            if !set_conforms(ir, ctx, la[0], ra[0]) {
                return false;
            }
            // Argument types: contravariant.
            la[1..]
                .iter()
                .zip(ra[1..].iter())
                .all(|(l, r)| set_conforms(ir, ctx, *r, *l))
        }
        Some(FN_REL) => {
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == ra.len()
                && la
                    .iter()
                    .zip(ra.iter())
                    .all(|(l, r)| set_conforms(ir, ctx, *l, *r))
        }
        Some(FN_OPTION) => {
            let la = apply_fn_args(ir, left).unwrap();
            let ra = apply_fn_args(ir, right).unwrap();
            la.len() == 1 && ra.len() == 1 && set_conforms(ir, ctx, la[0], ra[0])
        }
        _ => false,
    }
}

/// Struct subtyping: `left` conforms to `right` if:
/// - All required fields of `right` are present in `left` with conforming types.
/// - Optional fields of `right` may be absent in `left`.
/// - `left` may have extra fields.
fn struct_type_conforms(
    ir: &Ir,
    ctx: &TypeContext,
    left: InstId,
    right: InstId,
) -> bool {
    let left_fields = struct_type_fields(ir, left);
    let right_fields = struct_type_fields(ir, right);

    // Build map of left fields: field_name_str -> (type_id, is_optional).
    let left_map: FxHashMap<String, (InstId, bool)> = left_fields
        .iter()
        .filter_map(|(fname, ftype, opt)| {
            name_str(ir, *fname).map(|n| (n.to_string(), (*ftype, *opt)))
        })
        .collect();

    for (fname, ftype, is_optional) in &right_fields {
        if let Some(fname_str) = name_str(ir, *fname) {
            match left_map.get(fname_str) {
                Some((left_type, _left_opt)) => {
                    if !set_conforms(ir, ctx, *left_type, *ftype) {
                        return false;
                    }
                }
                None => {
                    // Required field missing in left.
                    if !is_optional {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Helper: checks whether a TaggedUnion on the left conforms to some right type.
fn tagged_union_set_conforms_left(
    ir: &Ir,
    ctx: &TypeContext,
    left: InstId,
    right: InstId,
) -> bool {
    // Each variant of the tagged union must conform to right.
    if let Some((tags, structs)) = tagged_union_variants(ir, left) {
        let tag_field = tagged_union_tag_field(ir, left).unwrap();
        for (tag, variant_struct) in tags.iter().zip(structs.iter()) {
            // Conceptually: the expanded struct has tag_field: Singleton(tag) + variant fields.
            // We check conformance structurally without actually creating the expanded node.
            if !expanded_variant_conforms(ir, ctx, tag_field, *tag, *variant_struct, right) {
                return false;
            }
        }
        true
    } else {
        false
    }
}

/// Helper: checks whether some left type conforms to a TaggedUnion on the right.
fn tagged_union_set_conforms_right(
    ir: &Ir,
    ctx: &TypeContext,
    left: InstId,
    right: InstId,
) -> bool {
    // left must conform to at least one expanded variant of the tagged union.
    if let Some((tags, structs)) = tagged_union_variants(ir, right) {
        let tag_field = tagged_union_tag_field(ir, right).unwrap();
        for (tag, variant_struct) in tags.iter().zip(structs.iter()) {
            if expanded_variant_conforms_right(ir, ctx, left, tag_field, *tag, *variant_struct) {
                return true;
            }
        }
        false
    } else {
        false
    }
}

/// Checks that an expanded variant (tag_field: /name, ...variant_fields)
/// conforms to `right`.
fn expanded_variant_conforms(
    ir: &Ir,
    ctx: &TypeContext,
    tag_field: InstId,
    tag: InstId,
    variant_struct: InstId,
    right: InstId,
) -> bool {
    // For bounds-style checking: expanded variant uses /name for tag field.
    // We can't easily construct the expanded struct without &mut Ir,
    // so we check the variant struct directly and accept that the tag field
    // adds conformance to /name.
    // Simple approximation: the variant struct conforms if right is /any or
    // right is also a struct that the variant matches.
    if name_str(ir, right) == Some("/any") {
        return true;
    }
    // If right is a struct type, check that variant fields are a superset.
    if is_struct_type(ir, right) {
        // The variant struct + tag field must cover right's fields.
        return set_conforms(ir, ctx, variant_struct, right);
    }
    // Both sides tagged unions: left's variant must match some right variant.
    // A right variant matches when it uses the same tag field, the tag value
    // is identical, and the variant struct on the right accepts ours.
    if is_tagged_union_type(ir, right) {
        if let Some((r_tags, r_structs)) = tagged_union_variants(ir, right) {
            let r_tag_field = tagged_union_tag_field(ir, right).unwrap();
            if !ir_eq(ir, tag_field, r_tag_field) {
                return false;
            }
            return r_tags.iter().zip(r_structs.iter()).any(|(rt, rs)| {
                ir_eq(ir, tag, *rt) && set_conforms(ir, ctx, variant_struct, *rs)
            });
        }
        return false;
    }
    // If right is a union, check that the expanded variant conforms to some alt.
    if let Some(alts) = union_type_args(ir, right) {
        let alts = alts.to_vec();
        return alts.iter().any(|alt| {
            expanded_variant_conforms(ir, ctx, tag_field, tag, variant_struct, *alt)
        });
    }
    false
}

/// Checks that `left` conforms to an expanded variant of a tagged union.
fn expanded_variant_conforms_right(
    ir: &Ir,
    ctx: &TypeContext,
    left: InstId,
    _tag_field: InstId,
    _tag: InstId,
    variant_struct: InstId,
) -> bool {
    // left conforms to the expanded variant if it conforms to the variant struct
    // (ignoring the tag field, which is added by expansion).
    set_conforms(ir, ctx, left, variant_struct)
}

/// Checks if a constant instruction has a given base type.
fn const_has_base_type(ir: &Ir, const_id: InstId, type_id: InstId) -> bool {
    let type_name = match name_str(ir, type_id) {
        Some(n) => n,
        None => return false,
    };
    match ir.get(const_id) {
        Inst::Number(_) => type_name == "/number" || type_name == "/any",
        Inst::Float(_) => type_name == "/float64" || type_name == "/any",
        Inst::String(_) => type_name == "/string" || type_name == "/any",
        Inst::Bool(_) => type_name == "/bool" || type_name == "/any",
        Inst::Time(_) => type_name == "/time" || type_name == "/any",
        Inst::Duration(_) => type_name == "/duration" || type_name == "/any",
        Inst::Bytes(_) => type_name == "/bytes" || type_name == "/any",
        Inst::Name(n) => {
            if type_name == "/name" || type_name == "/any" {
                return true;
            }
            // Name hierarchy: /foo/bar has type /foo.
            let name = ir.resolve_name(*n);
            name.starts_with(type_name)
                && (name.len() == type_name.len()
                    || name.as_bytes().get(type_name.len()) == Some(&b'/'))
        }
        _ => false,
    }
}

/// Checks structural IR equality (same instruction kind and content).
fn ir_eq(ir: &Ir, a: InstId, b: InstId) -> bool {
    if a == b {
        return true;
    }
    match (ir.get(a), ir.get(b)) {
        (Inst::Name(na), Inst::Name(nb)) => na == nb,
        (Inst::Number(na), Inst::Number(nb)) => na == nb,
        (Inst::Float(fa), Inst::Float(fb)) => fa.to_bits() == fb.to_bits(),
        (Inst::String(sa), Inst::String(sb)) => sa == sb,
        (Inst::Bool(ba), Inst::Bool(bb)) => ba == bb,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Upper bound / Lower bound / Intersection
// ---------------------------------------------------------------------------

/// Computes the upper bound (least common supertype) of a set of type expressions.
///
/// Flattens unions, eliminates redundant types via SetConforms, returns single
/// type or Union. Returns EmptyType for empty input, `/any` if any input is `/any`.
pub fn upper_bound(ir: &mut Ir, ctx: &TypeContext, type_exprs: &[InstId]) -> InstId {
    let mut worklist = Vec::new();
    for &te in type_exprs {
        if is_any(ir, te) {
            return find_or_create_name(ir, "/any");
        }
        if let Some(args) = union_type_args(ir, te) {
            let args = args.to_vec();
            worklist.extend(args);
        } else {
            worklist.push(te);
        }
    }
    if worklist.is_empty() {
        return empty_type(ir);
    }
    let mut reduced = vec![worklist[0]];
    'outer: for &te in &worklist[1..] {
        for i in 0..reduced.len() {
            if set_conforms(ir, ctx, te, reduced[i]) {
                continue 'outer;
            }
            if set_conforms(ir, ctx, reduced[i], te) {
                reduced[i] = te;
                continue 'outer;
            }
        }
        reduced.push(te);
    }
    if reduced.len() == 1 {
        return reduced[0];
    }
    new_union_or_single(ir, reduced)
}

/// Computes the intersection (greatest lower bound) of two type expressions.
pub fn intersect_type(ir: &mut Ir, ctx: &TypeContext, a: InstId, b: InstId) -> InstId {
    if ir_eq(ir, a, b) {
        return a;
    }
    if is_any(ir, a) {
        return b;
    }
    if is_any(ir, b) {
        return a;
    }
    // Type variable resolution.
    if let Inst::Var(v) = ir.get(a) {
        let v = *v;
        if let Some(&bound) = ctx.get(&v) {
            return intersect_type(ir, ctx, bound, b);
        }
        return empty_type(ir);
    }
    if let Inst::Var(v) = ir.get(b) {
        let v = *v;
        if let Some(&bound) = ctx.get(&v) {
            return intersect_type(ir, ctx, a, bound);
        }
        return empty_type(ir);
    }
    if set_conforms(ir, ctx, a, b) {
        return a;
    }
    if set_conforms(ir, ctx, b, a) {
        return b;
    }
    // Union decomposition on a.
    if is_union_type(ir, a) {
        let args = union_type_args(ir, a).unwrap().to_vec();
        let mut res = Vec::new();
        for elem in args {
            let u = intersect_type(ir, ctx, elem, b);
            if !is_empty_type(ir, u) {
                res.push(u);
            }
        }
        return upper_bound(ir, ctx, &res);
    }
    // Union decomposition on b.
    if is_union_type(ir, b) {
        let args = union_type_args(ir, b).unwrap().to_vec();
        let mut res = Vec::new();
        for elem in args {
            if set_conforms(ir, ctx, a, elem) {
                res.push(a);
            } else if set_conforms(ir, ctx, elem, a) {
                res.push(elem);
            }
        }
        return upper_bound(ir, ctx, &res);
    }
    empty_type(ir)
}

/// Computes the lower bound (most general common subtype) of a set of type expressions.
///
/// Repeatedly intersects types. Returns EmptyType if intersection is empty.
pub fn lower_bound(ir: &mut Ir, ctx: &TypeContext, type_exprs: &[InstId]) -> InstId {
    let any = find_or_create_name(ir, "/any");
    let mut result = any;
    for &t in type_exprs {
        result = intersect_type(ir, ctx, result, t);
        if is_empty_type(ir, result) {
            return result;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// HasType — Runtime type checking
// ---------------------------------------------------------------------------

/// Checks if a concrete IR constant value matches a type expression.
///
/// This is used at system boundaries (fact loading, external input) to validate
/// that values conform to declared types. It is NOT needed for derived facts
/// when the bounds checker has already proven conformance statically.
pub fn has_type(ir: &Ir, type_expr: InstId, value: InstId) -> bool {
    // Base type (Name constant like /number, /string, ...).
    if let Inst::Name(_) = ir.get(type_expr) {
        return const_has_base_type(ir, value, type_expr);
    }

    // Type variable: accept anything (existentially quantified).
    if let Inst::Var(_) = ir.get(type_expr) {
        return true;
    }

    let fname = match apply_fn_name(ir, type_expr) {
        Some(n) => n,
        None => return false,
    };
    let args = apply_fn_args(ir, type_expr).unwrap();

    match fname {
        FN_SINGLETON => {
            args.len() == 1 && ir_eq(ir, value, args[0])
        }

        FN_UNION => args.iter().any(|alt| has_type(ir, *alt, value)),

        FN_TAGGED_UNION => {
            // Tagged unions are structs at runtime. Check each expanded variant.
            if let Some((tags, structs)) = tagged_union_variants(ir, type_expr) {
                let tag_field = tagged_union_tag_field(ir, type_expr).unwrap();
                for (tag, variant_struct) in tags.iter().zip(structs.iter()) {
                    if has_type_tagged_variant(ir, tag_field, *tag, *variant_struct, value) {
                        return true;
                    }
                }
            }
            false
        }

        FN_LIST => {
            if args.len() != 1 {
                return false;
            }
            match list_value_elems(ir, value) {
                Some(elems) => elems.iter().all(|e| has_type(ir, args[0], *e)),
                None => false,
            }
        }

        FN_MAP => {
            if args.len() != 2 {
                return false;
            }
            match map_value_entries(ir, value) {
                Some((keys, values)) => {
                    keys.iter().all(|k| has_type(ir, args[0], *k))
                        && values.iter().all(|v| has_type(ir, args[1], *v))
                }
                None => false,
            }
        }

        FN_PAIR => {
            if args.len() != 2 {
                return false;
            }
            match list_value_elems(ir, value) {
                Some(elems) if elems.len() == 2 => {
                    has_type(ir, args[0], elems[0]) && has_type(ir, args[1], elems[1])
                }
                _ => false,
            }
        }

        FN_STRUCT => has_type_struct(ir, type_expr, value),

        FN_OPTION => {
            if args.len() != 1 {
                return false;
            }
            // Option<T> matches T or /unit.
            if let Some(n) = name_str(ir, value) {
                if n == "/unit" {
                    return true;
                }
            }
            has_type(ir, args[0], value)
        }

        _ => false,
    }
}

/// Extracts `(field_name_ids, field_value_ids)` from a struct value.
///
/// Struct values have two equivalent IR representations:
/// 1. `Inst::Struct { fields, values }` — parallel vectors (brace syntax
///    lowered as a compile-time constant).
/// 2. `Inst::ApplyFn { function: "fn:struct", args: [n1, v1, n2, v2, ...] }`
///    — interleaved (brace or paren syntax lowered through the generic
///    ApplyFn path). The field names are `Inst::Name`.
///
/// Returns the element list for a list value in either representation:
/// `Inst::List(..)` (from a `Const::List`) or `Inst::ApplyFn("fn:list", ..)`.
fn list_value_elems(ir: &Ir, value: InstId) -> Option<Vec<InstId>> {
    match ir.get(value) {
        Inst::List(elems) => Some(elems.clone()),
        Inst::ApplyFn { function, args } if ir.resolve_name(*function) == "fn:list" => {
            Some(args.clone())
        }
        _ => None,
    }
}

/// Returns `(keys, values)` for a map value in either representation:
/// `Inst::Map { keys, values }` or `Inst::ApplyFn("fn:map", [k1, v1, k2, v2, ..])`.
fn map_value_entries(ir: &Ir, value: InstId) -> Option<(Vec<InstId>, Vec<InstId>)> {
    match ir.get(value) {
        Inst::Map { keys, values } => Some((keys.clone(), values.clone())),
        Inst::ApplyFn { function, args } if ir.resolve_name(*function) == "fn:map" => {
            let args = args.clone();
            if args.len() % 2 != 0 {
                return None;
            }
            let mut keys = Vec::with_capacity(args.len() / 2);
            let mut values = Vec::with_capacity(args.len() / 2);
            for pair in args.chunks_exact(2) {
                keys.push(pair[0]);
                values.push(pair[1]);
            }
            Some((keys, values))
        }
        _ => None,
    }
}

/// Returns `None` if the value is neither shape.
fn struct_value_fields(ir: &Ir, value: InstId) -> Option<(Vec<NameId>, Vec<InstId>)> {
    match ir.get(value) {
        Inst::Struct { fields, values } => Some((fields.clone(), values.clone())),
        Inst::ApplyFn { function, args } if ir.resolve_name(*function) == "fn:struct" => {
            let args = args.clone();
            if args.len() % 2 != 0 {
                return None;
            }
            let mut fields = Vec::with_capacity(args.len() / 2);
            let mut values = Vec::with_capacity(args.len() / 2);
            for pair in args.chunks_exact(2) {
                match ir.get(pair[0]) {
                    Inst::Name(n) => fields.push(*n),
                    _ => return None,
                }
                values.push(pair[1]);
            }
            Some((fields, values))
        }
        _ => None,
    }
}

/// Checks if a value matches a struct type expression.
fn has_type_struct(ir: &Ir, type_id: InstId, value: InstId) -> bool {
    let type_fields = struct_type_fields(ir, type_id);
    let (vfields, vvalues) = match struct_value_fields(ir, value) {
        Some(pair) => pair,
        None => return false,
    };

    // Build map of value fields.
    let value_map: FxHashMap<String, InstId> = vfields
        .iter()
        .zip(vvalues.iter())
        .map(|(f, v)| (ir.resolve_name(*f).to_string(), *v))
        .collect();

    // Check all required type fields are present and match.
    for (fname_id, ftype_id, is_optional) in &type_fields {
        if let Some(fname) = name_str(ir, *fname_id) {
            match value_map.get(fname) {
                Some(val_id) => {
                    if !has_type(ir, *ftype_id, *val_id) {
                        return false;
                    }
                }
                None => {
                    if !is_optional {
                        return false;
                    }
                }
            }
        }
    }

    // Check no extra fields beyond what the type declares.
    let type_field_names: FxHashSet<String> = type_fields
        .iter()
        .filter_map(|(f, _, _)| name_str(ir, *f).map(|s| s.to_string()))
        .collect();
    for vf in &vfields {
        let vf_name = ir.resolve_name(*vf);
        if !type_field_names.contains(vf_name) {
            return false;
        }
    }

    true
}

/// Checks if a struct value matches a specific tagged union variant.
fn has_type_tagged_variant(
    ir: &Ir,
    tag_field: InstId,
    tag: InstId,
    variant_struct: InstId,
    value: InstId,
) -> bool {
    let (vfields, vvalues) = match struct_value_fields(ir, value) {
        Some(pair) => pair,
        None => return false,
    };

    // Check that the tag field has the right value.
    let tag_field_name = match name_str(ir, tag_field) {
        Some(n) => n,
        None => return false,
    };

    let tag_idx = vfields
        .iter()
        .position(|f| ir.resolve_name(*f) == tag_field_name);
    match tag_idx {
        Some(idx) => {
            if !ir_eq(ir, vvalues[idx], tag) {
                return false;
            }
        }
        None => return false,
    }

    // Check that the remaining fields match the variant struct type.
    let type_fields = struct_type_fields(ir, variant_struct);

    // Build a map of value fields (excluding tag field).
    let value_map: FxHashMap<String, InstId> = vfields
        .iter()
        .zip(vvalues.iter())
        .filter(|(f, _)| ir.resolve_name(**f) != tag_field_name)
        .map(|(f, v)| (ir.resolve_name(*f).to_string(), *v))
        .collect();

    // Check variant struct fields.
    for (fname_id, ftype_id, is_optional) in &type_fields {
        if let Some(fname) = name_str(ir, *fname_id) {
            match value_map.get(fname) {
                Some(val_id) => {
                    if !has_type(ir, *ftype_id, *val_id) {
                        return false;
                    }
                }
                None => {
                    if !is_optional {
                        return false;
                    }
                }
            }
        }
    }

    // Check no extra fields beyond tag + variant fields.
    let type_field_names: FxHashSet<String> = type_fields
        .iter()
        .filter_map(|(f, _, _)| name_str(ir, *f).map(|s| s.to_string()))
        .collect();
    for vf in &vfields {
        let name = ir.resolve_name(*vf);
        if name != tag_field_name && !type_field_names.contains(name) {
            return false;
        }
    }

    true
}

// ---------------------------------------------------------------------------
// Relation type utilities (for bounds checker)
// ---------------------------------------------------------------------------

/// Builds a `fn:Rel(t1, t2, ...)` type from a slice of argument types.
pub fn new_rel_type(ir: &mut Ir, arg_types: &[InstId]) -> InstId {
    let rel_name = ir.intern_name(FN_REL);
    ir.add_inst(Inst::ApplyFn {
        function: rel_name,
        args: arg_types.to_vec(),
    })
}

/// Builds `fn:Union(alts...)` or returns the single type if only one.
pub fn new_union_or_single(ir: &mut Ir, alts: Vec<InstId>) -> InstId {
    if alts.len() == 1 {
        return alts[0];
    }
    let union_name = ir.intern_name(FN_UNION);
    ir.add_inst(Inst::ApplyFn {
        function: union_name,
        args: alts,
    })
}

/// Extracts the argument types from a `fn:Rel(t1, t2, ...)`.
pub fn rel_type_args(ir: &Ir, id: InstId) -> Option<&[InstId]> {
    if apply_fn_name(ir, id) != Some(FN_REL) {
        return None;
    }
    apply_fn_args(ir, id)
}

/// Builds `fn:Rel` types from declaration bounds.
/// Each BoundDecl becomes one `fn:Rel`.
pub fn rel_types_from_decl(ir: &mut Ir, bounds: &[InstId]) -> Vec<InstId> {
    bounds
        .iter()
        .filter_map(|bound_id| {
            if let Inst::BoundDecl { base_terms } = ir.get(*bound_id) {
                let terms = base_terms.clone();
                Some(new_rel_type(ir, &terms))
            } else {
                None
            }
        })
        .collect()
}

/// Gets the type context (all type variables mapped to `/any`) from
/// the type variables appearing in a type expression.
pub fn get_type_context(ir: &Ir, id: InstId) -> TypeContext {
    let mut ctx = TypeContext::default();
    collect_type_vars(ir, id, &mut ctx);
    ctx
}

fn collect_type_vars(ir: &Ir, id: InstId, ctx: &mut TypeContext) {
    match ir.get(id) {
        Inst::Var(v) => {
            if !ctx.contains_key(v) {
                // Map to /any as default bound. We'd need &mut Ir to create the
                // /any InstId, so we use a sentinel. The caller can handle this.
                // For now, insert with the same id as a placeholder.
                ctx.insert(*v, id);
            }
        }
        Inst::ApplyFn { args, .. } => {
            for arg in args {
                collect_type_vars(ir, *arg, ctx);
            }
        }
        _ => {}
    }
}

/// Extracts alternatives from a possibly-union-wrapped relation type.
///
/// If `id` is `fn:Union(R1, R2, ...)`, returns `[R1, R2, ...]`.
/// Otherwise returns `[id]`.
pub fn rel_type_alternatives(ir: &Ir, id: InstId) -> Vec<InstId> {
    if let Some(args) = union_type_args(ir, id) {
        args.to_vec()
    } else {
        vec![id]
    }
}

/// Creates a single relation type or union from a list of alternatives.
pub fn rel_type_from_alternatives(ir: &mut Ir, alts: Vec<InstId>) -> InstId {
    if alts.is_empty() {
        return empty_type(ir);
    }
    new_union_or_single(ir, alts)
}

/// Removes types conforming to `to_remove` from a union type.
/// Returns the remaining union, or EmptyType if nothing remains.
pub fn remove_from_union_type(ir: &mut Ir, to_remove: InstId, union_type: InstId) -> InstId {
    let ctx = TypeContext::default();
    if let Some(args) = union_type_args(ir, union_type) {
        let args = args.to_vec();
        let remaining: Vec<InstId> = args
            .into_iter()
            .filter(|alt| !set_conforms(ir, &ctx, *alt, to_remove))
            .collect();
        if remaining.is_empty() {
            return empty_type(ir);
        }
        return new_union_or_single(ir, remaining);
    }
    // Not a union: check if the whole type conforms to to_remove.
    if set_conforms(ir, &ctx, union_type, to_remove) {
        return empty_type(ir);
    }
    union_type
}

/// Creates a `fn:List(elem_type)` type expression.
pub fn new_list_type(ir: &mut Ir, elem: InstId) -> InstId {
    let name = ir.intern_name(FN_LIST);
    ir.add_inst(Inst::ApplyFn {
        function: name,
        args: vec![elem],
    })
}

/// Creates a `fn:Map(key_type, val_type)` type expression.
pub fn new_map_type(ir: &mut Ir, key: InstId, val: InstId) -> InstId {
    let name = ir.intern_name(FN_MAP);
    ir.add_inst(Inst::ApplyFn {
        function: name,
        args: vec![key, val],
    })
}

/// Creates a `fn:Struct(field1, type1, ...)` type expression.
pub fn new_struct_type(ir: &mut Ir, args: Vec<InstId>) -> InstId {
    let name = ir.intern_name(FN_STRUCT);
    ir.add_inst(Inst::ApplyFn {
        function: name,
        args,
    })
}

/// Creates a `fn:Tuple(t1, t2, ...)` type expression.
pub fn new_tuple_type(ir: &mut Ir, args: Vec<InstId>) -> InstId {
    let name = ir.intern_name(FN_TUPLE);
    ir.add_inst(Inst::ApplyFn {
        function: name,
        args,
    })
}

/// Applies a substitution (type variable -> type) to a type expression.
/// Returns a new InstId with all type variables replaced.
pub fn apply_subst(ir: &mut Ir, id: InstId, subst: &FxHashMap<NameId, InstId>) -> InstId {
    if subst.is_empty() {
        return id;
    }
    match ir.get(id) {
        Inst::Var(v) => {
            let v = *v;
            if let Some(&replacement) = subst.get(&v) {
                replacement
            } else {
                id
            }
        }
        Inst::ApplyFn { function, args } => {
            let function = *function;
            let args = args.clone();
            let new_args: Vec<InstId> = args
                .iter()
                .map(|a| apply_subst(ir, *a, subst))
                .collect();
            if new_args == args {
                return id;
            }
            ir.add_inst(Inst::ApplyFn {
                function,
                args: new_args,
            })
        }
        _ => id,
    }
}

/// Collects all type variables from a type expression into a set.
pub fn collect_vars(ir: &Ir, id: InstId, vars: &mut FxHashSet<NameId>) {
    match ir.get(id) {
        Inst::Var(v) => {
            vars.insert(*v);
        }
        Inst::ApplyFn { args, .. } => {
            for arg in args {
                collect_vars(ir, *arg, vars);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_name(ir: &mut Ir, s: &str) -> InstId {
        let n = ir.intern_name(s);
        ir.add_inst(Inst::Name(n))
    }

    fn make_apply(ir: &mut Ir, fn_name: &str, args: Vec<InstId>) -> InstId {
        let n = ir.intern_name(fn_name);
        ir.add_inst(Inst::ApplyFn {
            function: n,
            args,
        })
    }

    // -- Wellformedness tests --

    #[test]
    fn wellformed_base_types() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");
        let any = make_name(&mut ir, "/any");
        assert!(wellformed_type(&ir, &ctx, num).is_ok());
        assert!(wellformed_type(&ir, &ctx, str_).is_ok());
        assert!(wellformed_type(&ir, &ctx, any).is_ok());
    }

    #[test]
    fn wellformed_struct() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let x = make_name(&mut ir, "/x");
        let num = make_name(&mut ir, "/number");
        let y = make_name(&mut ir, "/y");
        let str_ = make_name(&mut ir, "/string");
        let s = make_apply(&mut ir, FN_STRUCT, vec![x, num, y, str_]);
        assert!(wellformed_type(&ir, &ctx, s).is_ok());
    }

    #[test]
    fn wellformed_struct_odd_args() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let x = make_name(&mut ir, "/x");
        let num = make_name(&mut ir, "/number");
        let y = make_name(&mut ir, "/y");
        // Odd number of args without fn:opt.
        let s = make_apply(&mut ir, FN_STRUCT, vec![x, num, y]);
        assert!(wellformed_type(&ir, &ctx, s).is_err());
    }

    #[test]
    fn wellformed_tagged_union() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let kind = make_name(&mut ir, "/kind");
        let move_ = make_name(&mut ir, "/move");
        let x = make_name(&mut ir, "/x");
        let num = make_name(&mut ir, "/number");
        let move_struct = make_apply(&mut ir, FN_STRUCT, vec![x, num]);
        let quit = make_name(&mut ir, "/quit");
        let quit_struct = make_apply(&mut ir, FN_STRUCT, vec![]);
        let tu = make_apply(
            &mut ir,
            FN_TAGGED_UNION,
            vec![kind, move_, move_struct, quit, quit_struct],
        );
        assert!(wellformed_type(&ir, &ctx, tu).is_ok());
    }

    #[test]
    fn wellformed_tagged_union_negative_too_few_args() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let kind = make_name(&mut ir, "/kind");
        let tu = make_apply(&mut ir, FN_TAGGED_UNION, vec![kind]);
        assert!(wellformed_type(&ir, &ctx, tu).is_err());
    }

    #[test]
    fn wellformed_tagged_union_negative_even_args() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let kind = make_name(&mut ir, "/kind");
        let move_ = make_name(&mut ir, "/move");
        let move_struct = make_apply(&mut ir, FN_STRUCT, vec![]);
        let quit = make_name(&mut ir, "/quit");
        // 4 args (even) is invalid.
        let tu = make_apply(
            &mut ir,
            FN_TAGGED_UNION,
            vec![kind, move_, move_struct, quit],
        );
        assert!(wellformed_type(&ir, &ctx, tu).is_err());
    }

    #[test]
    fn wellformed_tagged_union_negative_dup_tags() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let kind = make_name(&mut ir, "/kind");
        let move_ = make_name(&mut ir, "/move");
        let s1 = make_apply(&mut ir, FN_STRUCT, vec![]);
        let move2 = make_name(&mut ir, "/move");
        let s2 = make_apply(&mut ir, FN_STRUCT, vec![]);
        let tu = make_apply(
            &mut ir,
            FN_TAGGED_UNION,
            vec![kind, move_, s1, move2, s2],
        );
        assert!(wellformed_type(&ir, &ctx, tu).is_err());
    }

    #[test]
    fn wellformed_tagged_union_negative_tag_field_in_variant() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let kind = make_name(&mut ir, "/kind");
        let move_ = make_name(&mut ir, "/move");
        // Variant struct has /kind field — disallowed.
        let kind2 = make_name(&mut ir, "/kind");
        let num = make_name(&mut ir, "/number");
        let s = make_apply(&mut ir, FN_STRUCT, vec![kind2, num]);
        let tu = make_apply(&mut ir, FN_TAGGED_UNION, vec![kind, move_, s]);
        assert!(wellformed_type(&ir, &ctx, tu).is_err());
    }

    // -- Conformance tests --

    #[test]
    fn conforms_base_types() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let any = make_name(&mut ir, "/any");
        let str_ = make_name(&mut ir, "/string");

        assert!(set_conforms(&ir, &ctx, num, any));
        assert!(set_conforms(&ir, &ctx, num, num));
        assert!(!set_conforms(&ir, &ctx, num, str_));
    }

    #[test]
    fn conforms_name_hierarchy() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let foo = make_name(&mut ir, "/foo");
        let foo_bar = make_name(&mut ir, "/foo/bar");
        let name = make_name(&mut ir, "/name");

        assert!(set_conforms(&ir, &ctx, foo_bar, foo));
        assert!(set_conforms(&ir, &ctx, foo, name));
        assert!(set_conforms(&ir, &ctx, foo_bar, name));
        assert!(!set_conforms(&ir, &ctx, foo, foo_bar));
    }

    #[test]
    fn conforms_singleton() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let move_ = make_name(&mut ir, "/move");
        let singleton = make_apply(&mut ir, FN_SINGLETON, vec![move_]);
        let name = make_name(&mut ir, "/name");
        let num = make_name(&mut ir, "/number");

        assert!(set_conforms(&ir, &ctx, singleton, name));
        assert!(!set_conforms(&ir, &ctx, singleton, num));
    }

    #[test]
    fn conforms_union() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");
        let union = make_apply(&mut ir, FN_UNION, vec![num, str_]);

        // Union conforms to /any.
        let any = make_name(&mut ir, "/any");
        assert!(set_conforms(&ir, &ctx, union, any));
        // Number conforms to Union<number, string>.
        assert!(set_conforms(&ir, &ctx, num, union));
        // Union<number, string> does NOT conform to /number.
        assert!(!set_conforms(&ir, &ctx, union, num));
    }

    #[test]
    fn conforms_struct_subtyping() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        // .Struct</x: /number, /y: /string>
        let x = make_name(&mut ir, "/x");
        let num = make_name(&mut ir, "/number");
        let y = make_name(&mut ir, "/y");
        let str_ = make_name(&mut ir, "/string");
        let wider = make_apply(&mut ir, FN_STRUCT, vec![x, num, y, str_]);
        // .Struct</x: /number>
        let x2 = make_name(&mut ir, "/x");
        let num2 = make_name(&mut ir, "/number");
        let narrower = make_apply(&mut ir, FN_STRUCT, vec![x2, num2]);

        // Wider (more fields) conforms to narrower.
        assert!(set_conforms(&ir, &ctx, wider, narrower));
        // Narrower does NOT conform to wider (missing required /y).
        assert!(!set_conforms(&ir, &ctx, narrower, wider));
    }

    #[test]
    fn conforms_list() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let any = make_name(&mut ir, "/any");
        let list_num = make_apply(&mut ir, FN_LIST, vec![num]);
        let list_any = make_apply(&mut ir, FN_LIST, vec![any]);

        assert!(set_conforms(&ir, &ctx, list_num, list_any));
        assert!(!set_conforms(&ir, &ctx, list_any, list_num));
    }

    // -- HasType tests --

    #[test]
    fn has_type_base() {
        let mut ir = Ir::new();
        let num_type = make_name(&mut ir, "/number");
        let str_type = make_name(&mut ir, "/string");
        let val_42 = ir.add_inst(Inst::Number(42));
        let val_hello = {
            let s = ir.intern_string("hello");
            ir.add_inst(Inst::String(s))
        };

        assert!(has_type(&ir, num_type, val_42));
        assert!(!has_type(&ir, str_type, val_42));
        assert!(has_type(&ir, str_type, val_hello));
    }

    #[test]
    fn has_type_struct() {
        let mut ir = Ir::new();
        // Type: .Struct</x: /number>
        let x_name = make_name(&mut ir, "/x");
        let num_type = make_name(&mut ir, "/number");
        let struct_type = make_apply(&mut ir, FN_STRUCT, vec![x_name, num_type]);

        // Value: {/x: 42}
        let x_field = ir.intern_name("/x");
        let val_42 = ir.add_inst(Inst::Number(42));
        let val_struct = ir.add_inst(Inst::Struct {
            fields: vec![x_field],
            values: vec![val_42],
        });

        assert!(has_type(&ir, struct_type, val_struct));
    }

    #[test]
    fn has_type_tagged_union() {
        let mut ir = Ir::new();

        // TaggedUnion</kind, /move: .Struct</x: /number>, /quit: .Struct<>>
        let kind = make_name(&mut ir, "/kind");
        let move_tag = make_name(&mut ir, "/move");
        let x_name = make_name(&mut ir, "/x");
        let num_type = make_name(&mut ir, "/number");
        let move_struct = make_apply(&mut ir, FN_STRUCT, vec![x_name, num_type]);
        let quit_tag = make_name(&mut ir, "/quit");
        let quit_struct = make_apply(&mut ir, FN_STRUCT, vec![]);
        let tu = make_apply(
            &mut ir,
            FN_TAGGED_UNION,
            vec![kind, move_tag, move_struct, quit_tag, quit_struct],
        );

        // Value: {/kind: /move, /x: 10}
        let kind_f = ir.intern_name("/kind");
        let move_v = ir.intern_name("/move");
        let move_val = ir.add_inst(Inst::Name(move_v));
        let x_f = ir.intern_name("/x");
        let val_10 = ir.add_inst(Inst::Number(10));
        let val_move = ir.add_inst(Inst::Struct {
            fields: vec![kind_f, x_f],
            values: vec![move_val, val_10],
        });
        assert!(has_type(&ir, tu, val_move));

        // Value: {/kind: /quit}
        let kind_f2 = ir.intern_name("/kind");
        let quit_v = ir.intern_name("/quit");
        let quit_val = ir.add_inst(Inst::Name(quit_v));
        let val_quit = ir.add_inst(Inst::Struct {
            fields: vec![kind_f2],
            values: vec![quit_val],
        });
        assert!(has_type(&ir, tu, val_quit));

        // Value: {/kind: /bad}
        let kind_f3 = ir.intern_name("/kind");
        let bad_v = ir.intern_name("/bad");
        let bad_val = ir.add_inst(Inst::Name(bad_v));
        let val_bad = ir.add_inst(Inst::Struct {
            fields: vec![kind_f3],
            values: vec![bad_val],
        });
        assert!(!has_type(&ir, tu, val_bad));
    }

    #[test]
    fn has_type_struct_accepts_applyfn_shape() {
        // `fn:struct(/x, 42, /y, "hi")` (ApplyFn form — what the parser emits
        // for `{/x: 42, /y: "hi"}`) must satisfy .Struct</x: /number, /y: /string>.
        let mut ir = Ir::new();
        let x = make_name(&mut ir, "/x");
        let y = make_name(&mut ir, "/y");
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");
        let stype = make_apply(&mut ir, FN_STRUCT, vec![x, num, y, str_]);

        let x_v = make_name(&mut ir, "/x");
        let y_v = make_name(&mut ir, "/y");
        let n = ir.add_inst(Inst::Number(42));
        let sid = ir.intern_string("hi");
        let s = ir.add_inst(Inst::String(sid));
        let value = make_apply(&mut ir, "fn:struct", vec![x_v, n, y_v, s]);
        assert!(has_type(&ir, stype, value));
    }

    #[test]
    fn has_type_list_accepts_applyfn_shape() {
        // `fn:list(1, 2, 3)` (ApplyFn form) must satisfy fn:List(/number).
        let mut ir = Ir::new();
        let num = make_name(&mut ir, "/number");
        let list_num = make_apply(&mut ir, FN_LIST, vec![num]);
        let n1 = ir.add_inst(Inst::Number(1));
        let n2 = ir.add_inst(Inst::Number(2));
        let n3 = ir.add_inst(Inst::Number(3));
        let value = make_apply(&mut ir, "fn:list", vec![n1, n2, n3]);
        assert!(has_type(&ir, list_num, value));
    }

    #[test]
    fn conforms_tagged_union_identity() {
        // Two structurally-identical TaggedUnion types (different IR ids,
        // same tag field + variants) must conform to each other.
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let build = |ir: &mut Ir| {
            let t = make_name(ir, "/type");
            let c = make_name(ir, "/create");
            let name_f = make_name(ir, "/name");
            let str_ = make_name(ir, "/string");
            let create_s = make_apply(ir, FN_STRUCT, vec![name_f, str_]);
            let p = make_name(ir, "/ping");
            let ping_s = make_apply(ir, FN_STRUCT, vec![]);
            make_apply(ir, FN_TAGGED_UNION, vec![t, c, create_s, p, ping_s])
        };
        let tu1 = build(&mut ir);
        let tu2 = build(&mut ir);
        assert!(set_conforms(&ir, &ctx, tu1, tu2));
    }

    // -- Expansion tests --

    #[test]
    fn expand_tagged_union() {
        let mut ir = Ir::new();
        let kind = make_name(&mut ir, "/kind");
        let move_ = make_name(&mut ir, "/move");
        let x = make_name(&mut ir, "/x");
        let num = make_name(&mut ir, "/number");
        let move_struct = make_apply(&mut ir, FN_STRUCT, vec![x, num]);
        let quit = make_name(&mut ir, "/quit");
        let quit_struct = make_apply(&mut ir, FN_STRUCT, vec![]);
        let tu = make_apply(
            &mut ir,
            FN_TAGGED_UNION,
            vec![kind, move_, move_struct, quit, quit_struct],
        );

        let expanded = expand_tagged_union_type(&mut ir, tu).unwrap();
        assert!(is_union_type(&ir, expanded));
        let alts = union_type_args(&ir, expanded).unwrap();
        assert_eq!(alts.len(), 2);
        // First variant should be a struct with /kind, Singleton(/move), /x, /number.
        assert!(is_struct_type(&ir, alts[0]));
        // Second variant should be a struct with /kind, Singleton(/quit).
        assert!(is_struct_type(&ir, alts[1]));
    }

    // -- UpperBound / LowerBound tests --

    #[test]
    fn upper_bound_basic() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");

        // UpperBound of [/number] = /number.
        assert_eq!(upper_bound(&mut ir, &ctx, &[num]), num);
        // UpperBound of [/number, /string] = fn:Union(/number, /string).
        let ub = upper_bound(&mut ir, &ctx, &[num, str_]);
        assert!(is_union_type(&ir, ub));
        // UpperBound of [] = EmptyType.
        let ub_empty = upper_bound(&mut ir, &ctx, &[]);
        assert!(is_empty_type(&ir, ub_empty));
    }

    #[test]
    fn upper_bound_eliminates_redundant() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let any = make_name(&mut ir, "/any");

        // UpperBound([/number, /any]) = /any.
        let ub = upper_bound(&mut ir, &ctx, &[num, any]);
        assert!(is_any(&ir, ub));
    }

    #[test]
    fn upper_bound_name_hierarchy() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let animal = make_name(&mut ir, "/animal");
        let animal_dog = make_name(&mut ir, "/animal/dog");

        // /animal/dog <: /animal, so UpperBound = /animal.
        let ub = upper_bound(&mut ir, &ctx, &[animal, animal_dog]);
        assert_eq!(ub, animal);
    }

    #[test]
    fn lower_bound_basic() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let any = make_name(&mut ir, "/any");

        // LowerBound([/number, /any]) = /number.
        let lb = lower_bound(&mut ir, &ctx, &[num, any]);
        assert_eq!(lb, num);
    }

    #[test]
    fn lower_bound_empty() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");

        // LowerBound([/number, /string]) = EmptyType (disjoint).
        let lb = lower_bound(&mut ir, &ctx, &[num, str_]);
        assert!(is_empty_type(&ir, lb));
    }

    #[test]
    fn intersect_type_with_union() {
        let mut ir = Ir::new();
        let ctx = TypeContext::default();
        let num = make_name(&mut ir, "/number");
        let str_ = make_name(&mut ir, "/string");
        let union_ns = make_apply(&mut ir, FN_UNION, vec![num, str_]);

        // intersect(Union(/number, /string), /number) = /number.
        let result = intersect_type(&mut ir, &ctx, union_ns, num);
        assert_eq!(result, num);
    }
}
