//! Ordered delivery to the recorder's public bounded event receiver.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
};

use tokio::{
    runtime::Handle,
    sync::mpsc::{Sender, error::TrySendError},
};

use crate::RecorderEvent;

pub(super) struct Events {
    sender: Sender<RecorderEvent>,
    runtime: Handle,
    // Nonempty only during backpressure. There is one forwarder until it
    // delivers the last entry, so a later event cannot overtake an older one.
    pending: Mutex<VecDeque<RecorderEvent>>,
}

impl Events {
    pub(super) fn new(sender: Sender<RecorderEvent>) -> Arc<Self> {
        Arc::new(Self {
            sender,
            runtime: Handle::current(),
            pending: Mutex::default(),
        })
    }

    pub(super) fn send(self: &Arc<Self>, event: RecorderEvent) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if self.sender.is_closed() {
            return;
        }
        if !pending.is_empty() {
            pending.push_back(event);
            return;
        }
        if let Err(TrySendError::Full(event)) = self.sender.try_send(event) {
            pending.push_back(event);
            let outbox = Arc::clone(self);
            self.runtime.spawn(async move { outbox.forward().await });
        }
    }

    async fn forward(&self) {
        loop {
            let Ok(slot) = self.sender.reserve().await else {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
                return;
            };
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(event) = pending.pop_front() {
                slot.send(event);
            }
            if pending.is_empty() {
                return;
            }
        }
    }
}
