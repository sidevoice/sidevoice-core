//! Which devices have calls open, so revoking a device can end them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::server::AppState;

/// One closing signal per open call, by device id.
#[derive(Default)]
pub(in crate::server) struct CallRegistry {
    senders: Mutex<HashMap<String, Vec<watch::Sender<bool>>>>,
}

impl CallRegistry {
    pub(in crate::server) fn open(&self) -> usize {
        self.senders
            .lock()
            .expect("calls lock")
            .values()
            .map(|senders| senders.iter().filter(|tx| tx.receiver_count() > 0).count())
            .sum()
    }

    pub(in crate::server) fn close(&self, id: &str) {
        if let Some(senders) = self.senders.lock().expect("calls lock").remove(id) {
            for sender in senders {
                let _ = sender.send(true);
            }
        }
    }

    fn register(&self, id: &str) -> watch::Receiver<bool> {
        let (sender, receiver) = watch::channel(false);
        self.senders
            .lock()
            .expect("calls lock")
            .entry(id.to_owned())
            .or_default()
            .push(sender);
        receiver
    }

    /// Drops the device's signals whose call has gone.
    fn release(&self, id: &str) {
        let mut calls = self.senders.lock().expect("calls lock");
        if let Some(senders) = calls.get_mut(id) {
            senders.retain(|sender| sender.receiver_count() > 0);
            if senders.is_empty() {
                calls.remove(id);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn signals(&self, id: &str) -> Option<usize> {
        self.senders
            .lock()
            .unwrap()
            .get(id)
            .map(|senders| senders.len())
    }
}

/// A call's place in the registry, released when the call ends.
pub(super) struct CallRegistration {
    state: Arc<AppState>,
    id: String,
    receiver: Option<watch::Receiver<bool>>,
}

impl CallRegistration {
    pub(super) fn new(state: Arc<AppState>, id: String) -> Self {
        let receiver = state.calls.register(&id);
        Self {
            state,
            id,
            receiver: Some(receiver),
        }
    }

    /// Resolves when the device is revoked or replaced.
    pub(super) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.receiver
            .as_mut()
            .expect("call receiver present")
            .changed()
            .await
    }
}

impl Drop for CallRegistration {
    fn drop(&mut self) {
        drop(self.receiver.take());
        self.state.calls.release(&self.id);
    }
}
