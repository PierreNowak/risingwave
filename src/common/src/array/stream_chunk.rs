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

use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::{fmt, mem};

use itertools::Itertools;
use rand::prelude::SmallRng;
use rand::{Rng, SeedableRng};
use risingwave_pb::data::{PbOp, PbStreamChunk};

use super::vis::VisMut;
use super::{ArrayImpl, ArrayRef, ArrayResult, DataChunkTestExt};
use crate::array::{DataChunk, Vis};
use crate::buffer::Bitmap;
use crate::estimate_size::EstimateSize;
use crate::field_generator::VarcharProperty;
use crate::row::Row;
use crate::types::{DataType, DefaultOrdered, ToText};
use crate::util::iter_util::ZipEqDebug;
/// `Op` represents three operations in `StreamChunk`.
///
/// `UpdateDelete` and `UpdateInsert` are semantically equivalent to `Delete` and `Insert`
/// but always appear in pairs to represent an update operation.
/// For example, table source, aggregation and outer join can generate updates by themselves,
/// while most of the other operators only pass through updates with best effort.
#[derive(Clone, Copy, Debug, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub enum Op {
    Insert,
    Delete,
    UpdateDelete,
    UpdateInsert,
}

impl Op {
    pub fn to_protobuf(self) -> PbOp {
        match self {
            Op::Insert => PbOp::Insert,
            Op::Delete => PbOp::Delete,
            Op::UpdateInsert => PbOp::UpdateInsert,
            Op::UpdateDelete => PbOp::UpdateDelete,
        }
    }

    pub fn from_protobuf(prost: &i32) -> ArrayResult<Op> {
        let op = match PbOp::from_i32(*prost) {
            Some(PbOp::Insert) => Op::Insert,
            Some(PbOp::Delete) => Op::Delete,
            Some(PbOp::UpdateInsert) => Op::UpdateInsert,
            Some(PbOp::UpdateDelete) => Op::UpdateDelete,
            Some(PbOp::Unspecified) => unreachable!(),
            None => bail!("No such op type"),
        };
        Ok(op)
    }
}

pub type Ops<'a> = &'a [Op];

/// `StreamChunk` is used to pass data over the streaming pathway.
#[derive(Clone, PartialEq)]
pub struct StreamChunk {
    // TODO: Optimize using bitmap
    ops: Arc<[Op]>,
    data: DataChunk,
}

impl Default for StreamChunk {
    /// Create a 0-row-0-col `StreamChunk`. Only used in some existing tests.
    /// This is NOT the same as an **empty** chunk, which has 0 rows but with
    /// columns aligned with executor schema.
    fn default() -> Self {
        Self {
            ops: Arc::new([]),
            data: DataChunk::new(vec![], 0),
        }
    }
}

impl StreamChunk {
    pub fn new(
        ops: impl Into<Arc<[Op]>>,
        columns: Vec<ArrayRef>,
        visibility: Option<Bitmap>,
    ) -> Self {
        let ops = ops.into();
        for col in &columns {
            assert_eq!(col.len(), ops.len());
        }

        let vis = match visibility {
            Some(b) => Vis::Bitmap(b),
            None => Vis::Compact(ops.len()),
        };
        let data = DataChunk::new(columns, vis);
        StreamChunk { ops, data }
    }

    /// Build a `StreamChunk` from rows.
    // TODO: introducing something like `StreamChunkBuilder` maybe better.
    pub fn from_rows(rows: &[(Op, impl Row)], data_types: &[DataType]) -> Self {
        let mut array_builders = data_types
            .iter()
            .map(|data_type| data_type.create_array_builder(rows.len()))
            .collect::<Vec<_>>();
        let mut ops = vec![];

        for (op, row) in rows {
            ops.push(*op);
            for (datum, builder) in row.iter().zip_eq_debug(array_builders.iter_mut()) {
                builder.append(datum);
            }
        }

        let new_columns = array_builders
            .into_iter()
            .map(|builder| builder.finish().into())
            .collect::<Vec<_>>();
        StreamChunk::new(ops, new_columns, None)
    }

    /// Get the reference of the underlying data chunk.
    pub fn data_chunk(&self) -> &DataChunk {
        &self.data
    }

    /// compact the `StreamChunk` with its visibility map
    pub fn compact(self) -> Self {
        if self.visibility().is_none() {
            return self;
        }

        let (ops, columns, visibility) = self.into_inner();
        let visibility = visibility.as_visibility().unwrap();

        let cardinality = visibility
            .iter()
            .fold(0, |vis_cnt, vis| vis_cnt + vis as usize);
        let columns: Vec<_> = columns
            .into_iter()
            .map(|col| col.compact(visibility, cardinality).into())
            .collect();
        let mut new_ops = Vec::with_capacity(cardinality);
        for idx in visibility.iter_ones() {
            new_ops.push(ops[idx]);
        }
        StreamChunk::new(new_ops, columns, None)
    }

