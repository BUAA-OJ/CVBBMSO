use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use cvbbmso_domain::{BatchOutput, BatchWrite, DomainError, ExamService, HeartbeatUpdate};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::{Instant, sleep_until},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
pub(crate) struct WriterConfig {
    pub(crate) group_commit_delay: Duration,
    pub(crate) max_batch_size: usize,
    pub(crate) queue_capacity: usize,
    pub(crate) max_global_pending_writes: usize,
    pub(crate) max_global_pending_bytes: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            group_commit_delay: Duration::from_millis(50),
            max_batch_size: 256,
            queue_capacity: 1024,
            max_global_pending_writes: 8192,
            max_global_pending_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
struct BudgetCounts {
    writes: usize,
    bytes: usize,
}

pub(crate) struct WriteBudget {
    max_writes: usize,
    max_bytes: usize,
    counts: Mutex<BudgetCounts>,
    available: Notify,
}

pub(crate) struct WritePermit {
    budget: Arc<WriteBudget>,
    bytes: usize,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

impl WriteBudget {
    pub(crate) fn new(max_writes: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_writes: max_writes.max(1),
            max_bytes: max_bytes.max(1),
            counts: Mutex::new(BudgetCounts::default()),
            available: Notify::new(),
        })
    }

    pub(crate) fn try_acquire(self: &Arc<Self>, bytes: usize) -> Option<WritePermit> {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fits_count = counts.writes < self.max_writes;
        let fits_bytes = counts.bytes.saturating_add(bytes) <= self.max_bytes || counts.writes == 0;
        if fits_count && fits_bytes {
            counts.writes += 1;
            counts.bytes = counts.bytes.saturating_add(bytes);
            Some(WritePermit {
                budget: Arc::clone(self),
                bytes,
            })
        } else {
            None
        }
    }

    pub(crate) async fn acquire(self: &Arc<Self>, bytes: usize) -> WritePermit {
        loop {
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(permit) = self.try_acquire(bytes) {
                return permit;
            }
            notified.await;
        }
    }

