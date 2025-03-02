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

//! Hummock state store's SST builder, format and iterator

// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.
mod block;

use std::collections::BTreeSet;
use std::fmt::{Debug, Formatter};
use std::ops::{BitXor, Bound};

pub use block::*;
mod block_iterator;
pub use block_iterator::*;
mod bloom;
mod xor_filter;
pub use bloom::BloomFilterBuilder;
use xor_filter::XorFilterReader;
pub use xor_filter::{BlockedXor16FilterBuilder, Xor16FilterBuilder, Xor8FilterBuilder};
pub mod builder;
pub use builder::*;
pub mod writer;
use risingwave_common::catalog::TableId;
use risingwave_object_store::object::BlockLocation;
pub use writer::*;
mod forward_sstable_iterator;
pub mod multi_builder;
use bytes::{Buf, BufMut};
pub use forward_sstable_iterator::*;
mod backward_sstable_iterator;
pub use backward_sstable_iterator::*;
use risingwave_hummock_sdk::key::{
    FullKey, KeyPayloadType, PointRange, TableKey, UserKey, UserKeyRangeRef,
};
use risingwave_hummock_sdk::{HummockEpoch, HummockSstableObjectId};
#[cfg(test)]
use risingwave_pb::hummock::{KeyRange, SstableInfo};

mod delete_range_aggregator;
mod filter;
mod sstable_object_id_manager;
mod utils;

pub use delete_range_aggregator::{
    get_min_delete_range_epoch_from_sstable, CompactionDeleteRanges, CompactionDeleteRangesBuilder,
    SstableDeleteRangeIterator,
};
pub use filter::FilterBuilder;
pub use sstable_object_id_manager::*;
pub use utils::CompressionAlgorithm;
use utils::{get_length_prefixed_slice, put_length_prefixed_slice};
use xxhash_rust::{xxh32, xxh64};

use self::delete_range_aggregator::{apply_event, CompactionDeleteRangeEvent};
use self::utils::{xxhash64_checksum, xxhash64_verify};
use super::{HummockError, HummockResult};
use crate::hummock::CachePolicy;
use crate::store::ReadOptions;

const DEFAULT_META_BUFFER_CAPACITY: usize = 4096;
const MAGIC: u32 = 0x5785ab73;
const VERSION: u32 = 1;

#[derive(Clone, PartialEq, Eq, Debug)]
// delete keys located in [start_user_key, end_user_key)
pub struct DeleteRangeTombstone {
    pub start_user_key: PointRange<Vec<u8>>,
    pub end_user_key: PointRange<Vec<u8>>,
    pub sequence: HummockEpoch,
}

