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

use std::future::Future;
use std::sync::atomic::AtomicUsize;

use futures::stream::BoxStream;
use futures_async_stream::try_stream;
use risingwave_common::row::OwnedRow;
use risingwave_common::types::ScalarImpl;

use crate::error::ConnectorError;
use crate::source::external::{
    CdcOffset, ConnectorResult, ExternalTableReader, MySqlOffset, SchemaTableName,
};

#[derive(Debug)]
pub struct MockExternalTableReader {
    binlog_watermarks: Vec<MySqlOffset>,
    snapshot_cnt: AtomicUsize,
}

impl MockExternalTableReader {
    pub fn new(binlog_watermarks: Vec<MySqlOffset>) -> Self {
        Self {
            binlog_watermarks,
            snapshot_cnt: AtomicUsize::new(0),
        }
    }

    #[try_stream(boxed, ok = OwnedRow, error = ConnectorError)]
    async fn snapshot_read_inner(&self) {
        let snap_idx = self
            .snapshot_cnt
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        println!("snapshot read: idx {}", snap_idx);

        let snap0 = vec![OwnedRow::new(vec![
            Some(ScalarImpl::Int64(1)),
            Some(ScalarImpl::Float64(1.0001.into())),
            None,
        ])];
        let snap1 = vec![
            OwnedRow::new(vec![
                Some(ScalarImpl::Int64(1)),
                Some(ScalarImpl::Float64(10.01.into())),
                None,
            ]),
            OwnedRow::new(vec![
                Some(ScalarImpl::Int64(2)),
                Some(ScalarImpl::Float64(2.02.into())),
                None,
            ]),
            OwnedRow::new(vec![
                Some(ScalarImpl::Int64(5)),
                Some(ScalarImpl::Float64(1.0005.into())),
                None,
            ]),
            OwnedRow::new(vec![
                Some(ScalarImpl::Int64(6)),
                Some(ScalarImpl::Float64(1.0006.into())),
                None,
            ]),
            OwnedRow::new(vec![
                Some(ScalarImpl::Int64(8)),
                Some(ScalarImpl::Float64(1.0008.into())),
                None,
            ]),
        ];

        let snapshots = vec![snap0, snap1];
        if snap_idx >= snapshots.len() {
            return Ok(());
        }

        for row in &snapshots[snap_idx] {
            yield row.clone();
        }
    }
}

impl ExternalTableReader for MockExternalTableReader {
    type CdcOffsetFuture<'a> = impl Future<Output = ConnectorResult<CdcOffset>> + 'a;

    fn get_normalized_table_name(&self, _table_name: &SchemaTableName) -> String {
        "`mock_table`".to_string()
    }

    fn current_cdc_offset(&self) -> Self::CdcOffsetFuture<'_> {
        static IDX: AtomicUsize = AtomicUsize::new(0);
        async move {
            let idx = IDX.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if idx < self.binlog_watermarks.len() {
                Ok(CdcOffset::MySql(self.binlog_watermarks[idx].clone()))
            } else {
                Ok(CdcOffset::MySql(MySqlOffset {
                    filename: "1.binlog".to_string(),
                    position: u64::MAX,
                }))
            }
        }
    }

    fn parse_binlog_offset(&self, offset: &str) -> ConnectorResult<CdcOffset> {
        // same as mysql offset
        Ok(CdcOffset::MySql(MySqlOffset::parse_str(offset)?))
    }

    fn snapshot_read(
        &self,
        _table_name: SchemaTableName,
        _start_pk: Option<OwnedRow>,
        _primary_keys: Vec<String>,
    ) -> BoxStream<'_, ConnectorResult<OwnedRow>> {
        self.snapshot_read_inner()
    }
}
