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

use std::alloc::{Allocator, Global};
use std::borrow::Borrow;
use std::cmp::min;
use std::hash::{BuildHasher, Hash};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use lru::{DefaultHasher, KeyRef, LruCache};
use prometheus::IntGauge;
use risingwave_common::estimate_size::EstimateSize;
use risingwave_common::util::epoch::Epoch;

use crate::common::metrics::MetricsInfo;

const REPORT_SIZE_EVERY_N_KB_CHANGE: usize = 4096;

/// The managed cache is a lru cache that bounds the memory usage by epoch.
/// Should be used with `GlobalMemoryManager`.
pub struct ManagedLruCache<K, V, S = DefaultHasher, A: Clone + Allocator = Global> {
    inner: LruCache<K, V, S, A>,
    /// The entry with epoch less than water should be evicted.
    /// Should only be updated by the `GlobalMemoryManager`.
    watermark_epoch: Arc<AtomicU64>,
    /// The heap size of keys/values
    kv_heap_size: usize,
    /// The metrics of memory usage
    memory_usage_metrics: IntGauge,
    // The metrics of evicted watermark time
    lru_evicted_watermark_time_diff_ms: IntGauge,
    // Metrics info
    metrics_info: MetricsInfo,
    /// The size reported last time
    last_reported_size_bytes: usize,
}

impl<K, V, S, A: Clone + Allocator> Drop for ManagedLruCache<K, V, S, A> {
    fn drop(&mut self) {
        let info = &self.metrics_info;
        self.memory_usage_metrics.set(0.into());

        if let Err(e) = info.metrics.stream_memory_usage.remove_label_values(&[
            &info.table_id,
            &info.actor_id,
            &info.desc,
        ]) {
            warn!(
                "unable to remove stream_memory_usage of {} {} {}: {:?}",
                info.table_id, info.actor_id, info.desc, e
            );
        };
        if let Err(e) = info
            .metrics
            .lru_evicted_watermark_time_diff_ms
            .remove_label_values(&[&info.table_id, &info.actor_id, &info.desc])
        {
            warn!(
                "unable to remove lru_evicted_watermark_time_diff_ms of {} {} {}: {:?}",
                info.table_id, info.actor_id, info.desc, e
            );
        }
    }
}

