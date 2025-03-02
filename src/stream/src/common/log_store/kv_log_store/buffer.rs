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

use std::collections::VecDeque;
use std::ops::DerefMut;
use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};
use risingwave_common::array::StreamChunk;
use risingwave_common::buffer::Bitmap;
use tokio::sync::{oneshot, Notify};

use crate::common::log_store::kv_log_store::{ReaderTruncationOffsetType, SeqIdType};
use crate::common::log_store::LogStoreResult;

#[derive(Clone)]
pub(crate) enum LogStoreBufferItem {
    StreamChunk {
        chunk: StreamChunk,
        start_seq_id: SeqIdType,
        end_seq_id: SeqIdType,
        flushed: bool,
    },

    Flushed {
        vnode_bitmap: Bitmap,
        start_seq_id: SeqIdType,
        end_seq_id: SeqIdType,
    },

    Barrier {
        is_checkpoint: bool,
        next_epoch: u64,
    },

    UpdateVnodes(Arc<Bitmap>),
}

struct LogStoreBufferInner {
    /// Items not read by log reader. Newer item at the front
    unconsumed_queue: VecDeque<(u64, LogStoreBufferItem)>,
    /// Items already read by log reader by not truncated. Newer item at the front
    consumed_queue: VecDeque<(u64, LogStoreBufferItem)>,
    stream_chunk_count: usize,
    consumed_stream_chunk_count: usize,
    max_stream_chunk_count: usize,

    updated_truncation: Option<ReaderTruncationOffsetType>,
}

impl LogStoreBufferInner {
    fn can_add_stream_chunk(&self) -> bool {
        self.stream_chunk_count < self.max_stream_chunk_count
    }

    fn add_item(&mut self, epoch: u64, item: LogStoreBufferItem) {
        if let LogStoreBufferItem::StreamChunk { .. } = item {
            unreachable!("StreamChunk should call try_add_item")
        }
        assert!(
            self.try_add_item(epoch, item).is_none(),
            "call on item other than StreamChunk should always succeed"
        );
    }

    /// Try adding a `LogStoreBufferItem` to the buffer. If the stream chunk count exceeds the
    /// maximum count, it will return the original stream chunk if we are adding a stream chunk.
    fn try_add_item(&mut self, epoch: u64, item: LogStoreBufferItem) -> Option<StreamChunk> {
        match item {
            LogStoreBufferItem::StreamChunk {
                chunk,
                start_seq_id,
                end_seq_id,
                flushed,
            } => {
                if !self.can_add_stream_chunk() {
                    Some(chunk)
                } else {
                    self.stream_chunk_count += 1;
                    self.unconsumed_queue.push_front((
                        epoch,
                        LogStoreBufferItem::StreamChunk {
                            chunk,
                            start_seq_id,
                            end_seq_id,
                            flushed,
                        },
                    ));
                    None
                }
            }
            item => {
                self.unconsumed_queue.push_front((epoch, item));
                None
            }
        }
    }

    fn pop_item(&mut self) -> Option<(u64, LogStoreBufferItem)> {
        if let Some((epoch, item)) = self.unconsumed_queue.pop_back() {
            if let LogStoreBufferItem::StreamChunk { .. } = &item {
                self.consumed_stream_chunk_count += 1;
            }
            self.consumed_queue.push_front((epoch, item.clone()));
            Some((epoch, item))
        } else {
            None
        }
    }

    fn add_flushed(
        &mut self,
        epoch: u64,
        start_seq_id: SeqIdType,
        end_seq_id: SeqIdType,
        new_vnode_bitmap: Bitmap,
    ) {
        if let Some((
            item_epoch,
            LogStoreBufferItem::Flushed {
                end_seq_id: prev_end_seq_id,
                vnode_bitmap,
                ..
            },
        )) = self.unconsumed_queue.front_mut()
        {
            assert!(
                *prev_end_seq_id < start_seq_id,
                "prev end_seq_id {} should be smaller than current start_seq_id {}",
                end_seq_id,
                start_seq_id
            );
            assert_eq!(
                epoch, *item_epoch,
                "epoch of newly added flushed item must be the same as the last flushed item"
            );
            *prev_end_seq_id = end_seq_id;
            *vnode_bitmap |= new_vnode_bitmap;
        } else {
            self.add_item(
                epoch,
                LogStoreBufferItem::Flushed {
                    start_seq_id,
                    end_seq_id,
                    vnode_bitmap: new_vnode_bitmap,
                },
            );
        }
    }
}

struct SharedMutex<T>(Arc<Mutex<T>>);

impl<T> Clone for SharedMutex<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> SharedMutex<T> {
    fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }

    fn inner(&self) -> MutexGuard<'_, T> {
        if let Some(guard) = self.0.try_lock() {
            guard
        } else {
            info!("fall back to lock");
            self.0.lock()
        }
    }
}

pub(crate) struct LogStoreBufferSender {
    init_epoch_tx: Option<oneshot::Sender<u64>>,
    buffer: SharedMutex<LogStoreBufferInner>,
    update_notify: Arc<Notify>,
}