    pub fn into_parts(self) -> (DataChunk, Arc<[Op]>) {
        (self.data, self.ops)
    }

    pub fn from_parts(ops: impl Into<Arc<[Op]>>, data_chunk: DataChunk) -> Self {
        let (columns, vis) = data_chunk.into_parts();
        Self::new(ops, columns, vis.into_visibility())
    }

    pub fn into_inner(self) -> (Arc<[Op]>, Vec<ArrayRef>, Vis) {
        let (columns, vis) = self.data.into_parts();
        (self.ops, columns, vis)
    }

    pub fn to_protobuf(&self) -> PbStreamChunk {
        PbStreamChunk {
            cardinality: self.cardinality() as u32,
            ops: self.ops.iter().map(|op| op.to_protobuf() as i32).collect(),
            columns: self.columns().iter().map(|col| col.to_protobuf()).collect(),
        }
    }

    pub fn from_protobuf(prost: &PbStreamChunk) -> ArrayResult<Self> {
        let cardinality = prost.get_cardinality() as usize;
        let mut ops = Vec::with_capacity(cardinality);
        for op in prost.get_ops() {
            ops.push(Op::from_protobuf(op)?);
        }
        let mut columns = vec![];
        for column in prost.get_columns() {
            columns.push(ArrayImpl::from_protobuf(column, cardinality)?.into());
        }
        Ok(StreamChunk::new(ops, columns, None))
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// `to_pretty_string` returns a table-like text representation of the `StreamChunk`.
    pub fn to_pretty_string(&self) -> String {
        use comfy_table::{Cell, CellAlignment, Table};

        if self.cardinality() == 0 {
            return "(empty)".to_owned();
        }

        let mut table = Table::new();
        table.load_preset("||--+-++|    ++++++");
        for (op, row_ref) in self.rows() {
            let mut cells = Vec::with_capacity(row_ref.len() + 1);
            cells.push(
                Cell::new(match op {
                    Op::Insert => "+",
                    Op::Delete => "-",
                    Op::UpdateDelete => "U-",
                    Op::UpdateInsert => "U+",
                })
                .set_alignment(CellAlignment::Right),
            );
            for datum in row_ref.iter() {
                let str = match datum {
                    None => "".to_owned(), // NULL
                    Some(scalar) => scalar.to_text(),
                };
                cells.push(Cell::new(str));
            }
            table.add_row(cells);
        }
        table.to_string()
    }

    /// Reorder (and possibly remove) columns.
    ///
    /// e.g. if `indices` is `[2, 1, 0]`, and the chunk contains column `[a, b, c]`, then the output
    /// will be `[c, b, a]`. If `indices` is [2, 0], then the output will be `[c, a]`.
    /// If the input mapping is identity mapping, no reorder will be performed.
    pub fn project(&self, indices: &[usize]) -> Self {
        Self {
            ops: self.ops.clone(),
            data: self.data.project(indices),
        }
    }

    /// Reorder columns and set visibility.
    pub fn project_with_vis(&self, indices: &[usize], vis: Vis) -> Self {
        Self {
            ops: self.ops.clone(),
            data: self.data.project_with_vis(indices, vis),
        }
    }

    /// Clone the `StreamChunk` with a new visibility.
    pub fn with_visibility(&self, vis: Vis) -> Self {
        Self {
            ops: self.ops.clone(),
            data: self.data.with_visibility(vis),
        }
    }
}

impl Deref for StreamChunk {
    type Target = DataChunk;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for StreamChunk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

/// `StreamChunk` can be created from `DataChunk` with all operations set to `Insert`.
impl From<DataChunk> for StreamChunk {
    fn from(data: DataChunk) -> Self {
        Self::from_parts(vec![Op::Insert; data.capacity()], data)
    }
}

impl fmt::Debug for StreamChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            write!(
                f,
                "StreamChunk {{ cardinality: {}, capacity: {}, data: \n{}\n }}",
                self.cardinality(),
                self.capacity(),
                self.to_pretty_string()
            )
        } else {
            f.debug_struct("StreamChunk")
                .field("cardinality", &self.cardinality())
                .field("capacity", &self.capacity())
                .finish_non_exhaustive()
        }
    }
}

impl EstimateSize for StreamChunk {
    fn estimated_heap_size(&self) -> usize {
        self.data.estimated_heap_size() + self.ops.len() * size_of::<Op>()
    }
}

enum OpsMutState {
    ArcRef(Arc<[Op]>),
    Mut(Vec<Op>),
}

impl OpsMutState {
    const UNDEFINED: Self = Self::Mut(Vec::new());
}