    fn release(&self, bytes: usize) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counts.writes = counts.writes.saturating_sub(1);
        counts.bytes = counts.bytes.saturating_sub(bytes);
        drop(counts);
        self.available.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> (usize, usize) {
        let counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (counts.writes, counts.bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WritePriority {
    Normal,
    Immediate,
}

pub(crate) enum WriterRequest {
    Write {
        operation_id: Uuid,
        priority: WritePriority,
        op: BatchWrite,
    },
    Heartbeats {
        batch_id: Uuid,
        updates: Vec<HeartbeatUpdate>,
    },
    StorageProbe,
    Shutdown(oneshot::Sender<()>),
}

pub(crate) enum WriteCompletion {
    Batch {
        ops: Vec<(Uuid, Option<Result<BatchOutput, DomainError>>)>,
        heartbeat_batches: Vec<Uuid>,
        batch_error: Option<Arc<DomainError>>,
    },
}

#[derive(Clone)]
pub(crate) struct DatabaseWriterHandle {
    sender: mpsc::Sender<WriterRequest>,
    writer_ready: Arc<Notify>,
    budget: Arc<WriteBudget>,
    #[cfg(test)]
    batch_waiting: Arc<Notify>,
}

pub(crate) enum TryEnqueueError {
    Full(Box<WriterRequest>),
    Closed,
}

impl DatabaseWriterHandle {
    pub(crate) fn try_send(&self, request: WriterRequest) -> Result<(), TryEnqueueError> {
        self.sender.try_send(request).map_err(|err| match err {
            mpsc::error::TrySendError::Full(req) => TryEnqueueError::Full(Box::new(req)),
            mpsc::error::TrySendError::Closed(_) => TryEnqueueError::Closed,
        })
    }

    pub(crate) fn writer_ready(&self) -> &Arc<Notify> {
        &self.writer_ready
    }

    pub(crate) fn budget(&self) -> &Arc<WriteBudget> {
        &self.budget
    }

    #[cfg(test)]
    pub(crate) fn batch_waiting(&self) -> &Arc<Notify> {
        &self.batch_waiting
    }

    pub(crate) async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.sender.send(WriterRequest::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub(crate) struct DatabaseWriter {
    exam: Arc<ExamService>,
    config: WriterConfig,
    requests: mpsc::Receiver<WriterRequest>,
    completions: mpsc::UnboundedSender<WriteCompletion>,
    writer_ready: Arc<Notify>,
    #[cfg(test)]
    batch_waiting: Arc<Notify>,
}

impl DatabaseWriter {
    pub(crate) fn new(
        exam: Arc<ExamService>,
        config: WriterConfig,
    ) -> (
        DatabaseWriterHandle,
        mpsc::UnboundedReceiver<WriteCompletion>,
        Self,
    ) {
        let (sender, requests) = mpsc::channel(config.queue_capacity.max(1));
        let (completions, completion_rx) = mpsc::unbounded_channel();
        let writer_ready = Arc::new(Notify::new());
        let budget = WriteBudget::new(
            config.max_global_pending_writes,
            config.max_global_pending_bytes,
        );
        #[cfg(test)]
        let batch_waiting = Arc::new(Notify::new());
        let handle = DatabaseWriterHandle {
            sender,
            writer_ready: Arc::clone(&writer_ready),
            budget,
            #[cfg(test)]
            batch_waiting: Arc::clone(&batch_waiting),
        };
        let writer = Self {
            exam,
            config,
            requests,
            completions,
            writer_ready,
            #[cfg(test)]
            batch_waiting,
        };
        (handle, completion_rx, writer)
    }

    pub(crate) async fn run(mut self) {
        let max_batch = self.config.max_batch_size.max(1);
        loop {
            let Some(first) = self.requests.recv().await else {
                break;
            };
            let mut batch = PendingBatch::default();
            batch.push(first);
            self.collect_group_window(&mut batch, max_batch).await;
            batch.drain_ready(&mut self.requests, max_batch);
            self.writer_ready.notify_one();
            self.commit_batch(
                batch.op_ids,
                batch.ops,
                batch.heartbeat_batches,
                batch.heartbeats,
                batch.probe,
            )
            .await;
            if let Some(done) = batch.shutdown {
                let _ = self.exam.close().await;
                let _ = done.send(());
                break;
            }
        }
    }

    async fn collect_group_window(&mut self, batch: &mut PendingBatch, max_batch: usize) {
        if batch.shutdown.is_some()
            || batch.immediate
            || batch.ops.len() >= max_batch
            || self.config.group_commit_delay.is_zero()
        {
            return;
        }
        let deadline = Instant::now() + self.config.group_commit_delay;
        #[cfg(test)]
        self.batch_waiting.notify_one();
        loop {
            batch.drain_ready(&mut self.requests, max_batch);
            if batch.immediate || batch.shutdown.is_some() || batch.ops.len() >= max_batch {
                break;
            }
            tokio::select! {
                biased;
                next = self.requests.recv() => match next {
                    Some(req) => {
                        batch.push(req);
                        if batch.immediate || batch.shutdown.is_some() || batch.ops.len() >= max_batch {
                            break;
                        }
                    }
                    None => break,
                },
                () = sleep_until(deadline) => break,
            }
        }
    }

    async fn commit_batch(
        &self,
        op_ids: Vec<Uuid>,
        ops: Vec<BatchWrite>,
        heartbeat_batches: Vec<Uuid>,
        heartbeats: Vec<HeartbeatUpdate>,
        probe: bool,
    ) {
        if ops.is_empty() && heartbeats.is_empty() {
            if probe {
                let batch_error = self.exam.probe_storage().await.err().map(Arc::new);
                let _ = self.completions.send(WriteCompletion::Batch {
                    ops: Vec::new(),
                    heartbeat_batches: Vec::new(),
                    batch_error,
                });
            }
            return;
        }
        let op_count = ops.len();
        let hb_count = heartbeats.len();
        let started = Instant::now();
        match self.exam.execute_batch(ops, heartbeats).await {
            Ok(outcomes) => {
                tracing::debug!(
                    ops = op_count,
                    heartbeats = hb_count,
                    elapsed_ms = started.elapsed().as_millis(),
                    "database batch committed"
                );
                let paired = op_ids
                    .into_iter()
                    .zip(outcomes.into_iter().map(Some))
                    .collect();
                let _ = self.completions.send(WriteCompletion::Batch {
                    ops: paired,
                    heartbeat_batches,
                    batch_error: None,
                });
            }
            Err(error) => {
                tracing::warn!(
                    ops = op_count,
                    heartbeats = hb_count,
                    elapsed_ms = started.elapsed().as_millis(),
                    reason = %error.diagnostic(),
                    "database batch failed"
                );
                let paired = op_ids.into_iter().map(|id| (id, None)).collect();
                let _ = self.completions.send(WriteCompletion::Batch {
                    ops: paired,
                    heartbeat_batches,
                    batch_error: Some(Arc::new(error)),
                });
            }
        }
    }
}

#[derive(Default)]
struct PendingBatch {
    op_ids: Vec<Uuid>,
    ops: Vec<BatchWrite>,
    heartbeat_batches: Vec<Uuid>,
    heartbeats: Vec<HeartbeatUpdate>,
    probe: bool,
    immediate: bool,
    shutdown: Option<oneshot::Sender<()>>,
}

impl PendingBatch {
    fn push(&mut self, req: WriterRequest) {
        match req {
            WriterRequest::Write {
                operation_id,
                priority,
                op,
            } => {
                self.op_ids.push(operation_id);
                self.ops.push(op);
                if priority == WritePriority::Immediate {
                    self.immediate = true;
                }
            }
            WriterRequest::Heartbeats { batch_id, updates } => {
                self.heartbeat_batches.push(batch_id);
                self.heartbeats.extend(updates);
                self.immediate = true;
            }
            WriterRequest::StorageProbe => {
                self.probe = true;
                self.immediate = true;
            }
            WriterRequest::Shutdown(done) => {
                self.shutdown = Some(done);
                self.immediate = true;
            }
        }
    }

    fn drain_ready(&mut self, requests: &mut mpsc::Receiver<WriterRequest>, max_batch: usize) {
        while self.ops.len() < max_batch && self.shutdown.is_none() {
            match requests.try_recv() {
                Ok(req) => self.push(req),
                Err(_) => break,
            }
        }
    }
}
