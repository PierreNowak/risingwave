// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(rustdoc::private_intra_doc_links)]
//! Defines all kinds of node in the plan tree, each node represent a relational expression.
//!
//! We use a immutable style tree structure, every Node are immutable and cannot be modified after
//! it has been created. If you want to modify the node, such as rewriting the expression in a
//! `ProjectNode` or changing a node's input node, you need to create a new node. We use Rc as the
//! node's reference, and a node just storage its inputs' reference, so change a node just need
//! create one new node but not the entire sub-tree.
//!
//! So when you want to add a new node, make sure:
//! - each field in the node struct are private
//! - recommend to implement the construction of Node in a unified `new()` function, if have multi
//!   methods to construct, make they have a consistent behavior
//! - all field should be valued in construction, so the properties' derivation should be finished
//!   in the `new()` function.

use std::fmt::Debug;
use std::hash::Hash;
use std::ops::Deref;
use std::rc::Rc;

use downcast_rs::{impl_downcast, Downcast};
use dyn_clone::{self, DynClone};
use fixedbitset::FixedBitSet;
use itertools::Itertools;
use paste::paste;
use pretty_xmlish::{Pretty, PrettyConfig};
use risingwave_common::catalog::Schema;
use risingwave_common::error::{ErrorCode, Result};
use risingwave_pb::batch_plan::PlanNode as BatchPlanPb;
use risingwave_pb::stream_plan::StreamNode as StreamPlanPb;
use serde::Serialize;
use smallvec::SmallVec;

use self::batch::BatchPlanRef;
use self::generic::GenericPlanRef;
use self::stream::StreamPlanRef;
use self::utils::Distill;
use super::property::{Distribution, FunctionalDependencySet, Order};

pub trait PlanNodeMeta {
    fn node_type(&self) -> PlanNodeType;
    fn plan_base(&self) -> &PlanBase;
    fn convention(&self) -> Convention;
}

/// The common trait over all plan nodes. Used by optimizer framework which will treat all node as
/// `dyn PlanNode`
///
/// We split the trait into lots of sub-trait so that we can easily use macro to impl them.
pub trait PlanNode:
    PlanTreeNode
    + DynClone
    + DynEq
    + DynHash
    + Distill
    + Debug
    + Downcast
    + ColPrunable
    + ExprRewritable
    + ToBatch
    + ToStream
    + ToDistributedBatch
    + ToPb
    + ToLocalBatch
    + PredicatePushdown
    + PlanNodeMeta
{
}

impl Hash for dyn PlanNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dyn_hash(state);
    }
}

impl PartialEq for dyn PlanNode {
    fn eq(&self, other: &Self) -> bool {
        self.dyn_eq(other.as_dyn_eq())
    }
}

impl Eq for dyn PlanNode {}

impl_downcast!(PlanNode);

// Using a new type wrapper allows direct function implementation on `PlanRef`,
// and we currently need a manual implementation of `PartialEq` for `PlanRef`.
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Clone, Debug, Eq, Hash)]
pub struct PlanRef(Rc<dyn PlanNode>);

// Cannot use the derived implementation for now.
// See https://github.com/rust-lang/rust/issues/31740
#[allow(clippy::op_ref)]
impl PartialEq for PlanRef {
    fn eq(&self, other: &Self) -> bool {
        &self.0 == &other.0
    }
}

impl Deref for PlanRef {
    type Target = dyn PlanNode;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<T: PlanNode> From<T> for PlanRef {
    fn from(value: T) -> Self {
        PlanRef(Rc::new(value))
    }
}

impl Layer for PlanRef {
    type Sub = Self;

    fn map<F>(self, f: F) -> Self
    where
        F: FnMut(Self::Sub) -> Self::Sub,
    {
        self.clone_with_inputs(&self.inputs().into_iter().map(f).collect_vec())
    }

