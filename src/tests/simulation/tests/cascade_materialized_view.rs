// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(madsim)]

use std::time::Duration;

use anyhow::Result;
use itertools::Itertools;
use madsim::time::sleep;
use risingwave_simulation::cluster::{Cluster, Configuration};
use risingwave_simulation::ctl_ext::predicate::{identity_contains, no_identity_contains};
use risingwave_simulation::utils::AssertResult;

const ROOT_TABLE_CREATE: &str = "create table t1 (v1 int);";
const MV1: &str = "create materialized view m1 as select * from t1 where v1 > 5;";
const MV2: &str = "create materialized view m2 as select * from t1 where v1 > 10;";
const MV3: &str = "create materialized view m3 as select * from m2 where v1 < 15;";
const MV4: &str = "create materialized view m4 as select m1.v1 as m1v, m3.v1 as m3v from m1 join m3 on m1.v1 = m3.v1;";
const MV5: &str = "create materialized view m5 as select * from m4;";

#[madsim::test]
async fn test_simple_cascade_materialized_view() -> Result<()> {
    let mut cluster = Cluster::start(Configuration::for_scale()).await?;

    cluster.run(ROOT_TABLE_CREATE).await?;
    cluster.run(MV1).await?;

    let fragment = cluster
        .locate_one_fragment(vec![
            identity_contains("materialize"),
            no_identity_contains("chain"),
            no_identity_contains("hashjoin"),
        ])
        .await?;

    let id = fragment.id();

    cluster.reschedule(format!("{id}-[1,2,3,4,5]")).await?;
    sleep(Duration::from_secs(3)).await;

    let fragment = cluster.locate_fragment_by_id(id).await?;
    assert_eq!(fragment.inner.actors.len(), 1);

    let chain_fragment = cluster
        .locate_one_fragment(vec![identity_contains("chain")])
        .await?;

    assert_eq!(
        chain_fragment.inner.actors.len(),
        fragment.inner.actors.len()
    );

    cluster
        .run(&format!(
            "insert into t1 values {}",
            (1..=10).map(|x| format!("({x})")).join(",")
        ))
        .await?;

    cluster.run("flush").await?;

    // v1 > 5, result is [6, 7, 8, 9, 10]
    cluster
        .run("select count(*) from m1")
        .await?
        .assert_result_eq("5");

    cluster.reschedule(format!("{id}+[1,2,3,4,5]")).await?;
    sleep(Duration::from_secs(3)).await;

    let fragment = cluster.locate_fragment_by_id(id).await?;
    assert_eq!(fragment.inner.actors.len(), 6);

    let chain_fragment = cluster
        .locate_one_fragment(vec![identity_contains("chain")])
        .await?;

    assert_eq!(
        chain_fragment.inner.actors.len(),
        fragment.inner.actors.len()
    );

    cluster
        .run("select count(*) from m1")
        .await?
        .assert_result_eq("5");

    cluster
        .run(&format!(
            "insert into t1 values {}",
            (11..=20).map(|x| format!("({x})")).join(",")
        ))
        .await?;

    cluster.run("flush").await?;
    // 10 < v1 < 15, result is [11, 12, 13, 14]
    cluster
        .run("select count(*) from m1")
        .await?
        .assert_result_eq("15");

    Ok(())
}

#[madsim::test]
async fn test_diamond_cascade_materialized_view() -> Result<()> {
    let mut cluster = Cluster::start(Configuration::for_scale()).await?;

    cluster.run(ROOT_TABLE_CREATE).await?;
    cluster.run(MV1).await?;
    cluster.run(MV2).await?;
    cluster.run(MV3).await?;
    cluster.run(MV4).await?;
    cluster.run(MV5).await?;

    let fragment = cluster
        .locate_one_fragment(vec![
            identity_contains("materialize"),
            no_identity_contains("chain"),
            no_identity_contains("hashjoin"),
        ])
        .await?;

    let id = fragment.id();

    cluster.reschedule(format!("{id}-[1,2,3,4,5]")).await?;
    sleep(Duration::from_secs(3)).await;

    let fragment = cluster.locate_fragment_by_id(id).await?;
    assert_eq!(fragment.inner.actors.len(), 1);

    cluster
        .run(&format!(
            "insert into t1 values {}",
            (1..=10).map(|x| format!("({x})")).join(",")
        ))
        .await?;

    cluster.run("flush").await?;
    cluster
        .run("select count(*) from m5")
        .await?
        .assert_result_eq("0");

    cluster.reschedule(format!("{id}+[1,2,3,4,5]")).await?;
    sleep(Duration::from_secs(3)).await;

    let fragment = cluster.locate_fragment_by_id(id).await?;
    assert_eq!(fragment.inner.actors.len(), 6);

    cluster
        .run("select count(*) from m5")
        .await?
        .assert_result_eq("0");

    cluster
        .run(&format!(
            "insert into t1 values {}",
            (11..=20).map(|x| format!("({x})")).join(",")
        ))
        .await?;

    cluster.run("flush").await?;
    cluster
        .run("select count(*) from m5")
        .await?
        .assert_result_eq("4");

    Ok(())
}
