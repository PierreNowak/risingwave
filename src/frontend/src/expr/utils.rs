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

use fixedbitset::FixedBitSet;
use risingwave_common::types::{DataType, ScalarImpl};
use risingwave_pb::expr::expr_node::Type;

use super::{Expr, ExprImpl, ExprRewriter, ExprVisitor, FunctionCall, InputRef};
use crate::expr::ExprType;

fn split_expr_by(expr: ExprImpl, op: ExprType, rets: &mut Vec<ExprImpl>) {
    match expr {
        ExprImpl::FunctionCall(func_call) if func_call.func_type() == op => {
            let (_, exprs, _) = func_call.decompose();
            for expr in exprs {
                split_expr_by(expr, op, rets);
            }
        }
        _ => rets.push(expr),
    }
}

pub fn merge_expr_by_binary<I>(mut exprs: I, op: ExprType, identity_elem: ExprImpl) -> ExprImpl
where
    I: Iterator<Item = ExprImpl>,
{
    if let Some(e) = exprs.next() {
        let mut ret = e;
        for expr in exprs {
            ret = FunctionCall::new(op, vec![ret, expr]).unwrap().into();
        }
        ret
    } else {
        identity_elem
    }
}

/// Transform a bool expression to Conjunctive form. e.g. given expression is
/// (expr1 AND expr2 AND expr3) the function will return Vec[expr1, expr2, expr3].
pub fn to_conjunctions(expr: ExprImpl) -> Vec<ExprImpl> {
    let mut rets = vec![];
    split_expr_by(expr, ExprType::And, &mut rets);
    rets
}

/// Transform a bool expression to Disjunctive form. e.g. given expression is
/// (expr1 OR expr2 OR expr3) the function will return Vec[expr1, expr2, expr3].
pub fn to_disjunctions(expr: ExprImpl) -> Vec<ExprImpl> {
    let mut rets = vec![];
    split_expr_by(expr, ExprType::Or, &mut rets);
    rets
}

/// fold boolean constants in boolean exprs
/// e.g. `(A And false) Or true` will become `True`
pub fn fold_boolean_constant(expr: ExprImpl) -> ExprImpl {
    let mut rewriter = BooleanConstantFolding {};
    rewriter.rewrite_expr(expr)
}

/// Fold boolean constants in a expr
struct BooleanConstantFolding {}

