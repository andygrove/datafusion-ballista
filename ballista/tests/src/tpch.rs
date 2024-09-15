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

use datafusion::common::Result;
use datafusion::physical_plan::displayable;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use ballista_scheduler::planner::DistributedPlanner;

#[tokio::test]
async fn test_tpch_plans() -> Result<()> {
    for i in 1..=22 {
        let name = format!("q{i}");
        let _ = test_expected_tpch_plan(name.as_str()).await?;
    }
    Ok(())
}


async fn test_expected_tpch_plan(name: &str) -> Result<()> {
    let ctx = SessionContext::new();
    for table in [
        "customer", "nation", "part", "region", "lineitem", "orders", "partsupp", "supplier",
    ] {
        let path = format!("/mnt/bigdata/tpch/sf100/{}.parquet", table);
        ctx.register_parquet(table, &path, ParquetReadOptions::default())
            .await?;
    }

    let query_file = format!("tpch/queries/{}.sql", name);
    println!("Reading: {}", query_file);
    let path = Path::new(&query_file);
    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut sqls = Vec::new();
    let mut sql = String::new();
    for line in reader.lines() {
        let line = line?;
        if line.starts_with("--") {
            continue;
        }
        sql.push_str(line.as_str());
        sql.push('\n');

        if line.trim().ends_with(";") {
            sqls.push(sql.clone());
            sql.clear();
        }
    }

    let expected_plan_path = format!("tpch/expected-plans/{}.txt", name);
    println!("Writing: {}", expected_plan_path);
    let file = File::create(expected_plan_path)?;
    let mut w = BufWriter::new(file);

    for sql in &sqls {
        write!(w, "{}\n\n", sql)?;
        let df = ctx.sql(&sql).await?;
        let physical_plan = ctx.state().create_physical_plan(df.logical_plan()).await?;
        let mut planner = DistributedPlanner::new();
        let query_stages = planner.plan_query_stages("test", physical_plan).unwrap();
        query_stages.iter().enumerate().for_each(|(idx, stage)| {
            let displayable = displayable(stage.as_ref()).indent(true);
            write!(w, "Query Stage {}:\n{}\n", idx, displayable).unwrap();
        });
    }

    Ok(())
}