    fn descent<F>(&self, f: F)
    where
        F: FnMut(&Self::Sub),
    {
        self.inputs().iter().for_each(f);
    }
}

#[derive(Clone, Debug, Copy, Serialize, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub struct PlanNodeId(pub i32);

/// A more sophisticated `Endo` taking into account of the DAG structure of `PlanRef`.
/// In addition to `Endo`, one have to specify the `cached` function
/// to persist transformed `LogicalShare` and their results,
/// and the `dag_apply` function will take care to only transform every `LogicalShare` nodes once.
///
/// Note: Due to the way super trait is designed in rust,
/// one need to have separate implementation blocks of `Endo<PlanRef>` and `EndoPlan`.
/// And conventionally the real transformation `apply` is under `Endo<PlanRef>`,
/// although one can refer to `dag_apply` in the implementation of `apply`.
pub trait EndoPlan: Endo<PlanRef> {
    // Return the cached result of `plan` if present,
    // otherwise store and return the value provided by `f`.
    // Notice that to allow mutable access of `self` in `f`,
    // we let `f` to take `&mut Self` as its first argument.
    fn cached<F>(&mut self, plan: PlanRef, f: F) -> PlanRef
    where
        F: FnMut(&mut Self) -> PlanRef;

    fn dag_apply(&mut self, plan: PlanRef) -> PlanRef {
        match plan.as_logical_share() {
            Some(_) => self.cached(plan.clone(), |this| this.tree_apply(plan.clone())),
            None => self.tree_apply(plan),
        }
    }
}

/// A more sophisticated `Visit` taking into account of the DAG structure of `PlanRef`.
/// In addition to `Visit`, one have to specify `visited`
/// to store and report visited `LogicalShare` nodes,
/// and the `dag_visit` function will take care to only visit every `LogicalShare` nodes once.
/// See also `EndoPlan`.
pub trait VisitPlan: Visit<PlanRef> {
    // Skip visiting `plan` if visited, otherwise run the traversal provided by `f`.
    // Notice that to allow mutable access of `self` in `f`,
    // we let `f` to take `&mut Self` as its first argument.
    fn visited<F>(&mut self, plan: &PlanRef, f: F)
    where
        F: FnMut(&mut Self);

