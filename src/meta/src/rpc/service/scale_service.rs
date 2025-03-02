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

use risingwave_pb::common::WorkerType;
use risingwave_pb::meta::scale_service_server::ScaleService;
use risingwave_pb::meta::{
    GetClusterInfoRequest, GetClusterInfoResponse, GetReschedulePlanRequest,
    GetReschedulePlanResponse, PauseRequest, PauseResponse, Reschedule, RescheduleRequest,
    RescheduleResponse, ResumeRequest, ResumeResponse,
};
use risingwave_pb::source::{ConnectorSplit, ConnectorSplits};
use tonic::{Request, Response, Status};

use crate::barrier::{BarrierManagerRef, BarrierScheduler, Command};
use crate::manager::{CatalogManagerRef, ClusterManagerRef, FragmentManagerRef};
use crate::model::{MetadataModel, PausedReason};
use crate::storage::MetaStore;
use crate::stream::{
    GlobalStreamManagerRef, ParallelUnitReschedule, RescheduleOptions, SourceManagerRef,
};

pub struct ScaleServiceImpl<S: MetaStore> {
    barrier_scheduler: BarrierScheduler<S>,
    fragment_manager: FragmentManagerRef<S>,
    cluster_manager: ClusterManagerRef<S>,
    source_manager: SourceManagerRef<S>,
    catalog_manager: CatalogManagerRef<S>,
    stream_manager: GlobalStreamManagerRef<S>,
    barrier_manager: BarrierManagerRef<S>,
}

impl<S> ScaleServiceImpl<S>
where
    S: MetaStore,
{
    pub fn new(
        barrier_scheduler: BarrierScheduler<S>,
        fragment_manager: FragmentManagerRef<S>,
        cluster_manager: ClusterManagerRef<S>,
        source_manager: SourceManagerRef<S>,
        catalog_manager: CatalogManagerRef<S>,
        stream_manager: GlobalStreamManagerRef<S>,
        barrier_manager: BarrierManagerRef<S>,
    ) -> Self {
        Self {
            barrier_scheduler,
            fragment_manager,
            cluster_manager,
            source_manager,
            catalog_manager,
            stream_manager,
            barrier_manager,
        }
    }
}

#[async_trait::async_trait]
impl<S> ScaleService for ScaleServiceImpl<S>
where
    S: MetaStore,
{
    #[cfg_attr(coverage, no_coverage)]
    async fn pause(&self, _: Request<PauseRequest>) -> Result<Response<PauseResponse>, Status> {
        // TODO: move this out of the scale service, as scaling actually executes `pause` and
        // `resume` with `PausedReason::ConfigChange`.
        self.barrier_scheduler
            .run_command(Command::pause(PausedReason::Manual))
            .await?;
        Ok(Response::new(PauseResponse {}))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn resume(&self, _: Request<ResumeRequest>) -> Result<Response<ResumeResponse>, Status> {
        // TODO: move this out of the scale service, as scaling actually executes `pause` and
        // `resume` with `PausedReason::ConfigChange`.
        self.barrier_scheduler
            .run_command(Command::resume(PausedReason::Manual))
            .await?;
        Ok(Response::new(ResumeResponse {}))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn get_cluster_info(
        &self,
        _: Request<GetClusterInfoRequest>,
    ) -> Result<Response<GetClusterInfoResponse>, Status> {
        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;

        let table_fragments = self
            .fragment_manager
            .get_fragment_read_guard()
            .await
            .table_fragments()
            .values()
            .map(|tf| tf.to_protobuf())
            .collect();

        let worker_nodes = self
            .cluster_manager
            .list_worker_node(WorkerType::ComputeNode, None)
            .await;

        let actor_splits = self
            .source_manager
            .get_actor_splits()
            .await
            .into_iter()
            .map(|(actor_id, splits)| {
                (
                    actor_id,
                    ConnectorSplits {
                        splits: splits.iter().map(ConnectorSplit::from).collect(),
                    },
                )
            })
            .collect();

        let sources = self.catalog_manager.list_sources().await;

        let source_infos = sources.into_iter().map(|s| (s.id, s)).collect();

        let revision = self.fragment_manager.get_revision().await.inner();

        Ok(Response::new(GetClusterInfoResponse {
            worker_nodes,
            table_fragments,
            actor_splits,
            source_infos,
            revision,
        }))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn reschedule(
        &self,
        request: Request<RescheduleRequest>,
    ) -> Result<Response<RescheduleResponse>, Status> {
        if !self.barrier_manager.is_running().await {
            return Err(Status::unavailable(
                "Rescheduling is unavailable for now. Likely the cluster is starting or recovering.",
            ));
        }

        let RescheduleRequest {
            reschedules,
            revision,
            resolve_no_shuffle_upstream,
        } = request.into_inner();

        let _reschedule_job_lock = self.stream_manager.reschedule_lock.write().await;

        let current_revision = self.fragment_manager.get_revision().await;

        if revision != current_revision.inner() {
            return Ok(Response::new(RescheduleResponse {
                success: false,
                revision: current_revision.inner(),
            }));
        }

        self.stream_manager
            .reschedule_actors(
                reschedules
                    .into_iter()
                    .map(|(fragment_id, reschedule)| {
                        let Reschedule {
                            added_parallel_units,
                            removed_parallel_units,
                        } = reschedule;

                        let added_parallel_units = added_parallel_units.into_iter().collect();
                        let removed_parallel_units = removed_parallel_units.into_iter().collect();

                        (
                            fragment_id,
                            ParallelUnitReschedule {
                                added_parallel_units,
                                removed_parallel_units,
                            },
                        )
                    })
                    .collect(),
                RescheduleOptions {
                    resolve_no_shuffle_upstream,
                },
            )
            .await?;

        let next_revision = self.fragment_manager.get_revision().await;

        Ok(Response::new(RescheduleResponse {
            success: true,
            revision: next_revision.into(),
        }))
    }

    #[cfg_attr(coverage, no_coverage)]
    async fn get_reschedule_plan(
        &self,
        request: Request<GetReschedulePlanRequest>,
    ) -> Result<Response<GetReschedulePlanResponse>, Status> {
        let req = request.into_inner();

        if !self.barrier_manager.is_running().await {
            return Err(Status::unavailable(
                "Rescheduling is unavailable for now. Likely the cluster is starting or recovering.",
            ));
        }

        let _reschedule_job_lock = self.stream_manager.reschedule_lock.read().await;

        let current_revision = self.fragment_manager.get_revision().await;

        if req.revision != current_revision.inner() {
            return Ok(Response::new(GetReschedulePlanResponse {
                success: false,
                revision: current_revision.inner(),
                reschedules: Default::default(),
            }));
        }

        let policy = req
            .policy
            .ok_or_else(|| Status::invalid_argument("policy is required"))?;

        let plan = self.stream_manager.get_reschedule_plan(policy).await?;

        let next_revision = self.fragment_manager.get_revision().await;

        // generate reschedule plan will not change the revision
        assert_eq!(current_revision, next_revision);

        Ok(Response::new(GetReschedulePlanResponse {
            success: true,
            revision: next_revision.into(),
            reschedules: plan
                .into_iter()
                .map(|(fragment_id, reschedule)| {
                    (
                        fragment_id,
                        Reschedule {
                            added_parallel_units: reschedule
                                .added_parallel_units
                                .into_iter()
                                .collect(),
                            removed_parallel_units: reschedule
                                .removed_parallel_units
                                .into_iter()
                                .collect(),
                        },
                    )
                })
                .collect(),
        }))
    }
}
