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

use std::collections::HashMap;

use risingwave_common::system_param::reader::SystemParamsReader;
use risingwave_pb::backup_service::MetaSnapshotMetadata;
use risingwave_pb::catalog::Table;
use risingwave_pb::ddl_service::DdlProgress;
use risingwave_pb::hummock::HummockSnapshot;
use risingwave_pb::meta::cancel_creating_jobs_request::PbJobs;
use risingwave_pb::meta::list_actor_states_response::ActorState;
use risingwave_pb::meta::list_fragment_distribution_response::FragmentDistribution;
use risingwave_pb::meta::list_table_fragment_states_response::TableFragmentState;
use risingwave_pb::meta::list_table_fragments_response::TableFragmentInfo;
use risingwave_rpc_client::error::Result;
use risingwave_rpc_client::{HummockMetaClient, MetaClient};

/// A wrapper around the `MetaClient` that only provides a minor set of meta rpc.
/// Most of the rpc to meta are delegated by other separate structs like `CatalogWriter`,
/// `WorkerNodeManager`, etc. So frontend rarely needs to call `MetaClient` directly.
/// Hence instead of to mock all rpc of `MetaClient` in tests, we aggregate those "direct" rpc
/// in this trait so that the mocking can be simplified.
#[async_trait::async_trait]
pub trait FrontendMetaClient: Send + Sync {
    async fn pin_snapshot(&self) -> Result<HummockSnapshot>;

    async fn get_snapshot(&self) -> Result<HummockSnapshot>;

    async fn flush(&self, checkpoint: bool) -> Result<HummockSnapshot>;

    async fn cancel_creating_jobs(&self, jobs: PbJobs) -> Result<Vec<u32>>;

    async fn list_table_fragments(
        &self,
        table_ids: &[u32],
    ) -> Result<HashMap<u32, TableFragmentInfo>>;

    async fn list_table_fragment_states(&self) -> Result<Vec<TableFragmentState>>;

    async fn list_fragment_distribution(&self) -> Result<Vec<FragmentDistribution>>;

    async fn list_actor_states(&self) -> Result<Vec<ActorState>>;

    async fn unpin_snapshot(&self) -> Result<()>;

    async fn unpin_snapshot_before(&self, epoch: u64) -> Result<()>;

    async fn list_meta_snapshots(&self) -> Result<Vec<MetaSnapshotMetadata>>;

    async fn get_system_params(&self) -> Result<SystemParamsReader>;

    async fn set_system_param(
        &self,
        param: String,
        value: Option<String>,
    ) -> Result<Option<SystemParamsReader>>;

    async fn list_ddl_progress(&self) -> Result<Vec<DdlProgress>>;

    async fn get_tables(&self, table_ids: &[u32]) -> Result<HashMap<u32, Table>>;
}

pub struct FrontendMetaClientImpl(pub MetaClient);

#[async_trait::async_trait]
impl FrontendMetaClient for FrontendMetaClientImpl {
    async fn pin_snapshot(&self) -> Result<HummockSnapshot> {
        self.0.pin_snapshot().await
    }

    async fn get_snapshot(&self) -> Result<HummockSnapshot> {
        self.0.get_snapshot().await
    }

    async fn flush(&self, checkpoint: bool) -> Result<HummockSnapshot> {
        self.0.flush(checkpoint).await
    }

    async fn cancel_creating_jobs(&self, infos: PbJobs) -> Result<Vec<u32>> {
        self.0.cancel_creating_jobs(infos).await
    }

    async fn list_table_fragments(
        &self,
        table_ids: &[u32],
    ) -> Result<HashMap<u32, TableFragmentInfo>> {
        self.0.list_table_fragments(table_ids).await
    }

    async fn list_table_fragment_states(&self) -> Result<Vec<TableFragmentState>> {
        self.0.list_table_fragment_states().await
    }

    async fn list_fragment_distribution(&self) -> Result<Vec<FragmentDistribution>> {
        self.0.list_fragment_distributions().await
    }

    async fn list_actor_states(&self) -> Result<Vec<ActorState>> {
        self.0.list_actor_states().await
    }

    async fn unpin_snapshot(&self) -> Result<()> {
        self.0.unpin_snapshot().await
    }

    async fn unpin_snapshot_before(&self, epoch: u64) -> Result<()> {
        self.0.unpin_snapshot_before(epoch).await
    }

    async fn list_meta_snapshots(&self) -> Result<Vec<MetaSnapshotMetadata>> {
        let manifest = self.0.get_meta_snapshot_manifest().await?;
        Ok(manifest.snapshot_metadata)
    }

    async fn get_system_params(&self) -> Result<SystemParamsReader> {
        self.0.get_system_params().await
    }

    async fn set_system_param(
        &self,
        param: String,
        value: Option<String>,
    ) -> Result<Option<SystemParamsReader>> {
        self.0.set_system_param(param, value).await
    }

    async fn list_ddl_progress(&self) -> Result<Vec<DdlProgress>> {
        let ddl_progress = self.0.get_ddl_progress().await?;
        Ok(ddl_progress)
    }

    async fn get_tables(&self, table_ids: &[u32]) -> Result<HashMap<u32, Table>> {
        let tables = self.0.get_tables(table_ids).await?;
        Ok(tables)
    }
}