impl<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher, A: Clone + Allocator>
    ManagedLruCache<K, V, S, A>
{
    pub fn new_inner(
        inner: LruCache<K, V, S, A>,
        watermark_epoch: Arc<AtomicU64>,
        metrics_info: MetricsInfo,
    ) -> Self {
        let memory_usage_metrics = metrics_info
            .metrics
            .stream_memory_usage
            .with_label_values(&[
                &metrics_info.table_id,
                &metrics_info.actor_id,
                &metrics_info.desc,
            ]);
        memory_usage_metrics.set(0.into());

        let lru_evicted_watermark_time_diff_ms = metrics_info
            .metrics
            .lru_evicted_watermark_time_diff_ms
            .with_label_values(&[
                &metrics_info.table_id,
                &metrics_info.actor_id,
                &metrics_info.desc,
            ]);
        lru_evicted_watermark_time_diff_ms.set(watermark_epoch.load(Ordering::Relaxed) as _);

        Self {
            inner,
            watermark_epoch,
            kv_heap_size: 0,
            memory_usage_metrics,
            lru_evicted_watermark_time_diff_ms,
            metrics_info,
            last_reported_size_bytes: 0,
        }
    }

    /// Evict epochs lower than the watermark
    pub fn evict(&mut self) {
        self.evict_by_epoch(self.load_cur_epoch());
    }

    /// Evict epochs lower than the watermark, except those entry which touched in this epoch
    pub fn evict_except_cur_epoch(&mut self) {
        let epoch = min(self.load_cur_epoch(), self.inner.current_epoch());
        self.evict_by_epoch(epoch);
    }

    /// Evict epochs lower than the watermark
    fn evict_by_epoch(&mut self, epoch: u64) {
        while let Some((key, value)) = self.inner.pop_lru_by_epoch(epoch) {
            self.kv_heap_size_dec(key.estimated_size() + value.estimated_size());
        }
        self.report_evicted_watermark_time(epoch);
    }

    pub fn update_epoch(&mut self, epoch: u64) {
        self.inner.update_epoch(epoch);
    }

    pub fn current_epoch(&mut self) -> u64 {
        self.inner.current_epoch()
    }

    /// An iterator visiting all values in most-recently used order. The iterator element type is
    /// &V.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.inner.iter().map(|(_k, v)| v)
    }

    pub fn put(&mut self, k: K, v: V) -> Option<V> {
        let key_size = k.estimated_size();
        self.kv_heap_size_inc(key_size + v.estimated_size());
        let old_val = self.inner.put(k, v);
        if let Some(old_val) = &old_val {
            self.kv_heap_size_dec(key_size + old_val.estimated_size());
        }
        old_val
    }

    pub fn get_mut(&mut self, k: &K) -> Option<MutGuard<'_, V>> {
        let v = self.inner.get_mut(k);
        v.map(|inner| {
            MutGuard::new(
                inner,
                &mut self.kv_heap_size,
                &mut self.last_reported_size_bytes,
                &mut self.memory_usage_metrics,
            )
        })
    }

    pub fn get_mut_unsafe(&mut self, k: &K) -> Option<UnsafeMutGuard<V>> {
        let v = self.inner.get_mut(k);
        v.map(|inner| {
            UnsafeMutGuard::new(
                inner,
                &mut self.kv_heap_size,
                &mut self.last_reported_size_bytes,
                &mut self.memory_usage_metrics,
            )
        })
    }

    pub fn get<Q>(&mut self, k: &Q) -> Option<&V>
    where
        KeyRef<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.get(k)
    }

    pub fn peek_mut(&mut self, k: &K) -> Option<MutGuard<'_, V>> {
        let v = self.inner.peek_mut(k);
        v.map(|inner| {
            MutGuard::new(
                inner,
                &mut self.kv_heap_size,
                &mut self.last_reported_size_bytes,
                &mut self.memory_usage_metrics,
            )
        })
    }

    pub fn push(&mut self, k: K, v: V) -> Option<(K, V)> {
        self.kv_heap_size_inc(k.estimated_size() + v.estimated_size());

        let old_kv = self.inner.push(k, v);

        if let Some((old_key, old_val)) = &old_kv {
            self.kv_heap_size_dec(old_key.estimated_size() + old_val.estimated_size());
        }
        old_kv
    }

    pub fn contains<Q>(&self, k: &Q) -> bool
    where
        KeyRef<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.contains(k)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    fn kv_heap_size_inc(&mut self, size: usize) {
        self.kv_heap_size = self.kv_heap_size.saturating_add(size);
        self.report_memory_usage();
    }

    fn kv_heap_size_dec(&mut self, size: usize) {
        self.kv_heap_size = self.kv_heap_size.saturating_sub(size);
        self.report_memory_usage();
    }

    fn report_memory_usage(&mut self) -> bool {
        if self.kv_heap_size.abs_diff(self.last_reported_size_bytes)
            > REPORT_SIZE_EVERY_N_KB_CHANGE << 10
        {
            self.memory_usage_metrics.set(self.kv_heap_size as _);
            self.last_reported_size_bytes = self.kv_heap_size;
            true
        } else {
            false
        }
    }

    fn report_evicted_watermark_time(&self, epoch: u64) {
        self.lru_evicted_watermark_time_diff_ms.set(
            (Epoch(self.load_cur_epoch()).physical_time() - Epoch(epoch).physical_time()) as _,
        );
    }

    fn load_cur_epoch(&self) -> u64 {
        self.watermark_epoch.load(Ordering::Relaxed)
    }
}

pub fn new_unbounded<K: Hash + Eq + EstimateSize, V: EstimateSize>(
    watermark_epoch: Arc<AtomicU64>,
    metrics_info: MetricsInfo,
) -> ManagedLruCache<K, V> {
    ManagedLruCache::new_inner(LruCache::unbounded(), watermark_epoch, metrics_info)
}

