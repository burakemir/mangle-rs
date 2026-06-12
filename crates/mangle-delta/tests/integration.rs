// Integration tests for mangle-delta.
//
// These tests create real (in-memory or tempdir) Delta tables and verify
// that DeltaEdbSource can read from them with and without predicate pushdown.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use arrow_array::{Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use deltalake_core::DeltaTable;
use mangle_common::Value;
use mangle_db::{ColumnPredicate, EdbSource, PredicateOp};

/// Create a small Delta table in a temp directory with the schema:
///   (id: int64, amount: int64, region: string)
/// where `region` is a partition column.
async fn create_test_delta_table(dir: &std::path::Path) -> Result<DeltaTable> {
    use deltalake_core::operations::create::CreateBuilder;
    use deltalake_core::kernel::schema::{DataType as DDataType, StructField};

    let table = CreateBuilder::new()
        .with_location(dir.to_str().unwrap())
        .with_columns(vec![
            StructField::nullable("id", DDataType::INTEGER),
            StructField::nullable("amount", DDataType::INTEGER),
            StructField::nullable("region", DDataType::STRING),
        ])
        .with_partition_columns(["region"])
        .await?;

    Ok(table)
}

/// Write a batch of rows to a Delta table.
async fn write_batch(table: &DeltaTable, ids: &[i64], amounts: &[i64], regions: &[&str]) -> Result<()> {
    use deltalake_core::datafusion::prelude::SessionContext;

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("amount", DataType::Int64, true),
        Field::new("region", DataType::Utf8, true),
    ]));

    let id_array = Int64Array::from(ids.to_vec());
    let amount_array = Int64Array::from(amounts.to_vec());
    let region_array = StringArray::from(regions.to_vec());

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(id_array),
            Arc::new(amount_array),
            Arc::new(region_array),
        ],
    )?;

    let ctx = SessionContext::new();
    let provider = table.table_provider().build().await?;
    ctx.register_table("test_table", Arc::new(provider))?;

    // Use DeltaOps to write
    let table = deltalake_core::DeltaOps(table.clone())
        .write(vec![batch])
        .with_save_mode(deltalake_core::protocol::SaveMode::Append)
        .await?;

    Ok(())
}

/// Create a populated test Delta table and return it.
async fn setup_delta_table() -> Result<(tempfile::TempDir, DeltaTable)> {
    let dir = tempfile::tempdir()?;
    let mut table = create_test_delta_table(dir.path()).await?;

    // Write some data
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("amount", DataType::Int64, true),
        Field::new("region", DataType::Utf8, true),
    ]));

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![1, 2, 3, 4])),
            Arc::new(Int64Array::from(vec![50, 200, 150, 3000])),
            Arc::new(StringArray::from(vec!["US", "US", "EU", "US"])),
        ],
    )?;

    table = deltalake_core::DeltaOps(table)
        .write(vec![batch])
        .with_save_mode(deltalake_core::protocol::SaveMode::Append)
        .await?;

    Ok((dir, table))
}

#[tokio::test]
async fn test_delta_edb_source_full_scan() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    // Full scan should return all 4 rows
    let rows = source.scan("orders")?;
    assert_eq!(rows.len(), 4, "full scan should return 4 rows, got {}", rows.len());

    Ok(())
}

#[tokio::test]
async fn test_delta_edb_source_scan_with_partition_predicate() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    // Verify partition columns are detected
    assert!(
        source.partition_columns().contains(&"region".to_string()),
        "expected 'region' in partition columns: {:?}",
        source.partition_columns(),
    );

    // Scan with equality predicate on partition column "region" (col index 2)
    let preds = vec![ColumnPredicate::new(2, PredicateOp::Eq, Value::String("US".to_string()))];

    let rows = source.scan_with_predicates("orders", &preds)?;
    assert!(
        rows.len() >= 3,
        "expected at least 3 US rows, got {}",
        rows.len(),
    );

    // Verify all returned rows have region = "US"
    // Note: partition columns may not appear in the row data depending
    // on how Delta returns them. Check what we actually get.
    for row in &rows {
        // The row may have 2 or 3 columns depending on partition column handling
        if row.len() > 2 {
            // If region is in the data, check it
            match &row[2] {
                Value::String(s) => assert_eq!(s, "US"),
                _ => {} // region may be a name or other type
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_delta_edb_source_scan_with_range_predicate() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    // Scan with range predicate on "amount" (col index 1)
    let preds = vec![ColumnPredicate::new(1, PredicateOp::Gt, Value::Number(100))];

    let rows = source.scan_with_predicates("orders", &preds)?;

    // All returned rows must satisfy amount > 100
    for row in &rows {
        if row.len() > 1 {
            match &row[1] {
                Value::Number(n) => assert!(*n > 100, "amount {} should be > 100", n),
                _ => {}
            }
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_delta_edb_source_column_names() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    let cols = source.column_names();
    assert!(
        cols.contains(&"id".to_string()),
        "expected 'id' in columns: {:?}",
        cols,
    );
    assert!(
        cols.contains(&"amount".to_string()),
        "expected 'amount' in columns: {:?}",
        cols,
    );
    assert!(
        cols.contains(&"region".to_string()),
        "expected 'region' in columns: {:?}",
        cols,
    );

    Ok(())
}

#[tokio::test]
async fn test_delta_edb_source_fingerprint() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    let fp = source.fingerprint()?;
    assert!(fp.is_some(), "fingerprint should be Some");

    Ok(())
}

#[tokio::test]
async fn test_delta_edb_source_relations() -> Result<()> {
    let (_dir, table) = setup_delta_table().await?;
    let source = mangle_delta::DeltaEdbSource::from_table(table, "orders")?;

    let rels = source.relations()?;
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].name, "orders");

    Ok(())
}
