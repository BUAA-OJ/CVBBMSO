//! Ordered delivery to the recorder's public bounded event receiver.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::{
    runtime::Handle,
    sync::{
        Notify,
        mpsc::{Sender, error::TrySendError},
    },
};

use crate::RecorderEvent;

pub(super) struct Events {
    sender: Sender<RecorderEvent>,
    runtime: Handle,
    outbox: Mutex<Outbox>,
    delivered: Notify,
}

#[derive(Default)]
struct Outbox {
    // One forwarder owns the backlog until the last entry is delivered.
    pending: VecDeque<RecorderEvent>,
    // Includes queued events, so an interrupted worker's durable segments can
    // be replayed without duplicating ones already in this delivery path.
    last_segment: u32,
}

impl Events {
    pub(super) fn new(sender: Sender<RecorderEvent>) -> Arc<Self> {
        Arc::new(Self {
            sender,
            runtime: Handle::current(),
            outbox: Mutex::default(),
            delivered: Notify::new(),
        })
    }

    pub(super) fn send(self: &Arc<Self>, event: RecorderEvent) {
        let mut outbox = self.lock();
        if self.sender.is_closed() {
            return;
        }
        let segment = match &event {
            RecorderEvent::SegmentFinalized(meta) => Some(meta.index),
            RecorderEvent::Fault(_) => None,
        };
        let launch_forwarder = if outbox.pending.is_empty() {
            match self.sender.try_send(event) {
                Ok(()) => false,
                Err(TrySendError::Closed(_)) => return,
                Err(TrySendError::Full(event)) => {
                    outbox.pending.push_back(event);
                    true
                }
            }
        } else {
            outbox.pending.push_back(event);
            false
        };
        if let Some(index) = segment {
            outbox.last_segment = index;
        }
        if launch_forwarder {
            let events = Arc::clone(self);
            self.runtime.spawn(async move { events.forward().await });
        }
    }

    pub(super) fn last_segment(&self) -> u32 {
        self.lock().last_segment
    }

    /// Waits for the forwarder to publish the buffered events to the receiver.
    /// There is one waiter because finishing requires exclusive recorder access.
    pub(super) async fn wait_for_delivery(&self) {
        while !self.sender.is_closed() && !self.lock().pending.is_empty() {
            // notify_one retains a permit if the queue empties before we wait.
            self.delivered.notified().await;
        }
    }

    async fn forward(&self) {
        loop {
            let Ok(slot) = self.sender.reserve().await else {
                self.lock().pending.clear();
                self.delivered.notify_one();
                return;
            };
            let mut outbox = self.lock();
            if let Some(event) = outbox.pending.pop_front() {
                slot.send(event);
            }
            if outbox.pending.is_empty() {
                self.delivered.notify_one();
                return;
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
