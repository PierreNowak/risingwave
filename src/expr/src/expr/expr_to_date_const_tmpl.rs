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

use std::sync::Arc;

use risingwave_common::array::{Array, ArrayBuilder, DateArrayBuilder, Utf8Array};
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum, ScalarImpl};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_expr_macro::build_function;

use super::{BoxedExpression, Expression, Result};
use crate::expr::template::BinaryExpression;
use crate::vector_op::to_char::{compile_pattern_to_chrono, ChronoPattern};
use crate::vector_op::to_timestamp::{to_date, to_date_const_tmpl};

#[derive(Debug)]
struct ExprToDateConstTmpl {
    child: Box<dyn Expression>,
    chrono_pattern: ChronoPattern,
}

#[async_trait::async_trait]
impl Expression for ExprToDateConstTmpl {
    fn return_type(&self) -> DataType {
        DataType::Date
    }

    async fn eval(
        &self,
        input: &risingwave_common::array::DataChunk,
    ) -> crate::Result<risingwave_common::array::ArrayRef> {
        let data_arr = self.child.eval_checked(input).await?;
        let data_arr: &Utf8Array = data_arr.as_ref().into();
        let mut output = DateArrayBuilder::new(input.capacity());
        for (data, vis) in data_arr.iter().zip_eq_fast(input.vis().iter()) {
            if !vis {
                output.append_null();
            } else if let Some(data) = data {
                let res = to_date_const_tmpl(data, &self.chrono_pattern)?;
                output.append(Some(res));
            } else {
                output.append_null();
            }
        }

        Ok(Arc::new(output.finish().into()))
    }

    async fn eval_row(&self, input: &OwnedRow) -> crate::Result<Datum> {
        let data = self.child.eval_row(input).await?;
        Ok(if let Some(ScalarImpl::Utf8(data)) = data {
            let res = to_date_const_tmpl(&data, &self.chrono_pattern)?;
            Some(res.into())
        } else {
            None
        })
    }
}

#[build_function("char_to_date(varchar, varchar) -> date")]
fn build_to_date_expr(
    return_type: DataType,
    children: Vec<BoxedExpression>,
) -> Result<BoxedExpression> {
    use risingwave_common::array::*;

    let mut iter = children.into_iter();
    let data_expr = iter.next().unwrap();
    let tmpl_expr = iter.next().unwrap();

    Ok(if let Ok(Some(tmpl)) = tmpl_expr.eval_const() {
        ExprToDateConstTmpl {
            child: data_expr,
            chrono_pattern: compile_pattern_to_chrono(tmpl.as_utf8()),
        }
        .boxed()
    } else {
        BinaryExpression::<Utf8Array, Utf8Array, DateArray, _>::new(
            data_expr,
            tmpl_expr,
            return_type,
            to_date,
        )
        .boxed()
    })
}
