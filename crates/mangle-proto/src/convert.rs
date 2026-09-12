//! Conversions between the canonical wire types (owned) and
//! `mangle_common::Value` / `Vec<Value>` tuples.

use anyhow::{Result, bail};
use buffa::Enumeration as _;
use mangle_common::{CompoundKind, Value as MValue};

use crate::pb::mangle as pb;
use pb::{
    Column, ColumnType, Entry, Fact, FactBatch, Field, ListValue, MapValue, PairValue, StructValue,
    Value as PValue,
};

use pb::__buffa::oneof::value::Kind;

// --- Value (row encoding) ---

/// Convert an internal Mangle value to the canonical wire form.
///
/// `Null` maps to a `Value` with no `kind` set (the wire encoding of null).
pub fn value_to_proto(v: &MValue) -> PValue {
    let kind = match v {
        MValue::Number(n) => Kind::NumberValue(*n),
        MValue::Float(f) => Kind::FloatValue(*f),
        MValue::String(s) => Kind::StringValue(s.clone()),
        MValue::Name(s) => Kind::NameValue(s.clone()),
        MValue::Time(t) => Kind::TimeValue(*t),
        MValue::Duration(d) => Kind::DurationValue(*d),
        MValue::Null => return PValue::default(),
        MValue::Compound(kind, elems) => match kind {
            CompoundKind::List => Kind::ListValue(Box::new(ListValue {
                elems: elems.iter().map(value_to_proto).collect(),
                ..Default::default()
            })),
            CompoundKind::Pair if elems.len() == 2 => Kind::PairValue(Box::new(PairValue {
                first: value_to_proto(&elems[0]).into(),
                second: value_to_proto(&elems[1]).into(),
                ..Default::default()
            })),
            // Defensive: a "pair" that is not 2 elements is carried as a list
            // rather than silently dropping data.
            CompoundKind::Pair => Kind::ListValue(Box::new(ListValue {
                elems: elems.iter().map(value_to_proto).collect(),
                ..Default::default()
            })),
            CompoundKind::Map => Kind::MapValue(Box::new(MapValue {
                entries: elems
                    .chunks_exact(2)
                    .map(|kv| Entry {
                        key: value_to_proto(&kv[0]).into(),
                        value: value_to_proto(&kv[1]).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })),
            CompoundKind::Struct => Kind::StructValue(Box::new(StructValue {
                fields: elems
                    .chunks_exact(2)
                    .map(|kv| Field {
                        name: struct_key_name(&kv[0]),
                        value: value_to_proto(&kv[1]).into(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })),
        },
    };
    PValue {
        kind: Some(kind),
        ..Default::default()
    }
}

/// The wire form of a struct field name: names are names, everything else
/// falls back to `Display` (field names should always be names in practice).
fn struct_key_name(v: &MValue) -> String {
    match v {
        MValue::Name(s) | MValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Convert a canonical wire value to the internal Mangle form.
///
/// An unset `kind` maps to `Value::Null`. `bytes` and `bool` wire values are
/// rejected until `mangle_common::Value` grows the corresponding variants
/// (the wire format reserves them; see RPC_DESIGN.md §9).
pub fn value_from_proto(v: &PValue) -> Result<MValue> {
    let Some(kind) = &v.kind else {
        return Ok(MValue::Null);
    };
    Ok(match kind {
        Kind::NumberValue(n) => MValue::Number(*n),
        Kind::FloatValue(f) => MValue::Float(*f),
        Kind::StringValue(s) => MValue::String(s.clone()),
        Kind::NameValue(s) => MValue::Name(s.clone()),
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
            l.elems
                .iter()
                .map(value_from_proto)
                .collect::<Result<_>>()?,
        ),
        Kind::PairValue(p) => MValue::Compound(
            CompoundKind::Pair,
            vec![value_from_proto(&p.first)?, value_from_proto(&p.second)?],
        ),
        Kind::MapValue(m) => {
            let mut elems = Vec::with_capacity(m.entries.len() * 2);
            for e in &m.entries {
                elems.push(value_from_proto(&e.key)?);
                elems.push(value_from_proto(&e.value)?);
            }
            MValue::Compound(CompoundKind::Map, elems)
        }
        Kind::StructValue(s) => {
            let mut elems = Vec::with_capacity(s.fields.len() * 2);
            for f in &s.fields {
                elems.push(MValue::Name(f.name.clone()));
                elems.push(value_from_proto(&f.value)?);
            }
            MValue::Compound(CompoundKind::Struct, elems)
        }
    })
}

// --- Fact (row encoding) ---

/// Build a row-encoded `Fact` from a relation name and a tuple.
pub fn fact_to_proto(relation: &str, row: &[MValue]) -> Fact {
    Fact {
        relation: relation.to_string(),
        args: row.iter().map(value_to_proto).collect(),
        ..Default::default()
    }
}

/// Decode a row-encoded `Fact` into (relation, tuple).
pub fn fact_from_row(fact: &Fact) -> Result<(String, Vec<MValue>)> {
    let tuple = fact
        .args
        .iter()
        .map(value_from_proto)
        .collect::<Result<_>>()?;
    Ok((fact.relation.clone(), tuple))
}

// --- FactBatch (columnar encoding) ---

/// Build a columnar `FactBatch` for `relation` from a set of rows.
///
/// Column types are *inferred* from the data: a column whose values are all
/// the same scalar type is emitted as a packed column; anything else
/// (including compounds, nulls, empty columns) is emitted as `ANY` with
/// row-encoded values. Once the server resolves declared column types from
/// the program schema, that information should be used instead (see
/// RPC_DESIGN.md §3).
pub fn batch_from_rows(relation: &str, rows: &[Vec<MValue>]) -> FactBatch {
    let arity = rows.first().map_or(0, |r| r.len());
    let mut columns = Vec::with_capacity(arity);

    for j in 0..arity {
        // A column is emitted as packed iff every value has the same scalar
        // type; anything else (mixed, compound, null) goes through `ANY`.
        let uniform = rows
            .first()
            .and_then(|r| scalar_type(&r[j]))
            .filter(|ty| rows.iter().all(|r| scalar_type(&r[j]) == Some(*ty)));

        let col = match uniform {
            Some(ColumnType::NUMBER) => Column {
                r#type: ColumnType::NUMBER.into(),
                numbers: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::Number(n) => *n,
                        _ => unreachable!("classified NUMBER above"),
                    })
                    .collect(),
                ..Default::default()
            },
            Some(ColumnType::FLOAT) => Column {
                r#type: ColumnType::FLOAT.into(),
                floats: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::Float(f) => *f,
                        _ => unreachable!("classified FLOAT above"),
                    })
                    .collect(),
                ..Default::default()
            },
            Some(ColumnType::STRING) => Column {
                r#type: ColumnType::STRING.into(),
                strings: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::String(s) => s.clone(),
                        _ => unreachable!("classified STRING above"),
                    })
                    .collect(),
                ..Default::default()
            },
            Some(ColumnType::NAME) => Column {
                r#type: ColumnType::NAME.into(),
                names: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::Name(s) => s.clone(),
                        _ => unreachable!("classified NAME above"),
                    })
                    .collect(),
                ..Default::default()
            },
            Some(ColumnType::TIME) => Column {
                r#type: ColumnType::TIME.into(),
                times: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::Time(t) => *t,
                        _ => unreachable!("classified TIME above"),
                    })
                    .collect(),
                ..Default::default()
            },
            Some(ColumnType::DURATION) => Column {
                r#type: ColumnType::DURATION.into(),
                durations: rows
                    .iter()
                    .map(|r| match &r[j] {
                        MValue::Duration(d) => *d,
                        _ => unreachable!("classified DURATION above"),
                    })
                    .collect(),
                ..Default::default()
            },
            _ => Column {
                r#type: ColumnType::ANY.into(),
                values: rows.iter().map(|r| value_to_proto(&r[j])).collect(),
                ..Default::default()
            },
        };
        columns.push(col);
    }

    FactBatch {
        relation: relation.to_string(),
        num_rows: rows.len() as u32,
        columns,
        ..Default::default()
    }
}

