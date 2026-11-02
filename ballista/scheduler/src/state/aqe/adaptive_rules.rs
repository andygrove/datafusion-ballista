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

//! Rules a scheduler embedder adds to adaptive query planning.

use datafusion::physical_optimizer::PhysicalOptimizerRule;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

/// A physical optimizer rule, as adaptive planning runs it.
pub type AdaptiveRule = Arc<dyn PhysicalOptimizerRule + Send + Sync>;

/// Physical optimizer rules that a scheduler embedder adds to the adaptive
/// planning of a submitted physical plan, for plans with execution plan nodes
/// of its own that Ballista's rules don't know.
///
/// Both kinds see the plan with its stage boundaries as
/// [`ExchangeExec`](super::execution_plan::ExchangeExec) nodes. A rule that
/// places an exchange of its own takes its plan id from `plan_ids`, the
/// generator the job's other rules share.
pub trait AdaptiveRules: Debug + Send + Sync {
    /// Rules that run once on the submitted plan, after its hash
    /// repartitions are pinned as exchanges and before any stage runs. A rule
    /// can hold a stage back here by marking its exchange inactive.
    fn preparation_rules(&self, _plan_ids: Arc<AtomicUsize>) -> Vec<AdaptiveRule> {
        vec![]
    }

    /// Rules that run each time the plan is re-optimized after a stage
    /// completes, when the finished stage's exchange reports its measured
    /// statistics. They run after Ballista's own join selection and before
    /// DataFusion's rules.
    fn replan_rules(&self, _plan_ids: Arc<AtomicUsize>) -> Vec<AdaptiveRule> {
        vec![]
    }
}