    fn dag_visit(&mut self, plan: &PlanRef) {
        match plan.as_logical_share() {
            Some(_) => self.visited(plan, |this| this.tree_visit(plan)),
            None => self.tree_visit(plan),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Convention {
    Logical,
    Batch,
    Stream,
}

pub(crate) trait RewriteExprsRecursive {
    fn rewrite_exprs_recursive(&self, r: &mut impl ExprRewriter) -> PlanRef;
}

impl RewriteExprsRecursive for PlanRef {
    fn rewrite_exprs_recursive(&self, r: &mut impl ExprRewriter) -> PlanRef {
        let new = self.rewrite_exprs(r);
        let inputs: Vec<PlanRef> = new
            .inputs()
            .iter()
            .map(|plan_ref| plan_ref.rewrite_exprs_recursive(r))
            .collect();
        new.clone_with_inputs(&inputs[..])
    }
}

impl PlanRef {
    fn prune_col_inner(&self, required_cols: &[usize], ctx: &mut ColumnPruningContext) -> PlanRef {
        if let Some(logical_share) = self.as_logical_share() {
            // Check the share cache first. If cache exists, it means this is the second round of
            // column pruning.
            if let Some((new_share, merge_required_cols)) = ctx.get_share_cache(self.id()) {
                // Piggyback share remove if its has only one parent.
                if ctx.get_parent_num(logical_share) == 1 {
                    let input: PlanRef = logical_share.input();
                    return input.prune_col(required_cols, ctx);
                }

                // If it is the first visit, recursively call `prune_col` for its input and
                // replace it.
                if ctx.visit_share_at_second_round(self.id()) {
                    let new_logical_share: &LogicalShare = new_share
                        .as_logical_share()
                        .expect("must be share operator");
                    let new_share_input = new_logical_share.input().prune_col(
                        &(0..new_logical_share.base.schema().len()).collect_vec(),
                        ctx,
                    );
                    new_logical_share.replace_input(new_share_input);
                }

                // Calculate the new required columns based on the new share.
                let new_required_cols: Vec<usize> = required_cols
                    .iter()
                    .map(|col| merge_required_cols.iter().position(|x| x == col).unwrap())
                    .collect_vec();
                let mapping = ColIndexMapping::with_remaining_columns(
                    &new_required_cols,
                    new_share.schema().len(),
                );
                return LogicalProject::with_mapping(new_share, mapping).into();
            }

            // `LogicalShare` can't clone, so we implement column pruning for `LogicalShare`
            // here.
            // Basically, we need to wait for all parents of `LogicalShare` to prune columns before
            // we merge the required columns and prune.
            let parent_has_pushed = ctx.add_required_cols(self.id(), required_cols.into());
            if parent_has_pushed == ctx.get_parent_num(logical_share) {
                let merge_require_cols = ctx
                    .take_required_cols(self.id())
                    .expect("must have required columns")
                    .into_iter()
                    .flat_map(|x| x.into_iter())
                    .sorted()
                    .dedup()
                    .collect_vec();
                let input: PlanRef = logical_share.input();
                let input = input.prune_col(&merge_require_cols, ctx);

                // Cache the new share operator for the second round.
                let new_logical_share = LogicalShare::create(input.clone());
                ctx.add_share_cache(self.id(), new_logical_share, merge_require_cols.clone());

                let exprs = logical_share
                    .base
                    .schema()
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, field)| {
                        if let Some(pos) = merge_require_cols.iter().position(|x| *x == i) {
                            ExprImpl::InputRef(Box::new(InputRef::new(
                                pos,
                                field.data_type.clone(),
                            )))
                        } else {
                            ExprImpl::Literal(Box::new(Literal::new(None, field.data_type.clone())))
                        }
                    })
                    .collect_vec();
                let project = LogicalProject::create(input, exprs);
                logical_share.replace_input(project);
            }
            let mapping =
                ColIndexMapping::with_remaining_columns(required_cols, self.schema().len());
            LogicalProject::with_mapping(self.clone(), mapping).into()
        } else {
            // Dispatch to dyn PlanNode instead of PlanRef.
            let dyn_t = self.deref();
            dyn_t.prune_col(required_cols, ctx)
        }
    }

    fn predicate_pushdown_inner(
        &self,
        predicate: Condition,
        ctx: &mut PredicatePushdownContext,
    ) -> PlanRef {
        if let Some(logical_share) = self.as_logical_share() {
            // Piggyback share remove if its has only one parent.
            if ctx.get_parent_num(logical_share) == 1 {
                let input: PlanRef = logical_share.input();
                return input.predicate_pushdown(predicate, ctx);
            }

            // `LogicalShare` can't clone, so we implement predicate pushdown for `LogicalShare`
            // here.
            // Basically, we need to wait for all parents of `LogicalShare` to push down the
            // predicate before we merge the predicates and pushdown.
            let parent_has_pushed = ctx.add_predicate(self.id(), predicate.clone());
            if parent_has_pushed == ctx.get_parent_num(logical_share) {
                let merge_predicate = ctx
                    .take_predicate(self.id())
                    .expect("must have predicate")
                    .into_iter()
                    .map(|mut c| Condition {
                        conjunctions: c
                            .conjunctions
                            .drain_filter(|e| e.count_nows() == 0 && e.is_pure())
                            .collect(),
                    })
                    .reduce(|a, b| a.or(b))
                    .unwrap();
                let input: PlanRef = logical_share.input();
                let input = input.predicate_pushdown(merge_predicate, ctx);
                logical_share.replace_input(input);
            }
            LogicalFilter::create(self.clone(), predicate)
        } else {
            // Dispatch to dyn PlanNode instead of PlanRef.
            let dyn_t = self.deref();
            dyn_t.predicate_pushdown(predicate, ctx)
        }
    }
}

impl ColPrunable for PlanRef {
    #[allow(clippy::let_and_return)]
    fn prune_col(&self, required_cols: &[usize], ctx: &mut ColumnPruningContext) -> PlanRef {
        let res = self.prune_col_inner(required_cols, ctx);
        #[cfg(debug_assertions)]
        super::heuristic_optimizer::HeuristicOptimizer::check_equivalent_plan(
            "column pruning",
            &LogicalProject::with_out_col_idx(self.clone(), required_cols.iter().cloned()).into(),
            &res,
        );
        res
    }
}

impl PredicatePushdown for PlanRef {
    #[allow(clippy::let_and_return)]
    fn predicate_pushdown(
        &self,
        predicate: Condition,
        ctx: &mut PredicatePushdownContext,
    ) -> PlanRef {
        #[cfg(debug_assertions)]
        let predicate_clone = predicate.clone();

        let res = self.predicate_pushdown_inner(predicate, ctx);

        #[cfg(debug_assertions)]
        super::heuristic_optimizer::HeuristicOptimizer::check_equivalent_plan(
            "predicate push down",
            &LogicalFilter::new(self.clone(), predicate_clone).into(),
            &res,
        );
        res
    }
}

impl PlanTreeNode for PlanRef {
    fn inputs(&self) -> SmallVec<[PlanRef; 2]> {
        // Dispatch to dyn PlanNode instead of PlanRef.
        let dyn_t = self.deref();
        dyn_t.inputs()
    }

