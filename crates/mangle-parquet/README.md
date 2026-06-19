# mangle-parquet

Plain Parquet adapter for Mangle with row-group predicate pushdown.

## Overview

This crate provides `ParquetEdbSource`, which implements Mangle's `EdbSource`
trait to read plain [Parquet](https://parquet.apache.org/) files as extensional
(EDB) facts. It shares its Arrow → Mangle schema mapping with the
`mangle-delta` adapter (via the `convert` module), so Parquet and Delta Lake
data are represented identically.

## Example

```rust
use std::sync::Arc;
use mangle_db::{Database, DatabaseConfig, IdbMode, RecomputeStrategy, StoreBackend};
use mangle_parquet::ParquetEdbSource;

fn example() -> anyhow::Result<()> {
    let source = ParquetEdbSource::open(
        "/data/orders.parquet", // or a directory of .parquet files
        "orders",
    )?;

    let config = DatabaseConfig {
        name: "analytics".to_string(),
        source: r#"
            big_orders(Id, Amount) :- orders(Id, Amount, /region, _),
                                       Amount > 1000.
        "#.to_string(),
        edb_sources: vec![Arc::new(source)],
        idb_mode: IdbMode::InMemory,
        recompute: RecomputeStrategy::Full,
        store_backend: StoreBackend::InMemory,
    };

    let db = Database::open(config)?;
    let _results = db.query("big_orders")?;
    Ok(())
}
```

## How Predicate Pushdown Works

`scan_with_predicates()` uses Parquet's per-row-group min/max column statistics
to skip entire row groups that cannot satisfy a predicate (row-group pruning):

1. The Mangle compiler extracts column-level predicates that are *always*
   applied to every row from an EDB relation.
2. These `ColumnPredicate`s are passed to `scan_with_predicates()`.
3. For each row group, `ParquetEdbSource` decodes the min/max statistics for the
   predicate's column and checks whether the predicate value falls within the
   `[min, max]` range. If not, the row group is skipped.
4. The Mangle runtime re-checks predicates in memory for correctness.

Pushdown is **best-effort and always safe**: when statistics are missing, the
column type isn't supported for pruning, or the predicate/value types aren't
order-comparable, the row group is read normally. Only flat (all-primitive)
schemas are eligible for pruning; nested columns are still converted correctly
but don't participate in pruning.

## Type Mapping

Identical to `mangle-delta` (see the `convert` module):

| Arrow type | Mangle `Value` |
|---|---|
| `Int8/16/32/64` | `Value::Number(i64)` |
| `UInt8/16/32` | `Value::Number(i64)` |
| `Float32/64` | `Value::Float(f64)` |
| `Utf8/LargeUtf8` | `Value::String(String)` |
| `Timestamp(...)` | `Value::Time(i64)` (nanoseconds) |
| `Date32/64` | `Value::Time(i64)` (nanoseconds) |
| `Duration(...)` | `Value::Duration(i64)` (nanoseconds) |
| `Boolean` | `Value::Number(0 or 1)` |
| `Null` | `Value::Null` |

## Limitations

- **Read-only**: Writing Mangle-derived facts back to Parquet is not supported.
- **Schema homogeneity**: All files are assumed to share the schema of the first
  file (read at `open()` time).
- **Directory scans are non-recursive**: Only `.parquet` files directly in the
  given directory are read.
- **Pruning scope**: Only flat schemas participate in row-group pruning. `Date64`
  and `UInt64` statistics are not decoded for pruning (rows are still converted
  correctly; they just aren't used to skip row groups).