impl ExprRewriter for BooleanConstantFolding {
    /// fold boolean constants in [`FunctionCall`] and rewrite Expr
    /// # Panics
    /// This rewriter is based on the assumption that [`Type::And`] and [`Type::Or`] are binary
    /// functions, i.e. they have exactly 2 inputs.
    /// This rewriter will panic if the length of the inputs is not `2`.
    fn rewrite_function_call(&mut self, func_call: FunctionCall) -> ExprImpl {
        let (func_type, inputs, ret) = func_call.decompose();
        let inputs: Vec<_> = inputs
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        let bool_constant_values: Vec<Option<bool>> =
            inputs.iter().map(try_get_bool_constant).collect();
        let contains_bool_constant = bool_constant_values.iter().any(|x| x.is_some());
        // Take elements from inputs, and reorder them to make sure lhs is a constant value
        let prepare_binary_function_inputs = |mut inputs: Vec<ExprImpl>| -> (ExprImpl, ExprImpl) {
            // Make sure that binary functions have exactly 2 inputs
            assert_eq!(inputs.len(), 2);
            let rhs = inputs.pop().unwrap();
            let lhs = inputs.pop().unwrap();
            if bool_constant_values[0].is_some() {
                (lhs, rhs)
            } else {
                (rhs, lhs)
            }
        };
        match func_type {
            // unary functions
            Type::Not => {
                let input = inputs.first().unwrap();
                if let Some(v) = try_get_bool_constant(input) {
                    return ExprImpl::literal_bool(!v);
                }
            }
            Type::IsFalse => {
                let input = inputs.first().unwrap();
                if input.is_null() {
                    return ExprImpl::literal_bool(false);
                }
                if let Some(v) = try_get_bool_constant(input) {
                    return ExprImpl::literal_bool(!v);
                }
            }
            Type::IsTrue => {
                let input = inputs.first().unwrap();
                if input.is_null() {
                    return ExprImpl::literal_bool(false);
                }
                if let Some(v) = try_get_bool_constant(input) {
                    return ExprImpl::literal_bool(v);
                }
            }
            Type::IsNull => {
                let input = inputs.first().unwrap();
                if input.is_null() {
                    return ExprImpl::literal_bool(true);
                }
            }
            Type::IsNotTrue => {
                let input = inputs.first().unwrap();
                if input.is_null() {
                    return ExprImpl::literal_bool(true);
                }
                if let Some(v) = try_get_bool_constant(input) {
                    return ExprImpl::literal_bool(!v);
                }
            }
            Type::IsNotFalse => {
                let input = inputs.first().unwrap();
                if input.is_null() {
                    return ExprImpl::literal_bool(true);
                }
                if let Some(v) = try_get_bool_constant(input) {
                    return ExprImpl::literal_bool(v);
                }
            }
            Type::IsNotNull => {
                let input = inputs.first().unwrap();
                if let ExprImpl::Literal(lit) = input {
                    return ExprImpl::literal_bool(lit.get_data().is_some());
                }
            }
            // binary functions
            Type::And if contains_bool_constant => {
                let (constant_lhs, rhs) = prepare_binary_function_inputs(inputs);
                return boolean_constant_fold_and(constant_lhs, rhs);
            }
            Type::Or if contains_bool_constant => {
                let (constant_lhs, rhs) = prepare_binary_function_inputs(inputs);
                return boolean_constant_fold_or(constant_lhs, rhs);
            }
            _ => {}
        }
        FunctionCall::new_unchecked(func_type, inputs, ret).into()
    }
}

/// Try to get bool constant from a [`ExprImpl`].
/// If `expr` is not a [`ExprImpl::Literal`], or the Literal is not a boolean, this function will
/// return None. Otherwise it will return the boolean value.
pub fn try_get_bool_constant(expr: &ExprImpl) -> Option<bool> {
    if let ExprImpl::Literal(l) = expr {
        if let Some(ScalarImpl::Bool(v)) = l.get_data() {
            return Some(*v);
        }
    }
    None
}

/// [`boolean_constant_fold_and`] takes the left hand side and right hands side of a [`Type::And`]
/// operator. It is required that the the lhs should always be a constant.
fn boolean_constant_fold_and(constant_lhs: ExprImpl, rhs: ExprImpl) -> ExprImpl {
    if try_get_bool_constant(&constant_lhs).unwrap() {
        // true And rhs <=> rhs
        rhs
    } else {
        // false And rhs <=> false
        constant_lhs
    }
}

/// [`boolean_constant_fold_or`] takes the left hand side and right hands side of a [`Type::Or`]
/// operator. It is required that the the lhs should always be a constant.
fn boolean_constant_fold_or(constant_lhs: ExprImpl, rhs: ExprImpl) -> ExprImpl {
    if try_get_bool_constant(&constant_lhs).unwrap() {
        // true Or rhs <=> true
        constant_lhs
    } else {
        // false Or rhs <=> false
        rhs
    }
}

/// Expand [`Type::Not`] expressions.
/// e.g. Not(A And B) will become (Not A) Or (Not B)
pub fn push_down_not(expr: ExprImpl) -> ExprImpl {
    let mut not_push_down = NotPushDown {};
    not_push_down.rewrite_expr(expr)
}

struct NotPushDown {}