pub struct OpsMut {
    state: OpsMutState,
}

impl OpsMut {
    pub fn new(ops: Arc<[Op]>) -> Self {
        Self {
            state: OpsMutState::ArcRef(ops),
        }
    }

    pub fn set(&mut self, n: usize, val: Op) {
        if let OpsMutState::Mut(v) = &mut self.state {
            v[n] = val;
        } else {
            let state = mem::replace(&mut self.state, OpsMutState::UNDEFINED); // intermediate state
            let mut v = match state {
                OpsMutState::ArcRef(v) => v.to_vec(),
                OpsMutState::Mut(_) => unreachable!(),
            };
            v[n] = val;
            self.state = OpsMutState::Mut(v);
        }
    }
}
impl From<OpsMut> for Arc<[Op]> {
    fn from(v: OpsMut) -> Self {
        match v.state {
            OpsMutState::ArcRef(a) => a,
            OpsMutState::Mut(v) => v.into(),
        }
    }
}

/// A mutable wrapper for `StreamChunk`. can only set the visibilities and ops in place, can not
/// change the length.
struct StreamChunkMut {
    columns: Arc<[ArrayRef]>,
    ops: OpsMut,
    vis: VisMut,
}

impl From<StreamChunk> for StreamChunkMut {
    fn from(c: StreamChunk) -> Self {
        let (c, ops) = c.into_parts();
        let (columns, vis) = c.into_parts_v2();
        Self {
            columns,
            ops: OpsMut::new(ops),
            vis: vis.into(),
        }
    }
}

impl From<StreamChunkMut> for StreamChunk {
    fn from(c: StreamChunkMut) -> Self {
        StreamChunk::from_parts(c.ops, DataChunk::from_parts(c.columns, c.vis.into()))
    }
}
pub struct OpRowMutRef<'a> {
    c: &'a mut StreamChunkMut,
    i: usize,
}

impl OpRowMutRef<'_> {
    pub fn set_vis(&mut self, val: bool) {
        self.c.set_vis(self.i, val);
    }

    pub fn set_op(&mut self, val: Op) {
        self.c.set_op(self.i, val);
    }
}

impl StreamChunkMut {
    pub fn set_vis(&mut self, n: usize, val: bool) {
        self.vis.set(n, val);
    }

    pub fn set_op(&mut self, n: usize, val: Op) {
        self.ops.set(n, val);
    }

    /// get the mut reference of the stream chunk.
    pub fn to_mut_rows(&self) -> impl Iterator<Item = OpRowMutRef<'_>> {
        unsafe {
            (0..self.vis.len()).map(|i| {
                let p = self as *const StreamChunkMut;
                let p = p as *mut StreamChunkMut;
                OpRowMutRef { c: &mut *p, i }
            })
        }
    }
}
/// Test utilities for [`StreamChunk`].
pub trait StreamChunkTestExt: Sized {
    fn from_pretty(s: &str) -> Self;

    /// Validate the `StreamChunk` layout.
    fn valid(&self) -> bool;

    /// Concatenate multiple `StreamChunk` into one.
    fn concat(chunks: Vec<Self>) -> Self;

    /// Sort rows.
    fn sort_rows(self) -> Self;

    /// Generate stream chunks
    fn gen_stream_chunks(
        num_of_chunks: usize,
        chunk_size: usize,
        data_types: &[DataType],
        varchar_properties: &VarcharProperty,
    ) -> Vec<Self>;

    fn gen_stream_chunks_inner(
        num_of_chunks: usize,
        chunk_size: usize,
        data_types: &[DataType],
        varchar_properties: &VarcharProperty,
        visibility_percent: f64, // % of rows that are visible
        inserts_percent: f64,
    ) -> Vec<Self>;
}