    fn clone_with_inputs(&self, inputs: &[PlanRef]) -> PlanRef {
        if let Some(logical_share) = self.clone().as_logical_share() {
            assert_eq!(inputs.len(), 1);
            // We can't clone `LogicalShare`, but only can replace input instead.
            logical_share.replace_input(inputs[0].clone());
            self.clone()
        } else if let Some(stream_share) = self.clone().as_stream_share() {
            assert_eq!(inputs.len(), 1);
            // We can't clone `StreamShare`, but only can replace input instead.
            stream_share.replace_input(inputs[0].clone());
            self.clone()
        } else {
            // Dispatch to dyn PlanNode instead of PlanRef.
            let dyn_t = self.deref();
            dyn_t.clone_with_inputs(inputs)
        }
    }
}

impl StreamPlanRef for PlanRef {
    fn distribution(&self) -> &Distribution {
        &self.plan_base().dist
    }

    fn append_only(&self) -> bool {
        self.plan_base().append_only
    }

    fn emit_on_window_close(&self) -> bool {
        self.plan_base().emit_on_window_close
    }
}

impl BatchPlanRef for PlanRef {
    fn order(&self) -> &Order {
        &self.plan_base().order
    }
}

impl GenericPlanRef for PlanRef {
    fn schema(&self) -> &Schema {
        &self.plan_base().schema
    }

    fn logical_pk(&self) -> &[usize] {
        &self.plan_base().logical_pk
    }

    fn ctx(&self) -> OptimizerContextRef {
        self.plan_base().ctx()
    }

    fn functional_dependency(&self) -> &FunctionalDependencySet {
        self.plan_base().functional_dependency()
    }
}

/// In order to let expression display id started from 1 for explaining, hidden column names and
/// other places. We will reset expression display id to 0 and clone the whole plan to reset the
/// schema.
pub fn reorganize_elements_id(plan: PlanRef) -> PlanRef {
    let old_expr_display_id = plan.ctx().get_expr_display_id();
    let old_plan_node_id = plan.ctx().get_plan_node_id();
    plan.ctx().set_expr_display_id(0);
    plan.ctx().set_plan_node_id(0);
    let plan = PlanCloner::clone_whole_plan(plan);
    plan.ctx().set_expr_display_id(old_expr_display_id);
    plan.ctx().set_plan_node_id(old_plan_node_id);
    plan
}

pub trait Explain {
    /// Write explain the whole plan tree.
    fn explain<'a>(&self) -> Pretty<'a>;

    /// Explain the plan node and return a string.
    fn explain_to_string(&self) -> String;
}

impl Explain for PlanRef {
    /// Write explain the whole plan tree.
    fn explain<'a>(&self) -> Pretty<'a> {
        let mut node = self.distill();
        let inputs = self.inputs();
        for input in inputs.iter().peekable() {
            node.children.push(input.explain());
        }
        Pretty::Record(node)
    }

    /// Explain the plan node and return a string.
    fn explain_to_string(&self) -> String {
        let plan = reorganize_elements_id(self.clone());

        let mut output = String::with_capacity(2048);
        let mut config = pretty_config();
        config.unicode(&mut output, &plan.explain());
        output
    }
}

pub(crate) fn pretty_config() -> PrettyConfig {
    PrettyConfig {
        indent: 3,
        need_boundaries: false,
        width: 2048,
        reduced_spaces: true,
    }
}

impl dyn PlanNode {
    pub fn id(&self) -> PlanNodeId {
        self.plan_base().id
    }

    pub fn ctx(&self) -> OptimizerContextRef {
        self.plan_base().ctx.clone()
    }

    pub fn schema(&self) -> &Schema {
        &self.plan_base().schema
    }

    pub fn logical_pk(&self) -> &[usize] {
        &self.plan_base().logical_pk
    }

    pub fn order(&self) -> &Order {
        &self.plan_base().order
    }

    pub fn distribution(&self) -> &Distribution {
        &self.plan_base().dist
    }

    pub fn append_only(&self) -> bool {
        self.plan_base().append_only
    }