impl ExprRewriter for NotPushDown {
    /// Rewrite [`Type::Not`] function call.
    /// This function will first rewrite the current node, then recursively rewrite its children.
    fn rewrite_function_call(&mut self, func_call: FunctionCall) -> ExprImpl {
        let (func_type, mut inputs, ret) = func_call.decompose();

        if func_type != Type::Not {
            let inputs = inputs
                .into_iter()
                .map(|expr| self.rewrite_expr(expr))
                .collect();
            FunctionCall::new_unchecked(func_type, inputs, ret).into()
        } else {
            // func_type == Type::Not here

            // [`Type::Not`] is an unary function
            assert_eq!(inputs.len(), 1);

            let input = inputs.pop().unwrap();
            let rewritten_not_expr = match input {
                ExprImpl::FunctionCall(func) => {
                    let (func_type, mut inputs, ret) = func.decompose();
                    match func_type {
                        // unary functions
                        Type::Not => {
                            // Not(Not(A)) <=> A
                            assert_eq!(inputs.len(), 1);
                            Ok(inputs.pop().unwrap())
                        }
                        // binary functions
                        Type::And => {
                            // Not(A And B) <=> Not(A) Or Not(B)
                            assert_eq!(inputs.len(), 2);
                            let rhs = inputs.pop().unwrap();
                            let lhs = inputs.pop().unwrap();
                            let rhs_not: ExprImpl =
                                FunctionCall::new(Type::Not, vec![rhs]).unwrap().into();
                            let lhs_not: ExprImpl =
                                FunctionCall::new(Type::Not, vec![lhs]).unwrap().into();
                            Ok(FunctionCall::new(Type::Or, vec![lhs_not, rhs_not])
                                .unwrap()
                                .into())
                        }
                        Type::Or => {
                            // Not(A Or B) <=> Not(A) And Not(B)
                            assert_eq!(inputs.len(), 2);
                            let rhs = inputs.pop().unwrap();
                            let lhs = inputs.pop().unwrap();
                            let rhs_not: ExprImpl =
                                FunctionCall::new(Type::Not, vec![rhs]).unwrap().into();
                            let lhs_not: ExprImpl =
                                FunctionCall::new(Type::Not, vec![lhs]).unwrap().into();
                            Ok(FunctionCall::new(Type::And, vec![lhs_not, rhs_not])
                                .unwrap()
                                .into())
                        }
                        _ => Err(FunctionCall::new_unchecked(func_type, inputs, ret).into()),
                    }
                }
                _ => Err(input),
            };
            match rewritten_not_expr {
                // Rewrite success. Keep rewriting current node.
                Ok(res) => self.rewrite_expr(res),
                // Rewrite failed(Current node is not a `Not` node, or cannot be pushed down).
                // Then recursively rewrite children.
                Err(input) => FunctionCall::new(Type::Not, vec![self.rewrite_expr(input)])
                    .unwrap()
                    .into(),
            }
        }
    }
}

pub fn factorization_expr(expr: ExprImpl) -> Vec<ExprImpl> {
    // For example, we assume that expr is (A & B & C & D) | (B & C & D) | (C & D & E)
    let disjunctions: Vec<ExprImpl> = to_disjunctions(expr);

    // if `expr` can not be divided into several disjunctions...
    if disjunctions.len() == 1 {
        return disjunctions;
    }

    // extract (A & B & C & D) | (B & C & D) | (C & D & E)
    // into [[A, B, C, D], [B, C, D], [C, D, E]]
    let mut disjunctions: Vec<Vec<_>> = disjunctions
        .into_iter()
        .map(|x| to_conjunctions(x).into_iter().collect())
        .collect();
    let (last, remaining) = disjunctions.split_last_mut().unwrap();
    // now greatest_common_factor == [C, D]
    let greatest_common_divider: Vec<_> = last
        .drain_filter(|factor| remaining.iter().all(|expr| expr.contains(factor)))
        .collect();
    for disjunction in remaining {
        // remove common factors
        disjunction.retain(|factor| !greatest_common_divider.contains(factor));
    }
    // now disjunctions == [[A, B], [B], [E]]
    let remaining = merge_expr_by_binary(
        disjunctions.into_iter().map(|conjunction| {
            merge_expr_by_binary(
                conjunction.into_iter(),
                ExprType::And,
                ExprImpl::literal_bool(true),
            )
        }),
        ExprType::Or,
        ExprImpl::literal_bool(false),
    );
    // now remaining is (A & B) | (B) | (E)
    // the result is C & D & ((A & B) | (B) | (E))
    greatest_common_divider
        .into_iter()
        .chain(std::iter::once(remaining))
        .map(fold_boolean_constant)
        .collect()
}

