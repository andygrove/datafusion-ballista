// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Deterministic parquet fixtures for the join test suite.
//!
//! Every table is written as several parquet files inside one directory so that
//! a scan starts out with more than one partition. That matters: a single-file
//! scan hides repartitioning bugs, because there is nothing to redistribute.
//!
//! Values come from a fixed-seed LCG rather than the `rand` crate. No new
//! dependency, and a failing case reproduces byte-for-byte on the next run.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, Int32Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use tempfile::TempDir;

/// Number of parquet files written per table.
const FILES_PER_TABLE: usize = 3;

/// A tiny linear congruential generator, so fixtures are reproducible without
/// pulling in `rand`. Constants are the ones from Knuth's MMIX.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Returns a value in `0..n`.
    fn next_below(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as u32) % n
    }

    /// Returns true with probability `percent`/100.
    fn chance(&mut self, percent: u32) -> bool {
        self.next_below(100) < percent
    }
}

/// The set of tables the join suite registers.
pub struct Fixtures {
    dir: TempDir,
}

impl Fixtures {
    /// Generates every fixture table into a fresh temporary directory.
    pub fn generate() -> datafusion::common::Result<Self> {
        let dir = tempfile::tempdir().expect("create fixture tempdir");
        let root = dir.path();

        write_table(root, "l", &probe_schema(true), left_batches())?;
        write_table(root, "r", &build_schema(true), right_batches())?;
        write_table(
            root,
            "r_nonull",
            &build_schema(false),
            right_nonull_batches(),
        )?;
        write_table(root, "empty", &build_schema(true), empty_batches())?;
        write_table(root, "all_null", &build_schema(true), all_null_batches())?;
        write_table(root, "skew", &build_schema(true), skew_batches())?;
        write_table(root, "comp", &composite_schema(), composite_batches())?;

        Ok(Self { dir })
    }

    /// Registers every fixture table on `ctx` under its own name.
    pub async fn register(&self, ctx: &SessionContext) -> datafusion::common::Result<()> {
        for name in TABLE_NAMES {
            let path = self.dir.path().join(name);
            ctx.register_parquet(
                *name,
                path.to_str().expect("fixture path is valid utf-8"),
                ParquetReadOptions::default(),
            )
            .await?;
        }
        Ok(())
    }
}

/// Every table name written by [`Fixtures::generate`].
pub const TABLE_NAMES: &[&str] =
    &["l", "r", "r_nonull", "empty", "all_null", "skew", "comp"];

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

/// Probe-side schema: nullable join key, an int payload, a string payload.
fn probe_schema(key_nullable: bool) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("k", DataType::Int32, key_nullable),
        Field::new("v", DataType::Int32, false),
        Field::new("s", DataType::Utf8, true),
    ]))
}

/// Build-side schema. Deliberately different column names from the probe side
/// so that `SELECT *` results are unambiguous and join conditions must be
/// written out explicitly.
fn build_schema(key_nullable: bool) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("k", DataType::Int32, key_nullable),
        Field::new("w", DataType::Int32, false),
        Field::new("t", DataType::Utf8, true),
    ]))
}

/// Composite-key schema, both key columns nullable.
fn composite_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("k1", DataType::Int32, true),
        Field::new("k2", DataType::Utf8, true),
        Field::new("v", DataType::Int32, false),
    ]))
}

// ---------------------------------------------------------------------------
// Data generation
// ---------------------------------------------------------------------------

/// 1000 rows, key drawn from `0..200` with ~15% NULL. Keys repeat, so both
/// sides carry duplicates and the join has to produce a genuine cross product
/// per key rather than a one-to-one match.
fn left_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0001);
    let schema = probe_schema(true);
    build_int_keyed(
        &schema,
        1000,
        "v",
        |rng| {
            if rng.chance(15) {
                None
            } else {
                Some(rng.next_below(200) as i32)
            }
        },
        &mut rng,
    )
}

/// 1000 rows, key drawn from `100..300` with ~15% NULL, so `100..200` overlaps
/// the left side and the rest does not. Both a matching and a non-matching
/// region exist for outer joins to exercise.
fn right_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0002);
    let schema = build_schema(true);
    build_int_keyed(
        &schema,
        1000,
        "w",
        |rng| {
            if rng.chance(15) {
                None
            } else {
                Some(100 + rng.next_below(200) as i32)
            }
        },
        &mut rng,
    )
}

/// The control for null-aware cases: identical shape to `r` but with no NULL
/// keys at all. `NOT IN` against this must return rows; against `r` it must
/// return none.
fn right_nonull_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0003);
    let schema = build_schema(false);
    build_int_keyed(
        &schema,
        800,
        "w",
        |rng| Some(100 + rng.next_below(200) as i32),
        &mut rng,
    )
}