pub fn new_with_hasher_in<
    K: Hash + Eq + EstimateSize,
    V: EstimateSize,
    S: BuildHasher,
    A: Clone + Allocator,
>(
    watermark_epoch: Arc<AtomicU64>,
    metrics_info: MetricsInfo,
    hasher: S,
    alloc: A,
) -> ManagedLruCache<K, V, S, A> {
    ManagedLruCache::new_inner(
        LruCache::unbounded_with_hasher_in(hasher, alloc),
        watermark_epoch,
        metrics_info,
    )
}

pub fn new_with_hasher<K: Hash + Eq + EstimateSize, V: EstimateSize, S: BuildHasher>(
    watermark_epoch: Arc<AtomicU64>,
    metrics_info: MetricsInfo,
    hasher: S,
) -> ManagedLruCache<K, V, S> {
    ManagedLruCache::new_inner(
        LruCache::unbounded_with_hasher(hasher),
        watermark_epoch,
        metrics_info,
    )
}

pub struct MutGuard<'a, V: EstimateSize> {
    inner: &'a mut V,
    // The size of the original value
    original_val_size: usize,
    // The total size of a collection
    total_size: &'a mut usize,
    last_reported_size_bytes: &'a mut usize,
    memory_usage_metrics: &'a mut IntGauge,
}

impl<'a, V: EstimateSize> MutGuard<'a, V> {
    pub fn new(
        inner: &'a mut V,
        total_size: &'a mut usize,
        last_reported_size_bytes: &'a mut usize,
        memory_usage_metrics: &'a mut IntGauge,
    ) -> Self {
        let original_val_size = inner.estimated_size();
        Self {
            inner,
            original_val_size,
            total_size,
            last_reported_size_bytes,
            memory_usage_metrics,
        }
    }

    fn report_memory_usage(&mut self) -> bool {
        if self.total_size.abs_diff(*self.last_reported_size_bytes)
            > REPORT_SIZE_EVERY_N_KB_CHANGE << 10
        {
            self.memory_usage_metrics.set(*self.total_size as _);
            *self.last_reported_size_bytes = *self.total_size;
            true
        } else {
            false
        }
    }
}

impl<'a, V: EstimateSize> Drop for MutGuard<'a, V> {
    fn drop(&mut self) {
        *self.total_size = self
            .total_size
            .saturating_sub(self.original_val_size)
            .saturating_add(self.inner.estimated_size());
        self.report_memory_usage();
    }
}

impl<'a, V: EstimateSize> Deref for MutGuard<'a, V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        self.inner
    }
}

impl<'a, V: EstimateSize> DerefMut for MutGuard<'a, V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
    }
}

pub struct UnsafeMutGuard<V: EstimateSize> {
    inner: NonNull<V>,
    // The size of the original value
    original_val_size: usize,
    // The total size of a collection
    total_size: NonNull<usize>,
    last_reported_size_bytes: NonNull<usize>,
    memory_usage_metrics: NonNull<IntGauge>,
}

impl<V: EstimateSize> UnsafeMutGuard<V> {
    pub fn new(
        inner: &mut V,
        total_size: &mut usize,
        last_reported_size_bytes: &mut usize,
        memory_usage_metrics: &mut IntGauge,
    ) -> Self {
        let original_val_size = inner.estimated_size();
        Self {
            inner: inner.into(),
            original_val_size,
            total_size: total_size.into(),
            last_reported_size_bytes: last_reported_size_bytes.into(),
            memory_usage_metrics: memory_usage_metrics.into(),
        }
    }

    /// # Safety
    ///
    /// 1. Only 1 `MutGuard` should be held for each value.
    /// 2. The returned `MutGuard` should not be moved to other threads.
    pub unsafe fn as_mut_guard<'a>(&mut self) -> MutGuard<'a, V> {
        MutGuard {
            inner: self.inner.as_mut(),
            original_val_size: self.original_val_size,
            total_size: self.total_size.as_mut(),
            last_reported_size_bytes: self.last_reported_size_bytes.as_mut(),
            memory_usage_metrics: self.memory_usage_metrics.as_mut(),
        }
    }
}