/// give a expression, and check all columns in its `input_ref` expressions less than the input
/// column number.
macro_rules! assert_input_ref {
    ($expr:expr, $input_col_num:expr) => {
        let _ = $expr.collect_input_refs($input_col_num);
    };
}
pub(crate) use assert_input_ref;

/// Collect all `InputRef`s' indexes in the expression.
///
/// # Panics
/// Panics if an `InputRef`'s index is out of bounds of the [`FixedBitSet`].
#[derive(Clone)]
pub struct CollectInputRef {
    /// All `InputRef`s' indexes are inserted into the [`FixedBitSet`].
    input_bits: FixedBitSet,
}

impl ExprVisitor<()> for CollectInputRef {
    fn merge(_: (), _: ()) {}

    fn visit_input_ref(&mut self, expr: &InputRef) {
        self.input_bits.insert(expr.index());
    }
}

impl CollectInputRef {
    /// Creates a `CollectInputRef` with an initial `input_bits`.
    pub fn new(initial_input_bits: FixedBitSet) -> Self {
        CollectInputRef {
            input_bits: initial_input_bits,
        }
    }

    /// Creates an empty `CollectInputRef` with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        CollectInputRef {
            input_bits: FixedBitSet::with_capacity(capacity),
        }
    }
}

impl From<CollectInputRef> for FixedBitSet {
    fn from(s: CollectInputRef) -> Self {
        s.input_bits
    }
}

impl Extend<usize> for CollectInputRef {
    fn extend<T: IntoIterator<Item = usize>>(&mut self, iter: T) {
        self.input_bits.extend(iter);
    }
}

/// Collect all `InputRef`s' indexes in the expressions.
///
/// # Panics
/// Panics if `input_ref >= input_col_num`.
pub fn collect_input_refs<'a>(
    input_col_num: usize,
    exprs: impl IntoIterator<Item = &'a ExprImpl>,
) -> FixedBitSet {
    let mut input_ref_collector = CollectInputRef::with_capacity(input_col_num);
    for expr in exprs {
        input_ref_collector.visit_expr(expr);
    }
    input_ref_collector.into()
}

/// Count `Now`s in the expression.
#[derive(Clone, Default)]
pub struct CountNow {}

impl ExprVisitor<usize> for CountNow {
    fn merge(a: usize, b: usize) -> usize {
        a + b
    }

    fn visit_now(&mut self, _: &super::Now) -> usize {
        1
    }
}

