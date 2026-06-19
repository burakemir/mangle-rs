// Integration tests for mangle-parquet.
//
// These write real Parquet files with the arrow-rs ArrowWriter and verify that
// ParquetEdbSource reads them correctly, with and without row-group pruning.

use std::sync::Arc;

use anyhow::Result;
use arrow_array::{Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use mangle_common::Value;
use mangle_db::{ColumnPredicate, EdbSource, PredicateOp};
use mangle_parquet::ParquetEdbSource;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("amount", DataType::Int64, true),
        Field::new("region", DataType::Utf8, true),
    ]))
}

/// Write rows to a parquet file. Rows are written in chunks of `chunk_size` and
/// `max_row_group_row_count` is set to `chunk_size` so each chunk becomes its
/// own row group (enabling row-group pruning tests).
fn write_parquet(
    path: &std::path::Path,
    ids: &[i64],
    amounts: &[i64],
    regions: &[&str],
    chunk_size: usize,
) -> Result<()> {
    let schema = schema();
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(chunk_size))
        .build();
    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))?;

    for start in (0..ids.len()).step_by(chunk_size) {
        let end = (start + chunk_size).min(ids.len());
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids[start..end].to_vec())),
                Arc::new(Int64Array::from(amounts[start..end].to_vec())),
                Arc::new(StringArray::from(regions[start..end].to_vec())),
            ],
        )?;
        writer.write(&batch)?;
    }
    writer.close()?;
    Ok(())
}

/// Build a source over a single file with two row groups:
///   RG1: (1, 50,  "US"), (2, 200, "US")
///   RG2: (3, 150, "EU"), (4, 3000,"EU")
/// so int and string pruning can each eliminate exactly one row group.
fn setup() -> Result<(tempfile::TempDir, ParquetEdbSource)> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("orders.parquet");
    write_parquet(
        &file,
        &[1, 2, 3, 4],
        &[50, 200, 150, 3000],
        &["US", "US", "EU", "EU"],
        2, // 2 rows per row group → 2 row groups
    )?;
    let source = ParquetEdbSource::open(&file, "orders")?;
    Ok((dir, source))
}

#[test]
fn test_full_scan() -> Result<()> {
    let (_dir, source) = setup()?;
    let rows = source.scan("orders")?;
    assert_eq!(rows.len(), 4, "full scan should return all 4 rows");
    // Spot-check the first row.
    assert_eq!(rows[0][0], Value::Number(1));
    assert_eq!(rows[0][1], Value::Number(50));
    assert_eq!(rows[0][2], Value::String("US".to_string()));
    Ok(())
}

#[test]
fn test_wrong_relation_returns_empty() -> Result<()> {
    let (_dir, source) = setup()?;
    assert!(source.scan("other")?.is_empty());
    Ok(())
}

#[test]
fn test_column_names_and_schema() -> Result<()> {
    let (_dir, source) = setup()?;
    let cols = source.column_names();
    assert_eq!(cols, &["id", "amount", "region"]);
    assert_eq!(source.schema().fields().len(), 3);
    Ok(())
}

#[test]
fn test_relations_estimated_rows_is_exact() -> Result<()> {
    let (_dir, source) = setup()?;
    let rels = source.relations()?;
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].name, "orders");
    // Parquet metadata gives an exact row count.
    assert_eq!(rels[0].estimated_rows, 4);
    Ok(())
}

#[test]
fn test_fingerprint_is_some() -> Result<()> {
    let (_dir, source) = setup()?;
    assert!(source.fingerprint()?.is_some());
    Ok(())
}

/// amount > 1000 → RG1 (max 200) is pruned; RG2 kept; in-memory re-check
/// leaves only (4, 3000).
#[test]
fn test_range_predicate_prunes_one_row_group() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(1000))];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert_eq!(rows.len(), 1, "expected exactly 1 row > 1000");
    assert_eq!(rows[0][0], Value::Number(4));
    assert_eq!(rows[0][1], Value::Number(3000));
    assert_eq!(rows[0][2], Value::String("EU".to_string()));
    Ok(())
}

/// amount > 100 → both row groups kept (RG1 has 200, RG2 has 150/3000);
/// in-memory re-check returns 3 rows.
#[test]
fn test_range_predicate_keeps_both_row_groups() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(100))];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert_eq!(rows.len(), 3, "expected 3 rows with amount > 100");
    for row in &rows {
        match &row[1] {
            Value::Number(n) => assert!(*n > 100),
            _ => panic!("amount should be a Number"),
        }
    }
    Ok(())
}

/// region == "US" → RG2 (min=max="EU") is pruned via string statistics;
/// RG1 kept; in-memory re-check returns the 2 US rows.
#[test]
fn test_string_equality_prunes_one_row_group() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(
        2,
        PredicateOp::Eq,
        Value::String("US".to_string()),
    )];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert_eq!(rows.len(), 2, "expected 2 US rows");
    for row in &rows {
        assert_eq!(row[2], Value::String("US".to_string()));
        // Both US rows come from RG1 (ids 1 and 2).
        assert!(matches!(row[0], Value::Number(n) if n == 1 || n == 2));
    }
    Ok(())
}

/// amount > 999_999 → no row group can match; all pruned; 0 rows.
#[test]
fn test_predicate_prunes_all_row_groups() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(999_999))];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert!(rows.is_empty(), "expected 0 rows");
    Ok(())
}

/// region == "EU" → RG1 pruned, RG2 kept; returns the 2 EU rows.
#[test]
fn test_string_equality_other_value() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(
        2,
        PredicateOp::Eq,
        Value::String("EU".to_string()),
    )];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row[2], Value::String("EU".to_string()));
        assert!(matches!(row[0], Value::Number(n) if n == 3 || n == 4));
    }
    Ok(())
}

/// A directory of multiple parquet files is read as the union of all files.
#[test]
fn test_directory_of_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    write_parquet(
        &dir.path().join("a.parquet"),
        &[1, 2],
        &[50, 200],
        &["US", "US"],
        2,
    )?;
    write_parquet(
        &dir.path().join("b.parquet"),
        &[3, 4],
        &[150, 3000],
        &["EU", "EU"],
        2,
    )?;

    let source = ParquetEdbSource::open(dir.path(), "orders")?;
    assert_eq!(source.paths().len(), 2);

    let rows = source.scan("orders")?;
    assert_eq!(rows.len(), 4, "union of both files should be 4 rows");

    // Predicate pushdown still works across files.
    let preds = vec![ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(1000))];
    let rows = source.scan_with_predicates("orders", &preds)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], Value::Number(3000));

    Ok(())
}

/// Neq predicates must never prune (best-effort safety): all rows returned
/// except those equal to the value.
#[test]
fn test_neq_does_not_over_prune() -> Result<()> {
    let (_dir, source) = setup()?;
    let preds = vec![ColumnPredicate::new(
        2,
        PredicateOp::Neq,
        Value::String("US".to_string()),
    )];
    let rows = source.scan_with_predicates("orders", &preds)?;
    // EU rows only.
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row[2], Value::String("EU".to_string()));
    }
    Ok(())
}