/// The packed column type of a scalar value, or `None` for compounds/null.
fn scalar_type(v: &MValue) -> Option<ColumnType> {
    match v {
        MValue::Number(_) => Some(ColumnType::NUMBER),
        MValue::Float(_) => Some(ColumnType::FLOAT),
        MValue::String(_) => Some(ColumnType::STRING),
        MValue::Name(_) => Some(ColumnType::NAME),
        MValue::Time(_) => Some(ColumnType::TIME),
        MValue::Duration(_) => Some(ColumnType::DURATION),
        _ => None,
    }
}

/// Decode a `FactBatch` into rows of internal values.
///
/// Validates that each column's selected field carries exactly `num_rows`
/// elements and that the batch is self-consistent.
pub fn rows_from_batch(batch: &FactBatch) -> Result<Vec<Vec<MValue>>> {
    let n = batch.num_rows as usize;
    let mut rows = vec![Vec::with_capacity(batch.columns.len()); n];

    for (j, col) in batch.columns.iter().enumerate() {
        let ty = col.r#type.to_i32();
        // Determine the element at each row index for this column.
        for (i, row) in rows.iter_mut().enumerate() {
            let v = if ty == ColumnType::NUMBER.to_i32() {
                col.numbers
                    .get(i)
                    .map(|n| MValue::Number(*n))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::FLOAT.to_i32() {
                col.floats
                    .get(i)
                    .map(|f| MValue::Float(*f))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::STRING.to_i32() {
                col.strings
                    .get(i)
                    .map(|s| MValue::String(s.clone()))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::NAME.to_i32() {
                col.names
                    .get(i)
                    .map(|s| MValue::Name(s.clone()))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::TIME.to_i32() {
                col.times
                    .get(i)
                    .map(|t| MValue::Time(*t))
                    .ok_or_else(|| column_len_err(j))?
            } else if ty == ColumnType::DURATION.to_i32() {
                col.durations
                    .get(i)
                    .map(|d| MValue::Duration(*d))
                    .ok_or_else(|| column_len_err(j))?
            } else {
                // ANY (and BYTES/BOOL, not yet representable internally).
                col.values
                    .get(i)
                    .map(value_from_proto)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: MValue) {
        let p = value_to_proto(&v);
        let back = value_from_proto(&p).unwrap();
        assert_eq!(back, v, "round-trip failed for {v:?}");
    }

    #[test]
    fn value_roundtrips() {
        roundtrip(MValue::Null);
        roundtrip(MValue::Number(42));
        roundtrip(MValue::Number(-7));
        roundtrip(MValue::Float(2.5));
        roundtrip(MValue::String("hello".into()));
        roundtrip(MValue::Name("/role/admin".into()));
        roundtrip(MValue::Time(1_700_000_000_000_000_000));
        roundtrip(MValue::Duration(90_000_000_000));
        roundtrip(MValue::Compound(
            CompoundKind::List,
            vec![MValue::Number(1), MValue::String("x".into())],
        ));
        roundtrip(MValue::Compound(
            CompoundKind::Pair,
            vec![MValue::Number(1), MValue::Number(2)],
        ));
        roundtrip(MValue::Compound(
            CompoundKind::Map,
            vec![
                MValue::Name("a".into()),
                MValue::Number(1),
                MValue::Name("b".into()),
                MValue::Number(2),
            ],
        ));
        roundtrip(MValue::Compound(
            CompoundKind::Struct,
            vec![
                MValue::Name("x".into()),
                MValue::Number(1),
                MValue::Name("y".into()),
                MValue::String("s".into()),
            ],
        ));
    }

    #[test]
    fn name_and_string_are_distinct() {
        let p = value_to_proto(&MValue::Name("/n".into()));
        assert!(matches!(
            p.kind.as_ref().unwrap(),
            Kind::NameValue(s) if s == "/n"
        ));
        let p = value_to_proto(&MValue::String("/n".into()));
        assert!(matches!(
            p.kind.as_ref().unwrap(),
            Kind::StringValue(s) if s == "/n"
        ));
    }

    #[test]
    fn batch_roundtrip_typed_columns() {
        let rows = vec![
            vec![MValue::Number(1), MValue::String("a".into())],
            vec![MValue::Number(2), MValue::String("b".into())],
        ];
        let batch = batch_from_rows("r", &rows);
        assert_eq!(batch.num_rows, 2);
        assert_eq!(batch.columns.len(), 2);
        let back = rows_from_batch(&batch).unwrap();
        assert_eq!(back, rows);
    }

    #[test]
    fn batch_roundtrip_mixed_column_falls_back_to_any() {
        let rows = vec![
            vec![MValue::Number(1), MValue::String("a".into())],
            vec![MValue::String("x".into()), MValue::String("b".into())],
        ];
        let batch = batch_from_rows("r", &rows);
        let back = rows_from_batch(&batch).unwrap();
        assert_eq!(back, rows);
        // First column is heterogeneous -> ANY.
        assert_eq!(batch.columns[0].r#type.to_i32(), ColumnType::ANY.to_i32());
    }

    #[test]
    fn batch_empty() {
        let batch = batch_from_rows("r", &[]);
        assert_eq!(batch.num_rows, 0);
        assert!(batch.columns.is_empty());
        assert!(rows_from_batch(&batch).unwrap().is_empty());
    }

    #[test]
    fn fact_roundtrip() {
        let fact = fact_to_proto(
            "route",
            &[MValue::String("GET".into()), MValue::Name("/api".into())],
        );
        let (rel, tuple) = fact_from_row(&fact).unwrap();
        assert_eq!(rel, "route");
        assert_eq!(
            tuple,
            vec![MValue::String("GET".into()), MValue::Name("/api".into())]
        );
    }

    #[test]
    fn batch_rejects_short_column() {
        let mut batch = batch_from_rows("r", &[vec![MValue::Number(1)]]);
        batch.num_rows = 2;
        assert!(rows_from_batch(&batch).is_err());
    }

    // Wire round-trip: encode the proto message and decode it back via the
    // zero-copy view path, exercising the generated codecs.
    #[test]
    fn batch_wire_roundtrip() {
        use buffa::Message;

        let rows = vec![
            vec![MValue::Number(1), MValue::String("a".into())],
            vec![MValue::Number(2), MValue::String("b".into())],
        ];
        let batch = batch_from_rows("r", &rows);
        let bytes = batch.encode_to_vec();
        let decoded = FactBatch::decode_from_slice(&bytes).unwrap();
        assert_eq!(rows_from_batch(&decoded).unwrap(), rows);
    }
}