    pub fn emit_on_window_close(&self) -> bool {
        self.plan_base().emit_on_window_close
    }

    pub fn functional_dependency(&self) -> &FunctionalDependencySet {
        &self.plan_base().functional_dependency
    }

    pub fn watermark_columns(&self) -> &FixedBitSet {
        &self.plan_base().watermark_columns
    }

    /// Serialize the plan node and its children to a stream plan proto.
    ///
    /// Note that [`StreamTableScan`] has its own implementation of `to_stream_prost`. We have a
    /// hook inside to do some ad-hoc thing for [`StreamTableScan`].
    pub fn to_stream_prost(&self, state: &mut BuildFragmentGraphState) -> StreamPlanPb {
        if let Some(stream_table_scan) = self.as_stream_table_scan() {
            return stream_table_scan.adhoc_to_stream_prost(state);
        }
        if let Some(stream_share) = self.as_stream_share() {
            return stream_share.adhoc_to_stream_prost(state);
        }

        let node = Some(self.to_stream_prost_body(state));
        let input = self
            .inputs()
            .into_iter()
            .map(|plan| plan.to_stream_prost(state))
            .collect();
        // TODO: support pk_indices and operator_id
        StreamPlanPb {
            input,
            identity: self.explain_myself_to_string(),
            node_body: node,
            operator_id: self.id().0 as _,
            stream_key: self.logical_pk().iter().map(|x| *x as u32).collect(),
            fields: self.schema().to_prost(),
            append_only: self.append_only(),
        }
    }

    /// Serialize the plan node and its children to a batch plan proto.
    pub fn to_batch_prost(&self) -> BatchPlanPb {
        self.to_batch_prost_identity(true)
    }

    /// Serialize the plan node and its children to a batch plan proto without the identity field
    /// (for testing).
    pub fn to_batch_prost_identity(&self, identity: bool) -> BatchPlanPb {
        let node_body = Some(self.to_batch_prost_body());
        let children = self
            .inputs()
            .into_iter()
            .map(|plan| plan.to_batch_prost_identity(identity))
            .collect();
        BatchPlanPb {
            children,
            identity: if identity {
                self.explain_myself_to_string()
            } else {
                "".into()
            },
            node_body,
        }
    }

