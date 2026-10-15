//! In-memory configuration and a single MQTT supervisor's change notification.
use crate::MqttSettings;
use core::cell::RefCell;
use core::future::Future;
use embassy_futures::select::{Either, select};
use embassy_sync::{
    blocking_mutex::{Mutex, raw::RawMutex},
    signal::Signal,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    WaitingForWifi,
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone)]
pub struct Snapshot {
    pub settings: MqttSettings,
    pub revision: u64,
    pub status: Status,
    pub last_error: Option<&'static str>,
}

pub struct Control<M: RawMutex> {
    state: Mutex<M, RefCell<Snapshot>>,
    changed: Signal<M, ()>,
}

impl<M: RawMutex> Control<M> {
    pub fn new(settings: MqttSettings) -> Self {
        Self {
            state: Mutex::new(RefCell::new(Snapshot {
                settings,
                revision: 0,
                status: Status::WaitingForWifi,
                last_error: None,
            })),
            changed: Signal::new(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock(|state| state.borrow().clone())
    }

    /// Call only after the new settings have been durably saved.
    pub fn apply(&self, settings: MqttSettings) -> bool {
        let changed = self.state.lock(|state| {
            let mut state = state.borrow_mut();
            if state.settings == settings {
                return false;
            }
            state.settings = settings;
            state.revision = state.revision.wrapping_add(1);
            state.status = Status::Connecting;
            state.last_error = None;
            true
        });
        if changed {
            self.changed.signal(());
        }
        changed
    }

    /// Ignore callbacks from a session superseded by a settings update.
    pub fn set_status(&self, revision: u64, status: Status, error: Option<&'static str>) {
        self.state.lock(|state| {
            let mut state = state.borrow_mut();
            if state.revision == revision {
                state.status = status;
                state.last_error = error;
            }
        });
    }

    /// Exactly one supervisor waits here. Rechecking the revision avoids lost or stale signals.
    pub async fn changed(&self, revision: u64) {
        loop {
            if self.state.lock(|state| state.borrow().revision != revision) {
                return;
            }
            self.changed.wait().await;
        }
    }

    /// Cancel a connection attempt, session, or retry delay when settings change.
    /// Resources needing asynchronous cleanup must be owned outside `future`.
    pub async fn until_changed<F: Future>(&self, revision: u64, future: F) -> Option<F::Output> {
        match select(self.changed(revision), future).await {
            Either::First(()) => None,
            Either::Second(result) => Some(result),
        }
    }
}