impl PartialOrd for DeleteRangeTombstone {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DeleteRangeTombstone {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.start_user_key
            .cmp(&other.start_user_key)
            .then_with(|| self.end_user_key.cmp(&other.end_user_key))
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl DeleteRangeTombstone {
    pub fn new(
        table_id: TableId,
        start_table_key: Vec<u8>,
        is_left_open: bool,
        end_table_key: Vec<u8>,
        is_right_close: bool,
        sequence: HummockEpoch,
    ) -> Self {
        Self {
            start_user_key: PointRange::from_user_key(
                UserKey::new(table_id, TableKey(start_table_key)),
                is_left_open,
            ),
            end_user_key: PointRange::from_user_key(
                UserKey::new(table_id, TableKey(end_table_key)),
                is_right_close,
            ),
            sequence,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(
        table_id: TableId,
        start_table_key: Vec<u8>,
        end_table_key: Vec<u8>,
        sequence: HummockEpoch,
    ) -> Self {
        Self::new(
            table_id,
            start_table_key,
            false,
            end_table_key,
            false,
            sequence,
        )
    }
}

/// Assume that watermark1 is 5, watermark2 is 7, watermark3 is 11, delete ranges
/// `{ [0, wmk1) in epoch1, [wmk1, wmk2) in epoch2, [wmk2, wmk3) in epoch3 }`
/// can be transformed into events below:
/// `{ <0, +epoch1> <wmk1, -epoch1> <wmk1, +epoch2> <wmk2, -epoch2> <wmk2, +epoch3> <wmk3,
/// -epoch3> }`
/// Then we can get monotonic events (they are in order by user key) as below:
/// `{ <0, epoch1>, <wmk1, epoch2>, <wmk2, epoch3>, <wmk3, +inf> }`
/// which means that delete range of [0, wmk1) is epoch1, delete range of [wmk1, wmk2) if epoch2,
/// etc. In this example, at the event key wmk1 (5), delete range changes from epoch1 to epoch2,
/// thus the `new epoch` is epoch2. epoch2 will be used from the event key wmk1 (5) and till the
/// next event key wmk2 (7) (not inclusive).
/// If there is no range deletes between current event key and next event key, `new_epoch` will be
/// `HummockEpoch::MAX`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonotonicDeleteEvent {
    pub event_key: PointRange<Vec<u8>>,
    pub new_epoch: HummockEpoch,
}

impl MonotonicDeleteEvent {
    #[cfg(test)]
    pub fn new(table_id: TableId, event_key: Vec<u8>, new_epoch: HummockEpoch) -> Self {
        Self {
            event_key: PointRange::from_user_key(
                UserKey::new(table_id, TableKey(event_key)),
                false,
            ),
            new_epoch,
        }
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        self.event_key.left_user_key.encode_length_prefixed(buf);
        buf.put_u8(if self.event_key.is_exclude_left_key {
            1
        } else {
            0
        });
        buf.put_u64_le(self.new_epoch);
    }

    pub fn decode(buf: &mut &[u8]) -> Self {
        let user_key = UserKey::decode_length_prefixed(buf);
        let exclude_left_key_flag = buf.get_u8();
        let is_exclude_left_key = match exclude_left_key_flag {
            0 => false,
            1 => true,
            _ => panic!("exclusive flag should be either 0 or 1"),
        };
        let new_epoch = buf.get_u64_le();
        Self {
            event_key: PointRange::from_user_key(user_key, is_exclude_left_key),
            new_epoch,
        }
    }

    #[inline]
    pub fn encoded_size(&self) -> usize {
        4 + self.event_key.left_user_key.encoded_len() + 1 + 8
    }
}

pub(crate) fn create_monotonic_events_from_compaction_delete_events(
    compaction_delete_range_events: Vec<CompactionDeleteRangeEvent>,
) -> Vec<MonotonicDeleteEvent> {
    let mut epochs = BTreeSet::new();
    let mut monotonic_tombstone_events = Vec::with_capacity(compaction_delete_range_events.len());
    for event in compaction_delete_range_events {
        apply_event(&mut epochs, &event);
        monotonic_tombstone_events.push(MonotonicDeleteEvent {
            event_key: event.0,
            new_epoch: epochs.first().map_or(HummockEpoch::MAX, |epoch| *epoch),
        });
    }
    monotonic_tombstone_events.dedup_by(|a, b| {
        a.event_key.left_user_key.table_id == b.event_key.left_user_key.table_id
            && a.new_epoch == b.new_epoch
    });
    monotonic_tombstone_events
}

#[cfg(any(test, feature = "test"))]
pub(crate) fn create_monotonic_events(
    mut delete_range_tombstones: Vec<DeleteRangeTombstone>,
) -> Vec<MonotonicDeleteEvent> {
    delete_range_tombstones.sort();
    let events = CompactionDeleteRangesBuilder::build_events(&delete_range_tombstones);
    create_monotonic_events_from_compaction_delete_events(events)
}

/// [`Sstable`] is a handle for accessing SST.
#[derive(Clone)]
pub struct Sstable {
    pub id: HummockSstableObjectId,
    pub meta: SstableMeta,
    pub filter_reader: XorFilterReader,
}

impl Debug for Sstable {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sstable")
            .field("id", &self.id)
            .field("meta", &self.meta)
            .finish()
    }
}

impl Sstable {
    pub fn new(id: HummockSstableObjectId, mut meta: SstableMeta) -> Self {
        let filter_data = std::mem::take(&mut meta.bloom_filter);
        let filter_reader = XorFilterReader::new(&filter_data, &meta.block_metas);

        Self {
            id,
            meta,
            filter_reader,
        }
    }

    #[inline(always)]
    pub fn has_bloom_filter(&self) -> bool {
        !self.filter_reader.is_empty()
    }

    pub fn calculate_block_info(&self, block_index: usize) -> (BlockLocation, usize) {
        let block_meta = &self.meta.block_metas[block_index];
        let block_loc = BlockLocation {
            offset: block_meta.offset as usize,
            size: block_meta.len as usize,
        };
        let uncompressed_capacity = block_meta.uncompressed_size as usize;
        (block_loc, uncompressed_capacity)
    }

    #[inline(always)]
    pub fn hash_for_bloom_filter_u32(dist_key: &[u8], table_id: u32) -> u32 {
        let dist_key_hash = xxh32::xxh32(dist_key, 0);
        // congyi adds this because he aims to dedup keys in different tables
        table_id.bitxor(dist_key_hash)
    }

    #[inline(always)]
    pub fn hash_for_bloom_filter(dist_key: &[u8], table_id: u32) -> u64 {
        let dist_key_hash = xxh64::xxh64(dist_key, 0);
        // congyi adds this because he aims to dedup keys in different tables
        (table_id as u64).bitxor(dist_key_hash)
    }