    pub fn explain_myself_to_string(&self) -> String {
        self.distill_to_string()
    }
}

mod plan_base;
#[macro_use]
mod plan_tree_node_v2;
pub use plan_base::*;
#[macro_use]
mod plan_tree_node;
pub use plan_tree_node::*;
mod col_pruning;
pub use col_pruning::*;
mod expr_rewritable;
pub use expr_rewritable::*;
mod convert;
pub use convert::*;
mod eq_join_predicate;
pub use eq_join_predicate::*;
mod to_prost;
pub use to_prost::*;
mod predicate_pushdown;
pub use predicate_pushdown::*;
mod merge_eq_nodes;
pub use merge_eq_nodes::*;

pub mod batch;
pub mod generic;
pub mod stream;
pub mod stream_derive;

pub use generic::{PlanAggCall, PlanAggCallDisplay};

mod batch_delete;
mod batch_exchange;
mod batch_expand;
mod batch_filter;
mod batch_group_topn;
mod batch_hash_agg;
mod batch_hash_join;
mod batch_hop_window;
mod batch_insert;
mod batch_limit;
mod batch_lookup_join;
mod batch_nested_loop_join;
mod batch_over_window;
mod batch_project;
mod batch_project_set;
mod batch_seq_scan;
mod batch_simple_agg;
mod batch_sort;
mod batch_sort_agg;
mod batch_source;
mod batch_table_function;
mod batch_topn;
mod batch_union;
mod batch_update;
mod batch_values;
mod logical_agg;
mod logical_apply;
mod logical_dedup;
mod logical_delete;
mod logical_except;
mod logical_expand;
mod logical_filter;
mod logical_hop_window;
mod logical_insert;
mod logical_intersect;
mod logical_join;
mod logical_limit;
mod logical_multi_join;
mod logical_now;
mod logical_over_window;
mod logical_project;
mod logical_project_set;
mod logical_scan;
mod logical_share;
mod logical_source;
mod logical_table_function;
mod logical_topn;
mod logical_union;
mod logical_update;
mod logical_values;
mod stream_dedup;
mod stream_delta_join;
mod stream_dml;
mod stream_dynamic_filter;
mod stream_eowc_over_window;
mod stream_exchange;
mod stream_expand;
mod stream_filter;
mod stream_group_topn;
mod stream_hash_agg;
mod stream_hash_join;
mod stream_hop_window;
mod stream_materialize;
mod stream_now;
mod stream_over_window;
mod stream_project;
mod stream_project_set;
mod stream_row_id_gen;
mod stream_simple_agg;
mod stream_sink;
mod stream_sort;
mod stream_source;
mod stream_stateless_simple_agg;
mod stream_table_scan;
mod stream_topn;
mod stream_values;
mod stream_watermark_filter;

mod derive;
mod stream_share;
mod stream_temporal_join;
mod stream_union;
pub mod utils;

pub use batch_delete::BatchDelete;
pub use batch_exchange::BatchExchange;
pub use batch_expand::BatchExpand;
pub use batch_filter::BatchFilter;
pub use batch_group_topn::BatchGroupTopN;
pub use batch_hash_agg::BatchHashAgg;
pub use batch_hash_join::BatchHashJoin;
pub use batch_hop_window::BatchHopWindow;
pub use batch_insert::BatchInsert;
pub use batch_limit::BatchLimit;
pub use batch_lookup_join::BatchLookupJoin;
pub use batch_nested_loop_join::BatchNestedLoopJoin;
pub use batch_over_window::BatchOverWindow;
pub use batch_project::BatchProject;
pub use batch_project_set::BatchProjectSet;
pub use batch_seq_scan::BatchSeqScan;
pub use batch_simple_agg::BatchSimpleAgg;
pub use batch_sort::BatchSort;
pub use batch_sort_agg::BatchSortAgg;
pub use batch_source::BatchSource;
pub use batch_table_function::BatchTableFunction;
pub use batch_topn::BatchTopN;
pub use batch_union::BatchUnion;
pub use batch_update::BatchUpdate;
pub use batch_values::BatchValues;
pub use logical_agg::LogicalAgg;
pub use logical_apply::LogicalApply;
pub use logical_dedup::LogicalDedup;
pub use logical_delete::LogicalDelete;
pub use logical_except::LogicalExcept;
pub use logical_expand::LogicalExpand;
pub use logical_filter::LogicalFilter;
pub use logical_hop_window::LogicalHopWindow;
pub use logical_insert::LogicalInsert;
pub use logical_intersect::LogicalIntersect;
pub use logical_join::LogicalJoin;
pub use logical_limit::LogicalLimit;
pub use logical_multi_join::{LogicalMultiJoin, LogicalMultiJoinBuilder};
pub use logical_now::LogicalNow;
pub use logical_over_window::LogicalOverWindow;
pub use logical_project::LogicalProject;
pub use logical_project_set::LogicalProjectSet;
pub use logical_scan::LogicalScan;
pub use logical_share::LogicalShare;
pub use logical_source::LogicalSource;
pub use logical_table_function::LogicalTableFunction;
pub use logical_topn::LogicalTopN;
pub use logical_union::LogicalUnion;
pub use logical_update::LogicalUpdate;
pub use logical_values::LogicalValues;
pub use stream_dedup::StreamDedup;
pub use stream_delta_join::StreamDeltaJoin;
pub use stream_dml::StreamDml;
pub use stream_dynamic_filter::StreamDynamicFilter;
pub use stream_eowc_over_window::StreamEowcOverWindow;
pub use stream_exchange::StreamExchange;
pub use stream_expand::StreamExpand;
pub use stream_filter::StreamFilter;
pub use stream_group_topn::StreamGroupTopN;
pub use stream_hash_agg::StreamHashAgg;
pub use stream_hash_join::StreamHashJoin;
pub use stream_hop_window::StreamHopWindow;
pub use stream_materialize::StreamMaterialize;
pub use stream_now::StreamNow;
pub use stream_over_window::StreamOverWindow;
pub use stream_project::StreamProject;
pub use stream_project_set::StreamProjectSet;
pub use stream_row_id_gen::StreamRowIdGen;
pub use stream_share::StreamShare;
pub use stream_simple_agg::StreamSimpleAgg;
pub use stream_sink::StreamSink;
pub use stream_sort::StreamEowcSort;
pub use stream_source::StreamSource;
pub use stream_stateless_simple_agg::StreamStatelessSimpleAgg;
pub use stream_table_scan::StreamTableScan;
pub use stream_temporal_join::StreamTemporalJoin;
pub use stream_topn::StreamTopN;
pub use stream_union::StreamUnion;
pub use stream_values::StreamValues;
pub use stream_watermark_filter::StreamWatermarkFilter;

use crate::expr::{ExprImpl, ExprRewriter, InputRef, Literal};
use crate::optimizer::optimizer_context::OptimizerContextRef;
use crate::optimizer::plan_rewriter::PlanCloner;
use crate::stream_fragmenter::BuildFragmentGraphState;
use crate::utils::{ColIndexMapping, Condition, DynEq, DynHash, Endo, Layer, Visit};

/// `for_all_plan_nodes` includes all plan nodes. If you added a new plan node
/// inside the project, be sure to add here and in its conventions like `for_logical_plan_nodes`
///
/// Every tuple has two elements, where `{ convention, name }`
/// You can use it as follows
/// ```rust
/// macro_rules! use_plan {
///     ($({ $convention:ident, $name:ident }),*) => {};
/// }
/// risingwave_frontend::for_all_plan_nodes! { use_plan }
/// ```
/// See the following implementations for example.
#[macro_export]
macro_rules! for_all_plan_nodes {
    ($macro:ident) => {
        $macro! {
              { Logical, Agg }
            , { Logical, Apply }
            , { Logical, Filter }
            , { Logical, Project }
            , { Logical, Scan }
            , { Logical, Source }
            , { Logical, Insert }
            , { Logical, Delete }
            , { Logical, Update }
            , { Logical, Join }
            , { Logical, Values }
            , { Logical, Limit }
            , { Logical, TopN }
            , { Logical, HopWindow }
            , { Logical, TableFunction }
            , { Logical, MultiJoin }
            , { Logical, Expand }
            , { Logical, ProjectSet }
            , { Logical, Union }
            , { Logical, OverWindow }
            , { Logical, Share }
            , { Logical, Now }
            , { Logical, Dedup }
            , { Logical, Intersect }
            , { Logical, Except }
            , { Batch, SimpleAgg }
            , { Batch, HashAgg }
            , { Batch, SortAgg }
            , { Batch, Project }
            , { Batch, Filter }
            , { Batch, Insert }
            , { Batch, Delete }
            , { Batch, Update }
            , { Batch, SeqScan }
            , { Batch, HashJoin }
            , { Batch, NestedLoopJoin }
            , { Batch, Values }
            , { Batch, Sort }
            , { Batch, Exchange }
            , { Batch, Limit }
            , { Batch, TopN }
            , { Batch, HopWindow }
            , { Batch, TableFunction }
            , { Batch, Expand }
            , { Batch, LookupJoin }
            , { Batch, ProjectSet }
            , { Batch, Union }
            , { Batch, GroupTopN }
            , { Batch, Source }
            , { Batch, OverWindow }
            , { Stream, Project }
            , { Stream, Filter }
            , { Stream, TableScan }
            , { Stream, Sink }
            , { Stream, Source }
            , { Stream, HashJoin }
            , { Stream, Exchange }
            , { Stream, HashAgg }
            , { Stream, SimpleAgg }
            , { Stream, StatelessSimpleAgg }
            , { Stream, Materialize }
            , { Stream, TopN }
            , { Stream, HopWindow }
            , { Stream, DeltaJoin }
            , { Stream, Expand }
            , { Stream, DynamicFilter }
            , { Stream, ProjectSet }
            , { Stream, GroupTopN }
            , { Stream, Union }
            , { Stream, RowIdGen }
            , { Stream, Dml }
            , { Stream, Now }
            , { Stream, Share }
            , { Stream, WatermarkFilter }
            , { Stream, TemporalJoin }
            , { Stream, Values }
            , { Stream, Dedup }
            , { Stream, EowcOverWindow }
            , { Stream, EowcSort }
            , { Stream, OverWindow }
        }
    };
}

/// `for_logical_plan_nodes` includes all plan nodes with logical convention.
#[macro_export]
macro_rules! for_logical_plan_nodes {
    ($macro:ident) => {
        $macro! {
              { Logical, Agg }
            , { Logical, Apply }
            , { Logical, Filter }
            , { Logical, Project }
            , { Logical, Scan }
            , { Logical, Source }
            , { Logical, Insert }
            , { Logical, Delete }
            , { Logical, Update }
            , { Logical, Join }
            , { Logical, Values }
            , { Logical, Limit }
            , { Logical, TopN }
            , { Logical, HopWindow }
            , { Logical, TableFunction }
            , { Logical, MultiJoin }
            , { Logical, Expand }
            , { Logical, ProjectSet }
            , { Logical, Union }
            , { Logical, OverWindow }
            , { Logical, Share }
            , { Logical, Now }
            , { Logical, Dedup }
            , { Logical, Intersect }
            , { Logical, Except }
        }
    };
}

/// `for_batch_plan_nodes` includes all plan nodes with batch convention.
#[macro_export]
macro_rules! for_batch_plan_nodes {
    ($macro:ident) => {
        $macro! {
              { Batch, SimpleAgg }
            , { Batch, HashAgg }
            , { Batch, SortAgg }
            , { Batch, Project }
            , { Batch, Filter }
            , { Batch, SeqScan }
            , { Batch, HashJoin }
            , { Batch, NestedLoopJoin }
            , { Batch, Values }
            , { Batch, Limit }
            , { Batch, Sort }
            , { Batch, TopN }
            , { Batch, Exchange }
            , { Batch, Insert }
            , { Batch, Delete }
            , { Batch, Update }
            , { Batch, HopWindow }
            , { Batch, TableFunction }
            , { Batch, Expand }
            , { Batch, LookupJoin }
            , { Batch, ProjectSet }
            , { Batch, Union }
            , { Batch, GroupTopN }
            , { Batch, Source }
            , { Batch, OverWindow }
        }
    };
}

/// `for_stream_plan_nodes` includes all plan nodes with stream convention.
#[macro_export]
macro_rules! for_stream_plan_nodes {
    ($macro:ident) => {
        $macro! {
              { Stream, Project }
            , { Stream, Filter }
            , { Stream, HashJoin }
            , { Stream, Exchange }
            , { Stream, TableScan }
            , { Stream, Sink }
            , { Stream, Source }
            , { Stream, HashAgg }
            , { Stream, SimpleAgg }
            , { Stream, StatelessSimpleAgg }
            , { Stream, Materialize }
            , { Stream, TopN }
            , { Stream, HopWindow }
            , { Stream, DeltaJoin }
            , { Stream, Expand }
            , { Stream, DynamicFilter }
            , { Stream, ProjectSet }
            , { Stream, GroupTopN }
            , { Stream, Union }
            , { Stream, RowIdGen }
            , { Stream, Dml }
            , { Stream, Now }
            , { Stream, Share }
            , { Stream, WatermarkFilter }
            , { Stream, TemporalJoin }
            , { Stream, Values }
            , { Stream, Dedup }
            , { Stream, EowcOverWindow }
            , { Stream, EowcSort }
            , { Stream, OverWindow }
        }
    };
}

/// impl [`PlanNodeType`] fn for each node.
macro_rules! impl_plan_node_meta {
    ($( { $convention:ident, $name:ident }),*) => {
        paste!{
            /// each enum value represent a PlanNode struct type, help us to dispatch and downcast
            #[derive(Copy, Clone, PartialEq, Debug, Hash, Eq, Serialize)]
            pub enum PlanNodeType {
                $( [<$convention $name>] ),*
            }

            $(impl PlanNodeMeta for [<$convention $name>] {
                fn node_type(&self) -> PlanNodeType{
                    PlanNodeType::[<$convention $name>]
                }
                fn plan_base(&self) -> &PlanBase {
                    &self.base
                }
                fn convention(&self) -> Convention {
                    Convention::$convention
                }
            })*
        }
    }
}

for_all_plan_nodes! { impl_plan_node_meta }

macro_rules! impl_plan_node {
    ($({ $convention:ident, $name:ident }),*) => {
        paste!{
            $(impl PlanNode for [<$convention $name>] { })*
        }
    }
}

for_all_plan_nodes! { impl_plan_node }

/// impl plan node downcast fn for each node.
macro_rules! impl_down_cast_fn {
    ($( { $convention:ident, $name:ident }),*) => {
        paste!{
            impl dyn PlanNode {
                $( pub fn [< as_$convention:snake _ $name:snake>](&self) -> Option<&[<$convention $name>]> {
                    self.downcast_ref::<[<$convention $name>]>()
                } )*
            }
        }
    }
}

for_all_plan_nodes! { impl_down_cast_fn }
