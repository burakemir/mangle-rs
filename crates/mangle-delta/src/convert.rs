// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS\" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Arrow → Mangle Value conversion.

use arrow_array::{
    Array, BooleanArray, Date32Array, Date64Array, DurationMicrosecondArray,
    DurationMillisecondArray, DurationNanosecondArray, DurationSecondArray, Float32Array,
    Float64Array, Int16Array, Int32Array, Int64Array, Int8Array, RecordBatch, StringArray,
    StructArray, TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray, UInt16Array, UInt32Array, UInt8Array,
};
use arrow_schema::DataType;
use mangle_common::{CompoundKind, Value};

/// Convert an Arrow `RecordBatch` into a `Vec<Vec<Value>>` (one `Vec<Value>` per row).
pub fn record_batch_to_values(batch: &RecordBatch) -> Vec<Vec<Value>> {
    let num_rows = batch.num_rows();
    let mut rows: Vec<Vec<Value>> = (0..num_rows).map(|_| Vec::with_capacity(batch.num_columns())).collect();

    for col_idx in 0..batch.num_columns() {
        let array = batch.column(col_idx);
        for (row_idx, value) in array_to_values(array).into_iter().enumerate() {
            rows[row_idx].push(value);
        }
    }

    rows
}

/// Convert an Arrow `Array` into a `Vec<Value>` (one per element).
fn array_to_values(array: &dyn Array) -> Vec<Value> {
    let data_type = array.data_type();

    match data_type {
        DataType::Null => (0..array.len()).map(|_| Value::Null).collect(),

        DataType::Boolean => {
            let arr = array.as_any().downcast_ref::<BooleanArray>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else if arr.value(i) {
                        Value::Number(1)
                    } else {
                        Value::Number(0)
                    }
                })
                .collect()
        }

        DataType::Int8 => integer_array(array),
        DataType::Int16 => integer_array(array),
        DataType::Int32 => integer_array(array),
        DataType::Int64 => integer_array(array),

        DataType::UInt8 => uint_array(array),
        DataType::UInt16 => uint_array(array),
        DataType::UInt32 => uint_array(array),

        DataType::Float32 => {
            let arr = array.as_any().downcast_ref::<Float32Array>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else {
                        Value::Float(arr.value(i) as f64)
                    }
                })
                .collect()
        }
        DataType::Float64 => {
            let arr = array.as_any().downcast_ref::<Float64Array>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else {
                        Value::Float(arr.value(i))
                    }
                })
                .collect()
        }

        DataType::Utf8 => {
            let arr = array.as_any().downcast_ref::<StringArray>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else {
                        Value::String(arr.value(i).to_string())
                    }
                })
                .collect()
        }

        DataType::Timestamp(unit, _tz) => match unit {
            arrow_schema::TimeUnit::Second => {
                let arr = array.as_any().downcast_ref::<TimestampSecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Time(arr.value(i) * 1_000_000_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Millisecond => {
                let arr = array.as_any().downcast_ref::<TimestampMillisecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Time(arr.value(i) * 1_000_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Microsecond => {
                let arr = array.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Time(arr.value(i) * 1_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Nanosecond => {
                let arr = array.as_any().downcast_ref::<TimestampNanosecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Time(arr.value(i))
                        }
                    })
                    .collect()
            }
        },

        DataType::Date32 => {
            let arr = array.as_any().downcast_ref::<Date32Array>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else {
                        // Days since epoch → nanoseconds
                        Value::Time(arr.value(i) as i64 * 86_400_000_000_000)
                    }
                })
                .collect()
        }
        DataType::Date64 => {
            let arr = array.as_any().downcast_ref::<Date64Array>().unwrap();
            (0..arr.len())
                .map(|i| {
                    if arr.is_null(i) {
                        Value::Null
                    } else {
                        // Milliseconds since epoch → nanoseconds
                        Value::Time(arr.value(i) * 1_000_000)
                    }
                })
                .collect()
        }

        DataType::Duration(unit) => match unit {
            arrow_schema::TimeUnit::Second => {
                let arr = array.as_any().downcast_ref::<DurationSecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Duration(arr.value(i) * 1_000_000_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Millisecond => {
                let arr = array.as_any().downcast_ref::<DurationMillisecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Duration(arr.value(i) * 1_000_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Microsecond => {
                let arr = array.as_any().downcast_ref::<DurationMicrosecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Duration(arr.value(i) * 1_000)
                        }
                    })
                    .collect()
            }
            arrow_schema::TimeUnit::Nanosecond => {
                let arr = array.as_any().downcast_ref::<DurationNanosecondArray>().unwrap();
                (0..arr.len())
                    .map(|i| {
                        if arr.is_null(i) {
                            Value::Null
                        } else {
                            Value::Duration(arr.value(i))
                        }
                    })
                    .collect()
            }
        },

        DataType::Struct(fields) => {
            let struct_arr = array.as_any().downcast_ref::<StructArray>().unwrap();
            let field_names: Vec<String> = fields.iter().map(|f| f.name().clone()).collect();
            let mut field_values: Vec<Vec<Value>> = Vec::with_capacity(fields.len());

            for col in struct_arr.columns() {
                field_values.push(array_to_values(col));
            }

            (0..struct_arr.len())
                .map(|i| {
                    if struct_arr.is_null(i) {
                        Value::Null
                    } else {
                        let mut elems = Vec::new();
                        for (field_idx, name) in field_names.iter().enumerate() {
                            elems.push(Value::Name(name.clone()));
                            elems.push(field_values[field_idx][i].clone());
                        }
                        Value::Compound(CompoundKind::Struct, elems)
                    }
                })
                .collect()
        }

        // Fallback: convert to string representation
        _ => {
            log::warn!("Unsupported Arrow type {:?}, converting to string", data_type);
            match arrow_cast::cast(array, &DataType::Utf8) {
                Ok(casted) => {
                    let arr = casted.as_any().downcast_ref::<StringArray>().unwrap();
                    (0..arr.len())
                        .map(|i| {
                            if arr.is_null(i) {
                                Value::Null
                            } else {
                                Value::String(arr.value(i).to_string())
                            }
                        })
                        .collect()
                }
                Err(_) => (0..array.len()).map(|_| Value::String("<unsupported>".to_string())).collect(),
            }
        }
    }
}

fn integer_array(_array: &dyn Array) -> Vec<Value> {
    let data_type = _array.data_type();
    match data_type {
        DataType::Int8 => {
            let arr = _array.as_any().downcast_ref::<Int8Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        DataType::Int16 => {
            let arr = _array.as_any().downcast_ref::<Int16Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        DataType::Int32 => {
            let arr = _array.as_any().downcast_ref::<Int32Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        DataType::Int64 => {
            let arr = _array.as_any().downcast_ref::<Int64Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i)) }).collect()
        }
        _ => unreachable!(),
    }
}

fn uint_array(_array: &dyn Array) -> Vec<Value> {
    let data_type = _array.data_type();
    match data_type {
        DataType::UInt8 => {
            let arr = _array.as_any().downcast_ref::<UInt8Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        DataType::UInt16 => {
            let arr = _array.as_any().downcast_ref::<UInt16Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        DataType::UInt32 => {
            let arr = _array.as_any().downcast_ref::<UInt32Array>().unwrap();
            (0..arr.len()).map(|i| if arr.is_null(i) { Value::Null } else { Value::Number(arr.value(i) as i64) }).collect()
        }
        _ => unreachable!(),
    }
}