/// analyze if the expression can derive a watermark from some input watermark. If it can
/// derive, return the input watermark column index
pub fn try_derive_watermark(expr: &ExprImpl) -> WatermarkDerivation {
    let a = WatermarkAnalyzer {};
    a.visit_expr(expr)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WatermarkDerivation {
    /// The expression will return a constant and not depends on its input.
    Constant,
    /// Can derive a watermark if an input column has watermark, the usize field is the input
    /// column index.
    Watermark(usize),
    /// For nondecreasing functions, we can always produce watermarks from where they are called.
    Nondecreasing,
    /// Can not derive a watermark in any cases.
    None,
}

#[derive(Clone, Default)]
struct WatermarkAnalyzer {}

impl WatermarkAnalyzer {
    fn visit_expr(&self, expr: &ExprImpl) -> WatermarkDerivation {
        match expr {
            ExprImpl::InputRef(inner) => WatermarkDerivation::Watermark(inner.index()),
            ExprImpl::Literal(_) => WatermarkDerivation::Constant,
            ExprImpl::FunctionCall(inner) => self.visit_function_call(inner),
            ExprImpl::FunctionCallWithLambda(inner) => self.visit_function_call(inner.base()),
            ExprImpl::TableFunction(_) => WatermarkDerivation::None,
            ExprImpl::Subquery(_)
            | ExprImpl::AggCall(_)
            | ExprImpl::CorrelatedInputRef(_)
            | ExprImpl::WindowFunction(_)
            | ExprImpl::Parameter(_)
            | ExprImpl::Now(_) => unreachable!(),
            ExprImpl::UserDefinedFunction(_) => WatermarkDerivation::None,
        }
    }

    fn visit_unary_op(&self, inputs: &[ExprImpl]) -> WatermarkDerivation {
        assert_eq!(inputs.len(), 1);
        self.visit_expr(&inputs[0])
    }

    fn visit_binary_op(&self, inputs: &[ExprImpl]) -> (WatermarkDerivation, WatermarkDerivation) {
        assert_eq!(inputs.len(), 2);
        (self.visit_expr(&inputs[0]), self.visit_expr(&inputs[1]))
    }

    fn visit_ternary_op(
        &self,
        inputs: &[ExprImpl],
    ) -> (
        WatermarkDerivation,
        WatermarkDerivation,
        WatermarkDerivation,
    ) {
        assert_eq!(inputs.len(), 3);
        (
            self.visit_expr(&inputs[0]),
            self.visit_expr(&inputs[1]),
            self.visit_expr(&inputs[2]),
        )
    }

    fn visit_function_call(&self, func_call: &FunctionCall) -> WatermarkDerivation {
        use WatermarkDerivation::{Constant, Nondecreasing, Watermark};
        match func_call.func_type() {
            ExprType::Unspecified => unreachable!(),
            ExprType::Add => match self.visit_binary_op(func_call.inputs()) {
                (Constant, Constant) => Constant,
                (Constant, Watermark(idx)) | (Watermark(idx), Constant) => Watermark(idx),
                (Constant, Nondecreasing) | (Nondecreasing, Constant) => Nondecreasing,
                _ => WatermarkDerivation::None,
            },
            ExprType::Subtract | ExprType::TumbleStart => {
                match self.visit_binary_op(func_call.inputs()) {
                    (Constant, Constant) => Constant,
                    (Watermark(idx), Constant) => Watermark(idx),
                    (Nondecreasing, Constant) => Nondecreasing,
                    _ => WatermarkDerivation::None,
                }
            }
            ExprType::Multiply | ExprType::Divide | ExprType::Modulus => {
                match self.visit_binary_op(func_call.inputs()) {
                    (Constant, Constant) => Constant,
                    // not meaningful to derive watermark for other situations
                    _ => WatermarkDerivation::None,
                }
            }
            ExprType::AtTimeZone => match self.visit_binary_op(func_call.inputs()) {
                (Constant, Constant) => Constant,
                (derivation @ (Watermark(_) | Nondecreasing), Constant) => {
                    if !(func_call.return_type() == DataType::Timestamptz
                        && func_call.inputs()[0].return_type() == DataType::Timestamp)
                        && func_call.inputs()[1]
                            .as_literal()
                            .and_then(|literal| literal.get_data().as_ref())
                            .map_or(true, |time_zone| {
                                !time_zone.as_utf8().eq_ignore_ascii_case("UTC")
                            })
                    {
                        WatermarkDerivation::None
                    } else {
                        derivation
                    }
                }
                _ => WatermarkDerivation::None,
            },
            ExprType::AddWithTimeZone | ExprType::SubtractWithTimeZone => {
                // Requires time zone and interval to be literal, at least for now.
                let time_zone = match &func_call.inputs()[2] {
                    ExprImpl::Literal(lit) => lit.get_data().as_ref().map(|s| s.as_utf8()),
                    _ => return WatermarkDerivation::None,
                };
                let interval = match &func_call.inputs()[1] {
                    ExprImpl::Literal(lit) => lit.get_data().as_ref().map(|s| s.as_interval()),
                    _ => return WatermarkDerivation::None,
                };
                // null zone or null interval is treated same as const `interval '1' second`, to be
                // consistent with other match arms.
                let zone_without_dst = time_zone.map_or(true, |s| s.eq_ignore_ascii_case("UTC"));
                let quantitative_only = interval.map_or(true, |v| {
                    v.months() == 0 && (v.days() == 0 || zone_without_dst)
                });
                match (self.visit_expr(&func_call.inputs()[0]), quantitative_only) {
                    (Constant, _) => Constant,
                    (Watermark(idx), true) => Watermark(idx),
                    (Nondecreasing, true) => Nondecreasing,
                    (Watermark(_) | Nondecreasing, false) => WatermarkDerivation::None,
                    (WatermarkDerivation::None, _) => WatermarkDerivation::None,
                }
            }
            ExprType::DateTrunc => match func_call.inputs().len() {
                2 => match self.visit_binary_op(func_call.inputs()) {
                    (Constant, any_derivation) => any_derivation,
                    _ => WatermarkDerivation::None,
                },
                3 => match self.visit_ternary_op(func_call.inputs()) {
                    (Constant, Constant, Constant) => Constant,
                    (Constant, derivation @ (Watermark(_) | Nondecreasing), Constant) => {
                        let zone_without_dst = func_call.inputs()[2]
                            .as_literal()
                            .and_then(|literal| literal.get_data().as_ref())
                            .map_or(false, |s| s.as_utf8().eq_ignore_ascii_case("UTC"));
                        if zone_without_dst {
                            derivation
                        } else {
                            WatermarkDerivation::None
                        }
                    }
                    _ => WatermarkDerivation::None,
                },
                _ => unreachable!(),
            },
            ExprType::ToTimestamp => self.visit_unary_op(func_call.inputs()),
            ExprType::ToTimestamp1 => WatermarkDerivation::None,
            ExprType::Cast => {
                // TODO: need more derivation
                WatermarkDerivation::None
            }
            ExprType::Case => {
                // TODO: do we need derive watermark when every case can derive a common watermark?
                WatermarkDerivation::None
            }
            ExprType::Proctime => Nondecreasing,
            _ => WatermarkDerivation::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::types::{DataType, ScalarImpl};
    use risingwave_pb::expr::expr_node::Type;

    use super::{fold_boolean_constant, push_down_not};
    use crate::expr::{ExprImpl, FunctionCall, InputRef};

    #[test]
    fn constant_boolean_folding_basic_and() {
        // expr := A && true
        let expr: ExprImpl = FunctionCall::new(
            Type::And,
            vec![
                InputRef::new(0, DataType::Boolean).into(),
                ExprImpl::literal_bool(true),
            ],
        )
        .unwrap()
        .into();

        let res = fold_boolean_constant(expr);

        assert!(res.as_input_ref().is_some());
        let res = res.as_input_ref().unwrap();
        assert_eq!(res.index(), 0);

        // expr := A && false
        let expr: ExprImpl = FunctionCall::new(
            Type::And,
            vec![
                InputRef::new(0, DataType::Boolean).into(),
                ExprImpl::literal_bool(false),
            ],
        )
        .unwrap()
        .into();

        let res = fold_boolean_constant(expr);
        assert!(res.as_literal().is_some());
        let res = res.as_literal().unwrap();
        assert_eq!(*res.get_data(), Some(ScalarImpl::Bool(false)));
    }

    #[test]
    fn constant_boolean_folding_basic_or() {
        // expr := A || true
        let expr: ExprImpl = FunctionCall::new(
            Type::Or,
            vec![
                InputRef::new(0, DataType::Boolean).into(),
                ExprImpl::literal_bool(true),
            ],
        )
        .unwrap()
        .into();

        let res = fold_boolean_constant(expr);
        assert!(res.as_literal().is_some());
        let res = res.as_literal().unwrap();
        assert_eq!(*res.get_data(), Some(ScalarImpl::Bool(true)));

        // expr := A || false
        let expr: ExprImpl = FunctionCall::new(
            Type::Or,
            vec![
                InputRef::new(0, DataType::Boolean).into(),
                ExprImpl::literal_bool(false),
            ],
        )
        .unwrap()
        .into();

        let res = fold_boolean_constant(expr);

        assert!(res.as_input_ref().is_some());
        let res = res.as_input_ref().unwrap();
        assert_eq!(res.index(), 0);
    }

    #[test]
    fn constant_boolean_folding_complex() {
        // expr := (false && true) && (true || 1 == 2)
        let expr: ExprImpl = FunctionCall::new(
            Type::And,
            vec![
                FunctionCall::new(
                    Type::And,
                    vec![ExprImpl::literal_bool(false), ExprImpl::literal_bool(true)],
                )
                .unwrap()
                .into(),
                FunctionCall::new(
                    Type::Or,
                    vec![
                        ExprImpl::literal_bool(true),
                        FunctionCall::new(
                            Type::Equal,
                            vec![ExprImpl::literal_int(1), ExprImpl::literal_int(2)],
                        )
                        .unwrap()
                        .into(),
                    ],
                )
                .unwrap()
                .into(),
            ],
        )
        .unwrap()
        .into();

        let res = fold_boolean_constant(expr);

        assert!(res.as_literal().is_some());
        let res = res.as_literal().unwrap();
        assert_eq!(*res.get_data(), Some(ScalarImpl::Bool(false)));
    }

    #[test]
    fn not_push_down_test() {
        // Not(Not(A))
        let expr: ExprImpl = FunctionCall::new(
            Type::Not,
            vec![
                FunctionCall::new(Type::Not, vec![InputRef::new(0, DataType::Boolean).into()])
                    .unwrap()
                    .into(),
            ],
        )
        .unwrap()
        .into();
        let res = push_down_not(expr);
        assert!(res.as_input_ref().is_some());
        // Not(A And Not(B)) <=> Not(A) Or B
        let expr: ExprImpl = FunctionCall::new(
            Type::Not,
            vec![FunctionCall::new(
                Type::And,
                vec![
                    InputRef::new(0, DataType::Boolean).into(),
                    FunctionCall::new(Type::Not, vec![InputRef::new(1, DataType::Boolean).into()])
                        .unwrap()
                        .into(),
                ],
            )
            .unwrap()
            .into()],
        )
        .unwrap()
        .into();
        let res = push_down_not(expr);
        assert!(res.as_function_call().is_some());
        let res = res.as_function_call().unwrap().clone();
        let (func, lhs, rhs) = res.decompose_as_binary();
        assert_eq!(func, Type::Or);
        assert!(rhs.as_input_ref().is_some());
        assert!(lhs.as_function_call().is_some());
        let lhs = lhs.as_function_call().unwrap().clone();
        let (func, input) = lhs.decompose_as_unary();
        assert_eq!(func, Type::Not);
        assert!(input.as_input_ref().is_some());
        // Not(A Or B) <=> Not(A) And Not(B)
        let expr: ExprImpl = FunctionCall::new(
            Type::Not,
            vec![FunctionCall::new(
                Type::Or,
                vec![
                    InputRef::new(0, DataType::Boolean).into(),
                    InputRef::new(1, DataType::Boolean).into(),
                ],
            )
            .unwrap()
            .into()],
        )
        .unwrap()
        .into();
        let res = push_down_not(expr);
        assert!(res.as_function_call().is_some());
        let (func_type, lhs, rhs) = res
            .as_function_call()
            .unwrap()
            .clone()
            .decompose_as_binary();
        assert_eq!(func_type, Type::And);
        let (lhs_type, lhs_input) = lhs.as_function_call().unwrap().clone().decompose_as_unary();
        assert_eq!(lhs_type, Type::Not);
        assert!(lhs_input.as_input_ref().is_some());
        let (rhs_type, rhs_input) = rhs.as_function_call().unwrap().clone().decompose_as_unary();
        assert_eq!(rhs_type, Type::Not);
        assert!(rhs_input.as_input_ref().is_some());
    }
}
