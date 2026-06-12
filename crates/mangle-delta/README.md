# mangle-delta

Delta Lake adapter for Mangle with predicate pushdown.

## Overview

This crate provides `DeltaEdbSource`, which implements Mangle's `EdbSource` trait
to read from [Delta Lake](https://delta.io/) tables as extensional (EDB) facts.

## ⚠️ Predicate Pushdown is Mandatory

**A realistic Delta Lake table is far too large for Mangle's edge-mode
in-memory processing.** Delta tables routinely contain millions to billions
of rows across hundreds or thousands of Parquet files. Mangle's Edge Mode
stores all facts in a `MemStore` (a `HashMap` in RAM). Loading an entire
Delta table into memory is neither feasible nor desirable.

The only practical way to use Delta Lake with Mangle is through **predicate
pushdown**: the Mangle compiler analyzes the physical plan and extracts
column-level predicates that are *always* applied to every row from an EDB
relation. These predicates are passed to `DeltaEdbSource::scan_with_predicates()`,
which exploits Delta Lake's multi-level pushdown capabilities:

| Pushdown level | Mechanism | What it skips |
|---|---|---|
| **Partition pruning** | `PartitionFilter` on partition columns | Entire directories of Parquet files |
| **File-level data skipping** | Min/max/null statistics in Delta log | Entire Parquet files |
| **Row-group pruning** | Parquet row-group min/max stats | Row groups within a Parquet file |
| **Parquet predicate pushdown** | Page-level min/max in Parquet | Individual data pages |

## Example

```rust
use std::sync::Arc;
use mangle_db::{Database, DatabaseConfig, IdbMode, RecomputeStrategy, StoreBackend};
use mangle_delta::DeltaEdbSource;

async fn example() -> anyhow::Result<()> {
    let source = DeltaEdbSource::open(
        "s3://my-bucket/delta/orders",
        "orders",
        std::collections::HashMap::new(),
    ).await?;

    let config = DatabaseConfig {
        name: "analytics".to_string(),
        source: r#"
            large_us_orders(Id, Amount) :- orders(Id, Amount, /region, "US"),
                                               Amount > 1000.
        "#.to_string(),
        edb_sources: vec![Arc::new(source)],
        idb_mode: IdbMode::InMemory,
        recompute: RecomputeStrategy::Full,
        store_backend: StoreBackend::InMemory,
    };

    let db = Database::open(config)?;
    let results = db.query("large_us_orders")?;
    Ok(())
}
```

## How Predicate Pushdown Works

1. The Mangle compiler compiles the source program into a physical plan (Op tree).
2. The `mangle-db` predicate extraction pass walks the Op tree and finds
   column-level filters that are always applied to EDB scans.
3. These `ColumnPredicate`s are passed to `EdbSource::scan_with_predicates()`.
4. `DeltaEdbSource` translates them into:
   - `PartitionFilter`s for equality predicates on partition columns → **partition pruning**
   - DataFusion SQL `WHERE` clauses → **data skipping + Parquet pushdown**
5. The Mangle runtime re-checks predicates in-memory for correctness.

## Type Mapping

| Arrow/Delta type | Mangle `Value` |
|---|---|
| `Int8/16/32/64` | `Value::Number(i64)` |
| `UInt8/16/32` | `Value::Number(i64)` |
| `Float32/64` | `Value::Float(f64)` |
| `Utf8` | `Value::String(String)` |
| `Timestamp(...)` | `Value::Time(i64)` (nanoseconds) |
| `Date32/64` | `Value::Time(i64)` (nanoseconds) |
| `Duration(...)` | `Value::Duration(i64)` (nanoseconds) |
| `Boolean` | `Value::Number(0 or 1)` |
| `Struct(...)` | `Value::Compound(Struct, [...])` |

## Limitations

- **Read-only**: Writing Mangle-derived facts back to Delta Lake is not yet supported.
- **Async/sync bridge**: Uses `tokio::runtime::Runtime::block_on()` internally.
- **Full scans are dangerous**: Calling `scan()` without predicates loads the
  entire table into memory. Always write Mangle rules that filter EDB
  relations so that predicates can be extracted.
