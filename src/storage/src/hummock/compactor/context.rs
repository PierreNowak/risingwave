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

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use parking_lot::RwLock;
use risingwave_rpc_client::HummockMetaClient;

use super::task_progress::TaskProgressManagerRef;
use crate::filter_key_extractor::FilterKeyExtractorManager;
use crate::hummock::compactor::CompactionExecutor;
use crate::hummock::sstable_store::SstableStoreRef;
use crate::hummock::MemoryLimiter;
use crate::monitor::CompactorMetrics;
use crate::opts::StorageOpts;

/// A `CompactorContext` describes the context of a compactor.
#[derive(Clone)]
pub struct CompactorContext {
    /// Storage options.
    pub storage_opts: Arc<StorageOpts>,

    /// The meta client.
    pub hummock_meta_client: Arc<dyn HummockMetaClient>,

    /// Sstable store that manages the sstables.
    pub sstable_store: SstableStoreRef,

    /// Statistics.
    pub compactor_metrics: Arc<CompactorMetrics>,

    /// True if it is a memory compaction (from shared buffer).
    pub is_share_buffer_compact: bool,

    pub compaction_executor: Arc<CompactionExecutor>,

    pub filter_key_extractor_manager: FilterKeyExtractorManager,

    pub memory_limiter: Arc<MemoryLimiter>,

    pub task_progress_manager: TaskProgressManagerRef,

    pub await_tree_reg: Option<Arc<RwLock<await_tree::Registry<String>>>>,

    pub running_task_count: Arc<AtomicU32>,
}

impl CompactorContext {
    pub fn new_local_compact_context(
        storage_opts: Arc<StorageOpts>,
        sstable_store: SstableStoreRef,
        hummock_meta_client: Arc<dyn HummockMetaClient>,
        compactor_metrics: Arc<CompactorMetrics>,
        filter_key_extractor_manager: FilterKeyExtractorManager,
    ) -> Self {
        let compaction_executor = if storage_opts.share_buffer_compaction_worker_threads_number == 0
        {
            Arc::new(CompactionExecutor::new(None))
        } else {
            Arc::new(CompactionExecutor::new(Some(
                storage_opts.share_buffer_compaction_worker_threads_number as usize,
            )))
        };

        // not limit memory for local compact
        Self {
            storage_opts,
            hummock_meta_client,
            sstable_store,
            compactor_metrics,
            is_share_buffer_compact: true,
            compaction_executor,
            filter_key_extractor_manager,
            memory_limiter: MemoryLimiter::unlimit(),
            task_progress_manager: Default::default(),
            await_tree_reg: None,
            running_task_count: Arc::new(AtomicU32::new(0)),
        }
    }
}