/// Zero rows, correct schema. Drives the empty-stage and `PropagateEmptyExec`
/// paths.
fn empty_batches() -> Vec<RecordBatch> {
    Vec::new()
}

/// 200 rows whose key is entirely NULL. The extreme null-aware case.
fn all_null_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0004);
    let schema = build_schema(true);
    build_int_keyed(&schema, 200, "w", |_| None, &mut rng)
}

/// 2000 rows where 80% share a single key. Produces one very large hash
/// partition and many small ones, which is what the AQE coalesce rule and the
/// per-task partition cap are supposed to cope with.
fn skew_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0005);
    let schema = build_schema(true);
    build_int_keyed(
        &schema,
        2000,
        "w",
        |rng| {
            if rng.chance(80) {
                Some(42)
            } else {
                Some(rng.next_below(200) as i32)
            }
        },
        &mut rng,
    )
}

/// 1000 rows with a two-column key, both nullable, so composite-key hash
/// repartitioning is exercised alongside NULL handling.
fn composite_batches() -> Vec<RecordBatch> {
    let mut rng = Lcg::new(0x5eed_0006);
    let schema = composite_schema();
    let total = 1000;

    let mut k1 = Vec::with_capacity(total);
    let mut k2 = Vec::with_capacity(total);
    let mut v = Vec::with_capacity(total);
    for i in 0..total {
        k1.push(if rng.chance(15) {
            None
        } else {
            Some(rng.next_below(50) as i32)
        });
        k2.push(if rng.chance(15) {
            None
        } else {
            Some(["a", "b", "c", "d", "e"][rng.next_below(5) as usize].to_string())
        });
        v.push(i as i32);
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int32Array::from(k1)),
        Arc::new(StringArray::from(k2)),
        Arc::new(Int32Array::from(v)),
    ];
    split_into_batches(&schema, columns, total)
}

/// Shared generator for the three-column `(key, int payload, string payload)`
/// tables. `key_fn` decides each row's key, including whether it is NULL.
fn build_int_keyed(
    schema: &SchemaRef,
    total: usize,
    _payload_name: &str,
    mut key_fn: impl FnMut(&mut Lcg) -> Option<i32>,
    rng: &mut Lcg,
) -> Vec<RecordBatch> {
    let mut keys = Vec::with_capacity(total);
    let mut payload = Vec::with_capacity(total);
    let mut text = Vec::with_capacity(total);

    for i in 0..total {
        keys.push(key_fn(rng));
        payload.push(i as i32);
        text.push(if rng.chance(10) {
            None
        } else {
            Some(format!("s{}", i % 37))
        });
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int32Array::from(keys)),
        Arc::new(Int32Array::from(payload)),
        Arc::new(StringArray::from(text)),
    ];
    split_into_batches(schema, columns, total)
}

/// Slices the fully-built columns into [`FILES_PER_TABLE`] batches of roughly
/// equal size, one per output file.
fn split_into_batches(
    schema: &SchemaRef,
    columns: Vec<ArrayRef>,
    total: usize,
) -> Vec<RecordBatch> {
    let chunk = total.div_ceil(FILES_PER_TABLE);
    let mut batches = Vec::with_capacity(FILES_PER_TABLE);
    let mut offset = 0;
    while offset < total {
        let len = chunk.min(total - offset);
        let sliced: Vec<ArrayRef> =
            columns.iter().map(|c| c.slice(offset, len)).collect();
        batches.push(
            RecordBatch::try_new(schema.clone(), sliced).expect("fixture batch is valid"),
        );
        offset += len;
    }
    batches
}

// ---------------------------------------------------------------------------
// Parquet output
// ---------------------------------------------------------------------------

/// Writes `batches` as one parquet file each under `root/name/`. An empty
/// `batches` still produces a directory holding one row-less file, so the table
/// registers with the right schema.
fn write_table(
    root: &Path,
    name: &str,
    schema: &SchemaRef,
    batches: Vec<RecordBatch>,
) -> datafusion::common::Result<()> {
    let table_dir: PathBuf = root.join(name);
    std::fs::create_dir_all(&table_dir).expect("create fixture table dir");

    if batches.is_empty() {
        write_one(&table_dir.join("part-0.parquet"), schema, &[]);
        return Ok(());
    }

    for (i, batch) in batches.iter().enumerate() {
        write_one(
            &table_dir.join(format!("part-{i}.parquet")),
            schema,
            std::slice::from_ref(batch),
        );
    }
    Ok(())
}

fn write_one(path: &Path, schema: &SchemaRef, batches: &[RecordBatch]) {
    let file = File::create(path).expect("create fixture parquet file");
    let mut writer =
        ArrowWriter::try_new(file, schema.clone(), None).expect("create arrow writer");
    for batch in batches {
        writer.write(batch).expect("write fixture batch");
    }
    writer.close().expect("close fixture parquet file");
}