    #[inline(always)]
    pub fn may_match_hash(&self, user_key_range: &UserKeyRangeRef<'_>, hash: u64) -> bool {
        self.filter_reader.may_match(user_key_range, hash)
    }

    pub fn block_count(&self) -> usize {
        self.meta.block_metas.len()
    }

    #[inline]
    pub fn estimate_size(&self) -> usize {
        8 /* id */ + self.filter_reader.estimate_size() + self.meta.encoded_size()
    }

    #[cfg(test)]
    pub fn get_sstable_info(&self) -> SstableInfo {
        SstableInfo {
            object_id: self.id,
            sst_id: self.id,
            key_range: Some(KeyRange {
                left: self.meta.smallest_key.clone(),
                right: self.meta.largest_key.clone(),
                right_exclusive: false,
            }),
            file_size: self.meta.estimated_size as u64,
            meta_offset: self.meta.meta_offset,
            total_key_count: self.meta.key_count as u64,
            uncompressed_file_size: self.meta.estimated_size as u64,
            ..Default::default()
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BlockMeta {
    pub smallest_key: Vec<u8>,
    pub offset: u32,
    pub len: u32,
    pub uncompressed_size: u32,
}

impl BlockMeta {
    /// Format:
    ///
    /// ```plain
    /// | offset (4B) | len (4B) | smallest key len (4B) | smallest key |
    /// ```
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.put_u32_le(self.offset);
        buf.put_u32_le(self.len);
        buf.put_u32_le(self.uncompressed_size);
        put_length_prefixed_slice(buf, &self.smallest_key);
    }

    pub fn decode(buf: &mut &[u8]) -> Self {
        let offset = buf.get_u32_le();
        let len = buf.get_u32_le();
        let uncompressed_size = buf.get_u32_le();
        let smallest_key = get_length_prefixed_slice(buf);
        Self {
            smallest_key,
            offset,
            len,
            uncompressed_size,
        }
    }

    #[inline]
    pub fn encoded_size(&self) -> usize {
        16 /* offset + len + key len + uncompressed size */ + self.smallest_key.len()
    }

    pub fn table_id(&self) -> TableId {
        FullKey::decode(&self.smallest_key).user_key.table_id
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SstableMeta {
    pub block_metas: Vec<BlockMeta>,
    pub bloom_filter: Vec<u8>,
    pub estimated_size: u32,
    pub key_count: u32,
    pub smallest_key: Vec<u8>,
    pub largest_key: Vec<u8>,
    pub meta_offset: u64,
    /// Assume that watermark1 is 5, watermark2 is 7, watermark3 is 11, delete ranges
    /// `{ [0, wmk1) in epoch1, [wmk1, wmk2) in epoch2, [wmk2, wmk3) in epoch3 }`
    /// can be transformed into events below:
    /// `{ <0, +epoch1> <wmk1, -epoch1> <wmk1, +epoch2> <wmk2, -epoch2> <wmk2, +epoch3> <wmk3,
    /// -epoch3> }`
    /// Then we can get monotonic events (they are in order by user key) as below:
    /// `{ <0, epoch1>, <wmk1, epoch2>, <wmk2, epoch3>, <wmk3, +inf> }`
    /// which means that delete range of [0, wmk1) is epoch1, delete range of [wmk1, wmk2) if
    /// epoch2, etc. In this example, at the event key wmk1 (5), delete range changes from
    /// epoch1 to epoch2, thus the `new epoch` is epoch2. epoch2 will be used from the event
    /// key wmk1 (5) and till the next event key wmk2 (7) (not inclusive).
    /// If there is no range deletes between current event key and next event key, `new_epoch` will
    /// be `HummockEpoch::MAX`.
    pub monotonic_tombstone_events: Vec<MonotonicDeleteEvent>,
    /// Format version, for further compatibility.
    pub version: u32,
}

impl SstableMeta {
    /// Format:
    ///
    /// ```plain
    /// | N (4B) |
    /// | block meta 0 | ... | block meta N-1 |
    /// | bloom filter len (4B) | bloom filter |
    /// | estimated size (4B) | key count (4B) |
    /// | smallest key len (4B) | smallest key |
    /// | largest key len (4B) | largest key |
    /// | K (4B) |
    /// | tombstone-event 0 | ... | tombstone-event K-1 |
    /// | file offset of this meta block (8B) |
    /// | checksum (8B) | version (4B) | magic (4B) |
    /// ```
    pub fn encode_to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(DEFAULT_META_BUFFER_CAPACITY);
        self.encode_to(&mut buf);
        buf
    }

    pub fn encode_to(&self, buf: &mut Vec<u8>) {
        let start_offset = buf.len();
        buf.put_u32_le(utils::checked_into_u32(self.block_metas.len()));
        for block_meta in &self.block_metas {
            block_meta.encode(buf);
        }
        put_length_prefixed_slice(buf, &self.bloom_filter);
        buf.put_u32_le(self.estimated_size);
        buf.put_u32_le(self.key_count);
        put_length_prefixed_slice(buf, &self.smallest_key);
        put_length_prefixed_slice(buf, &self.largest_key);
        buf.put_u32_le(utils::checked_into_u32(
            self.monotonic_tombstone_events.len(),
        ));
        for monotonic_tombstone_event in &self.monotonic_tombstone_events {
            monotonic_tombstone_event.encode(buf);
        }
        buf.put_u64_le(self.meta_offset);
        let checksum = xxhash64_checksum(&buf[start_offset..]);
        buf.put_u64_le(checksum);
        buf.put_u32_le(VERSION);
        buf.put_u32_le(MAGIC);
    }

    pub fn decode(buf: &mut &[u8]) -> HummockResult<Self> {
        let mut cursor = buf.len();

        cursor -= 4;
        let magic = (&buf[cursor..]).get_u32_le();
        if magic != MAGIC {
            return Err(HummockError::magic_mismatch(MAGIC, magic));
        }

        cursor -= 4;
        let version = (&buf[cursor..cursor + 4]).get_u32_le();
        if version != VERSION {
            return Err(HummockError::invalid_format_version(version));
        }

        cursor -= 8;
        let checksum = (&buf[cursor..cursor + 8]).get_u64_le();
        let buf = &mut &buf[..cursor];
        xxhash64_verify(buf, checksum)?;

        let block_meta_count = buf.get_u32_le() as usize;
        let mut block_metas = Vec::with_capacity(block_meta_count);
        for _ in 0..block_meta_count {
            block_metas.push(BlockMeta::decode(buf));
        }
        let bloom_filter = get_length_prefixed_slice(buf);
        let estimated_size = buf.get_u32_le();
        let key_count = buf.get_u32_le();
        let smallest_key = get_length_prefixed_slice(buf);
        let largest_key = get_length_prefixed_slice(buf);
        let tomb_event_count = buf.get_u32_le() as usize;
        let mut monotonic_tombstone_events = Vec::with_capacity(tomb_event_count);
        for _ in 0..tomb_event_count {
            let monotonic_tombstone_event = MonotonicDeleteEvent::decode(buf);
            monotonic_tombstone_events.push(monotonic_tombstone_event);
        }
        let meta_offset = buf.get_u64_le();

        Ok(Self {
            block_metas,
            bloom_filter,
            estimated_size,
            key_count,
            smallest_key,
            largest_key,
            meta_offset,
            monotonic_tombstone_events,
            version,
        })
    }

    #[inline]
    pub fn encoded_size(&self) -> usize {
        4 // block meta count
            + self
            .block_metas
            .iter()
            .map(|block_meta| block_meta.encoded_size())
            .sum::<usize>()
            + 4 // monotonic tombstone events len
            + self
            .monotonic_tombstone_events
            .iter()
            .map(|event| event.encoded_size())
            .sum::<usize>()
            + 4 // bloom filter len
            + self.bloom_filter.len()
            + 4 // estimated size
            + 4 // key count
            + 4 // key len
            + self.smallest_key.len()
            + 4 // key len
            + self.largest_key.len()
            + 8 // footer
            + 8 // checksum
            + 4 // version
            + 4 // magic
    }
}

#[derive(Default)]
pub struct SstableIteratorReadOptions {
    pub cache_policy: CachePolicy,
    pub must_iterated_end_user_key: Option<Bound<UserKey<KeyPayloadType>>>,
}

impl SstableIteratorReadOptions {
    pub fn from_read_options(read_options: &ReadOptions) -> Self {
        Self {
            cache_policy: read_options.cache_policy,
            must_iterated_end_user_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    pub fn test_sstable_meta_enc_dec() {
        let meta = SstableMeta {
            block_metas: vec![
                BlockMeta {
                    smallest_key: b"0-smallest-key".to_vec(),
                    offset: 0,
                    len: 100,
                    uncompressed_size: 0,
                },
                BlockMeta {
                    smallest_key: b"5-some-key".to_vec(),
                    offset: 100,
                    len: 100,
                    uncompressed_size: 0,
                },
            ],
            bloom_filter: b"0123456789".to_vec(),
            estimated_size: 123,
            key_count: 123,
            smallest_key: b"0-smallest-key".to_vec(),
            largest_key: b"9-largest-key".to_vec(),
            meta_offset: 123,
            monotonic_tombstone_events: vec![],
            version: VERSION,
        };
        let sz = meta.encoded_size();
        let buf = meta.encode_to_bytes();
        assert_eq!(sz, buf.len());
        let decoded_meta = SstableMeta::decode(&mut &buf[..]).unwrap();
        assert_eq!(decoded_meta, meta);
    }
}
