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

use pretty_xmlish::{Pretty, XmlNode};
pub use risingwave_pb::expr::expr_node::Type as ExprType;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::DynamicFilterNode;

use super::generic::DynamicFilter;
use super::utils::{childless_record, column_names_pretty, watermark_pretty, Distill};
use super::{generic, ExprRewritable};
use crate::expr::{Expr, ExprImpl};
use crate::optimizer::plan_node::{PlanBase, PlanTreeNodeBinary, StreamNode};
use crate::optimizer::PlanRef;
use crate::stream_fragmenter::BuildFragmentGraphState;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamDynamicFilter {
    pub base: PlanBase,
    core: generic::DynamicFilter<PlanRef>,
    cleaned_by_watermark: bool,
}

impl StreamDynamicFilter {
    pub fn new(core: DynamicFilter<PlanRef>) -> Self {
        let watermark_columns = core.watermark_columns(core.right().watermark_columns()[0]);

        // TODO: derive from input
        let base = PlanBase::new_stream_with_logical(
            &core,
            core.left().distribution().clone(),
            false, /* we can have a new abstraction for append only and monotonically increasing
                    * in the future */
            false, // TODO(rc): decide EOWC property
            watermark_columns,
        );
        let cleaned_by_watermark = Self::cleaned_by_watermark(&core);
        Self {
            base,
            core,
            cleaned_by_watermark,
        }
    }

    pub fn left_index(&self) -> usize {
        self.core.left_index()
    }

    /// 1. Check the comparator.
    /// 2. RHS input should only have 1 columns, which is the watermark column.
    ///    We check that the watermark should be set.
    pub fn cleaned_by_watermark(core: &DynamicFilter<PlanRef>) -> bool {
        let expr = core.predicate();
        if let Some(ExprImpl::FunctionCall(function_call)) = expr.as_expr_unless_true() {
            match function_call.func_type() {
                ExprType::GreaterThan | ExprType::GreaterThanOrEqual => {
                    let rhs_input = core.right();
                    rhs_input.watermark_columns().contains(0)
                }
                _ => false,
            }
        } else {
            false
        }
    }
}

impl Distill for StreamDynamicFilter {
    fn distill<'a>(&self) -> XmlNode<'a> {
        let verbose = self.base.ctx.is_explain_verbose();
        let pred = self.core.pretty_field();
        let mut vec = Vec::with_capacity(if verbose { 3 } else { 2 });
        vec.push(("predicate", pred));
        if let Some(ow) = watermark_pretty(&self.base.watermark_columns, self.schema()) {
            vec.push(("output_watermarks", ow));
        }
        vec.push(("output", column_names_pretty(self.schema())));
        if self.cleaned_by_watermark {
            vec.push((
                "cleaned_by_watermark",
                Pretty::display(&self.cleaned_by_watermark),
            ));
        }
        childless_record("StreamDynamicFilter", vec)
    }
}

impl PlanTreeNodeBinary for StreamDynamicFilter {
    fn left(&self) -> PlanRef {
        self.core.left().clone()
    }

    fn right(&self) -> PlanRef {
        self.core.right().clone()
    }

    fn clone_with_left_right(&self, left: PlanRef, right: PlanRef) -> Self {
        Self::new(self.core.clone_with_left_right(left, right))
    }
}

impl_plan_tree_node_for_binary! { StreamDynamicFilter }

impl StreamNode for StreamDynamicFilter {
    fn to_stream_prost_body(&self, state: &mut BuildFragmentGraphState) -> NodeBody {
        use generic::dynamic_filter::*;
        let cleaned_by_watermark = self.cleaned_by_watermark;
        let condition = self
            .core
            .predicate()
            .as_expr_unless_true()
            .map(|x| x.to_expr_proto());
        let left_index = self.core.left_index();
        let left_table = infer_left_internal_table_catalog(&self.base, left_index)
            .with_id(state.gen_table_id_wrapped())
            .with_cleaned_by_watermark(cleaned_by_watermark);
        let right = self.right();
        let right_table = infer_right_internal_table_catalog(right.plan_base())
            .with_id(state.gen_table_id_wrapped());
        NodeBody::DynamicFilter(DynamicFilterNode {
            left_key: left_index as u32,
            condition,
            left_table: Some(left_table.to_internal_table_prost()),
            right_table: Some(right_table.to_internal_table_prost()),
        })
    }
}

impl ExprRewritable for StreamDynamicFilter {}