impl LogStoreBufferSender {
    pub(crate) fn init(&mut self, epoch: u64) {
        if let Err(e) = self
            .init_epoch_tx
            .take()
            .expect("should be Some in first init")
            .send(epoch)
        {
            error!("unable to send init epoch: {}", e);
        }
    }

    pub(crate) fn add_flushed(
        &self,
        epoch: u64,
        start_seq_id: SeqIdType,
        end_seq_id: SeqIdType,
        vnode_bitmap: Bitmap,
    ) {
        self.buffer
            .inner()
            .add_flushed(epoch, start_seq_id, end_seq_id, vnode_bitmap);
        self.update_notify.notify_waiters();
    }

    pub(crate) fn try_add_stream_chunk(
        &self,
        epoch: u64,
        chunk: StreamChunk,
        start_seq_id: SeqIdType,
        end_seq_id: SeqIdType,
    ) -> Option<StreamChunk> {
        let ret = self.buffer.inner().try_add_item(
            epoch,
            LogStoreBufferItem::StreamChunk {
                chunk,
                start_seq_id,
                end_seq_id,
                flushed: false,
            },
        );
        if ret.is_none() {
            // notify when successfully add
            self.update_notify.notify_waiters();
        }
        ret
    }

    pub(crate) fn barrier(&self, epoch: u64, is_checkpoint: bool, next_epoch: u64) {
        self.buffer.inner().add_item(
            epoch,
            LogStoreBufferItem::Barrier {
                is_checkpoint,
                next_epoch,
            },
        );
        self.update_notify.notify_waiters();
    }

    pub(crate) fn update_vnode(&self, epoch: u64, vnode: Arc<Bitmap>) {
        self.buffer
            .inner()
            .add_item(epoch, LogStoreBufferItem::UpdateVnodes(vnode));
        self.update_notify.notify_waiters();
    }

    pub(crate) fn pop_truncation(&self) -> Option<ReaderTruncationOffsetType> {
        self.buffer.inner().updated_truncation.take()
    }

    pub(crate) fn flush_all_unflushed(
        &mut self,
        mut flush_fn: impl FnMut(&StreamChunk, u64, SeqIdType, SeqIdType) -> LogStoreResult<()>,
    ) -> LogStoreResult<()> {
        let mut inner_guard = self.buffer.inner();
        let inner = inner_guard.deref_mut();
        for (epoch, item) in inner
            .unconsumed_queue
            .iter_mut()
            .chain(inner.consumed_queue.iter_mut())
        {
            if let LogStoreBufferItem::StreamChunk {
                chunk,
                start_seq_id,
                end_seq_id,
                flushed,
            } = item
            {
                if *flushed {
                    // Since we iterate from new data to old data, when we meet a flushed data, the
                    // rest should all be flushed.
                    break;
                }
                flush_fn(chunk, *epoch, *start_seq_id, *end_seq_id)?;
                *flushed = true;
            }
        }
        Ok(())
    }
}

pub(crate) struct LogStoreBufferReceiver {
    init_epoch_rx: Option<oneshot::Receiver<u64>>,
    buffer: SharedMutex<LogStoreBufferInner>,
    update_notify: Arc<Notify>,
}

impl LogStoreBufferReceiver {
    pub(crate) async fn init(&mut self) -> u64 {
        self.init_epoch_rx
            .take()
            .expect("should be Some in first init")
            .await
            .expect("should get the first epoch")
    }

    pub(crate) async fn next_item(&self) -> (u64, LogStoreBufferItem) {
        let notified = self.update_notify.notified();
        if let Some(item) = {
            let opt = self.buffer.inner().pop_item();
            opt
        } {
            item
        } else {
            notified.await;
            self.buffer
                .inner()
                .pop_item()
                .expect("should get the item because notified")
        }
    }

    pub(crate) fn truncate(&mut self) {
        let mut inner = self.buffer.inner();
        if let Some((epoch, item)) = inner.consumed_queue.front() {
            match item {
                LogStoreBufferItem::Barrier { .. } => {
                    inner.updated_truncation = Some(*epoch);
                }
                _ => {
                    unreachable!("should only call truncate right after getting a barrier");
                }
            }
            inner.consumed_queue.clear();
            inner.stream_chunk_count -= inner.consumed_stream_chunk_count;
            inner.consumed_stream_chunk_count = 0;
        }
    }
}

pub(crate) fn new_log_store_buffer(
    max_stream_chunk_count: usize,
) -> (LogStoreBufferSender, LogStoreBufferReceiver) {
    let buffer = SharedMutex::new(LogStoreBufferInner {
        unconsumed_queue: VecDeque::new(),
        consumed_queue: VecDeque::new(),
        stream_chunk_count: 0,
        consumed_stream_chunk_count: 0,
        max_stream_chunk_count,
        updated_truncation: None,
    });
    let update_notify = Arc::new(Notify::new());
    let (init_epoch_tx, init_epoch_rx) = oneshot::channel();
    let tx = LogStoreBufferSender {
        init_epoch_tx: Some(init_epoch_tx),
        buffer: buffer.clone(),
        update_notify: update_notify.clone(),
    };

    let rx = LogStoreBufferReceiver {
        init_epoch_rx: Some(init_epoch_rx),
        buffer,
        update_notify,
    };

    (tx, rx)
}