impl StreamChunkTestExt for StreamChunk {
    /// Parse a chunk from string.
    ///
    /// See also [`DataChunkTestExt::from_pretty`].
    ///
    /// # Format
    ///
    /// The first line is a header indicating the column types.
    /// The following lines indicate rows within the chunk.
    /// Each line starts with an operation followed by values.
    /// NULL values are represented as `.`.
    ///
    /// # Example
    /// ```
    /// use risingwave_common::array::stream_chunk::StreamChunkTestExt as _;
    /// use risingwave_common::array::StreamChunk;
    /// let chunk = StreamChunk::from_pretty(
    ///     "  I I I I      // type chars
    ///     U- 2 5 . .      // '.' means NULL
    ///     U+ 2 5 2 6 D    // 'D' means deleted in visibility
    ///     +  . . 4 8      // ^ comments are ignored
    ///     -  . . 3 4",
    /// );
    /// //  ^ operations:
    /// //     +: Insert
    /// //     -: Delete
    /// //    U+: UpdateInsert
    /// //    U-: UpdateDelete
    ///
    /// // type chars:
    /// //     I: i64
    /// //     i: i32
    /// //     F: f64
    /// //     f: f32
    /// //     T: str
    /// //    TS: Timestamp
    /// //    TZ: Timestamptz
    /// //   SRL: Serial
    /// //   x[]: array of x
    /// // {i,f}: struct
    /// ```
    fn from_pretty(s: &str) -> Self {
        let mut chunk_str = String::new();
        let mut ops = vec![];

        let (header, body) = match s.split_once('\n') {
            Some(pair) => pair,
            None => {
                // empty chunk
                return StreamChunk {
                    ops: Arc::new([]),
                    data: DataChunk::from_pretty(s),
                };
            }
        };
        chunk_str.push_str(header);
        chunk_str.push('\n');

        for line in body.split_inclusive('\n') {
            if line.trim_start().is_empty() {
                continue;
            }
            let (op, row) = line
                .trim_start()
                .split_once(|c: char| c.is_ascii_whitespace())
                .ok_or_else(|| panic!("missing operation: {line:?}"))
                .unwrap();
            ops.push(match op {
                "+" => Op::Insert,
                "-" => Op::Delete,
                "U+" => Op::UpdateInsert,
                "U-" => Op::UpdateDelete,
                t => panic!("invalid op: {t:?}"),
            });
            chunk_str.push_str(row);
        }
        StreamChunk {
            ops: ops.into(),
            data: DataChunk::from_pretty(&chunk_str),
        }
    }

    fn valid(&self) -> bool {
        let len = self.ops.len();
        let data = &self.data;
        data.vis().len() == len && data.columns().iter().all(|col| col.len() == len)
    }

    fn concat(chunks: Vec<StreamChunk>) -> StreamChunk {
        assert!(!chunks.is_empty());
        let mut ops = vec![];
        let mut data_chunks = vec![];
        let mut capacity = 0;
        for chunk in chunks {
            capacity += chunk.capacity();
            ops.extend(chunk.ops.iter());
            data_chunks.push(chunk.data);
        }
        let data = DataChunk::rechunk(&data_chunks, capacity)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        StreamChunk {
            ops: ops.into(),
            data,
        }
    }

    fn sort_rows(self) -> Self {
        if self.capacity() == 0 {
            return self;
        }
        let rows = self.rows().collect_vec();
        let mut idx = (0..self.capacity()).collect_vec();
        idx.sort_by_key(|&i| {
            let (op, row_ref) = rows[i];
            (op, DefaultOrdered(row_ref))
        });
        StreamChunk {
            ops: idx.iter().map(|&i| self.ops[i]).collect(),
            data: self.data.reorder_rows(&idx),
        }
    }

    /// Generate `num_of_chunks` data chunks with type `data_types`,
    /// where each data chunk has cardinality of `chunk_size`.
    /// TODO(kwannoel): Generate different types of op, different vis.
    fn gen_stream_chunks(
        num_of_chunks: usize,
        chunk_size: usize,
        data_types: &[DataType],
        varchar_properties: &VarcharProperty,
    ) -> Vec<StreamChunk> {
        Self::gen_stream_chunks_inner(
            num_of_chunks,
            chunk_size,
            data_types,
            varchar_properties,
            1.0,
            1.0,
        )
    }

    fn gen_stream_chunks_inner(
        num_of_chunks: usize,
        chunk_size: usize,
        data_types: &[DataType],
        varchar_properties: &VarcharProperty,
        visibility_percent: f64, // % of rows that are visible
        inserts_percent: f64,    // Rest will be deletes.
    ) -> Vec<StreamChunk> {
        let ops = if inserts_percent == 0.0 {
            vec![Op::Delete; chunk_size]
        } else if inserts_percent == 1.0 {
            vec![Op::Insert; chunk_size]
        } else {
            let mut rng = SmallRng::from_seed([0; 32]);
            let mut ops = vec![];
            for _ in 0..chunk_size {
                ops.push(if rng.gen_bool(inserts_percent) {
                    Op::Insert
                } else {
                    Op::Delete
                });
            }
            ops
        };
        DataChunk::gen_data_chunks(
            num_of_chunks,
            chunk_size,
            data_types,
            varchar_properties,
            visibility_percent,
        )
        .into_iter()
        .map(|chunk| StreamChunk::from_parts(ops.clone(), chunk))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_pretty_string() {
        let chunk = StreamChunk::from_pretty(
            "  I I
             + 1 6
             - 2 .
            U- 3 7
            U+ 4 .",
        );
        assert_eq!(
            chunk.to_pretty_string(),
            "\
+----+---+---+
|  + | 1 | 6 |
|  - | 2 |   |
| U- | 3 | 7 |
| U+ | 4 |   |
+----+---+---+"
        );
    }
}
