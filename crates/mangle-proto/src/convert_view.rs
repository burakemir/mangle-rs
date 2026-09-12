//! Conversions from zero-copy buffa *view* types to internal Mangle values.
//!
//! These are the read paths used by the server on the wire side: strings and
//! bytes borrow directly from the request buffer, so decoding a mutation
//! allocates only the internal `Value` representations, never intermediate
//! wire structs.

use anyhow::{Result, bail};
use buffa::Enumeration as _;
use mangle_common::{CompoundKind, Value as MValue};

use crate::pb::mangle as pb;
use pb::{FactBatchView, FactView, ValueView};

use pb::__buffa::view::oneof::value::Kind;

/// Convert a canonical wire value (borrowed) to the internal Mangle form.
pub fn value_from_view(v: &ValueView<'_>) -> Result<MValue> {
    let Some(kind) = &v.kind else {
        return Ok(MValue::Null);
    };
    Ok(match kind {
        Kind::NumberValue(n) => MValue::Number(*n),
        Kind::FloatValue(f) => MValue::Float(*f),
        Kind::StringValue(s) => MValue::String((*s).to_string()),
        Kind::NameValue(s) => MValue::Name((*s).to_string()),
        Kind::TimeValue(t) => MValue::Time(*t),
        Kind::DurationValue(d) => MValue::Duration(*d),
        Kind::BytesValue(_) => {
            bail!("bytes values are not yet supported by the internal Value type")
        }
        Kind::BoolValue(_) => {
            bail!("bool values are not yet supported by the internal Value type")
        }
        Kind::ListValue(l) => MValue::Compound(
            CompoundKind::List,
            l.elems.iter().map(value_from_view).collect::<Result<_>>()?,
        ),
        Kind::PairValue(p) => MValue::Compound(
            CompoundKind::Pair,
            vec![
                p.first
                    .as_option()
                    .map(value_from_view)
                    .transpose()?
                    .unwrap_or(MValue::Null),
                p.second
                    .as_option()
                    .map(value_from_view)
                    .transpose()?
                    .unwrap_or(MValue::Null),
            ],
        ),
        Kind::MapValue(m) => {
            let mut elems = Vec::with_capacity(m.entries.len() * 2);
            for e in m.entries.iter() {
                elems.push(
                    e.key
                        .as_option()
                        .map(value_from_view)
                        .transpose()?
                        .unwrap_or(MValue::Null),
                );
                elems.push(
                    e.value
                        .as_option()
                        .map(value_from_view)
                        .transpose()?
                        .unwrap_or(MValue::Null),
                );
            }
            MValue::Compound(CompoundKind::Map, elems)
        }
        Kind::StructValue(s) => {
            let mut elems = Vec::with_capacity(s.fields.len() * 2);
            for f in s.fields.iter() {
                elems.push(MValue::Name(f.name.to_string()));
                elems.push(
                    f.value
                        .as_option()
                        .map(value_from_view)
                        .transpose()?
                        .unwrap_or(MValue::Null),
                );
            }
            MValue::Compound(CompoundKind::Struct, elems)
        }
    })
}

/// Decode a row-encoded `Fact` (borrowed) into (relation, tuple).
pub fn fact_from_view(fact: &FactView<'_>) -> Result<(String, Vec<MValue>)> {
    let tuple = fact
        .args
        .iter()
        .map(value_from_view)
        .collect::<Result<_>>()?;
    Ok((fact.relation.to_string(), tuple))
}

/// Decode a `FactBatch` (borrowed) into rows of internal values.
///
/// Validates that each column's selected field carries exactly `num_rows`
/// elements. Packed scalar columns are read straight off the input buffer.
pub fn rows_from_batch_view(batch: &FactBatchView<'_>) -> Result<Vec<Vec<MValue>>> {
    use pb::ColumnType;

    let n = batch.num_rows as usize;
    let mut rows = vec![Vec::with_capacity(batch.columns.len()); n];

    for (j, col) in batch.columns.iter().enumerate() {
        let ty = col.r#type.to_i32();
        for (i, row) in rows.iter_mut().enumerate() {
            let v = if ty == ColumnType::NUMBER.to_i32() {
                col.numbers
                    .iter()
                    .nth(i)
                    .map(|n| MValue::Number(*n))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::FLOAT.to_i32() {
                col.floats
                    .iter()
                    .nth(i)
                    .map(|f| MValue::Float(*f))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::STRING.to_i32() {
                col.strings
                    .iter()
                    .nth(i)
                    .map(|s| MValue::String((*s).to_string()))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::NAME.to_i32() {
                col.names
                    .iter()
                    .nth(i)
                    .map(|s| MValue::Name((*s).to_string()))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::TIME.to_i32() {
                col.times
                    .iter()
                    .nth(i)
                    .map(|t| MValue::Time(*t))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::DURATION.to_i32() {
                col.durations
                    .iter()
                    .nth(i)
                    .map(|d| MValue::Duration(*d))
                    .ok_or_else(|| column_len_err(j))?
            } else {
                col.values
                    .iter()
                    .nth(i)
                    .map(value_from_view)
                    .transpose()?
                    .ok_or_else(|| column_len_err(j))?
            };
            row.push(v);
        }
    }

    Ok(rows)
}

fn column_len_err(j: usize) -> anyhow::Error {
    anyhow::anyhow!("column {j} has fewer elements than num_rows")
}
