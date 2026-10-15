use crate::{FIELD_COUNT, Snapshot, Value};

pub const STALE_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug)]
pub struct Reading {
    pub value: Value,
    pub received_ms: u64,
    pub generation: u64,
}

impl Reading {
    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.received_ms) < STALE_MS
    }
}

/// One value per observed field, with independent freshness. Never queues history.
#[derive(Clone, Copy, Debug)]
pub struct Telemetry {
    pub readings: [Option<Reading>; FIELD_COUNT],
    pub valid_frames: u64,
    pub invalid_frames: u64,
    pub uart_errors: u64,
    pub last_valid_ms: Option<u64>,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl Telemetry {
    pub const fn new() -> Self {
        Self {
            readings: [None; FIELD_COUNT],
            valid_frames: 0,
            invalid_frames: 0,
            uart_errors: 0,
            last_valid_ms: None,
        }
    }

    pub fn update(&mut self, snapshot: Snapshot, now_ms: u64) {
        self.valid_frames = self.valid_frames.saturating_add(1);
        self.last_valid_ms = Some(now_ms);
        for (slot, value) in self.readings.iter_mut().zip(snapshot.readings) {
            if let Some(value) = value {
                *slot = Some(Reading {
                    value,
                    received_ms: now_ms,
                    generation: self.valid_frames,
                });
            }
        }
    }

    pub fn online(&self, now_ms: u64) -> bool {
        self.last_valid_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < STALE_MS)
    }

    pub fn seen_mask(&self) -> u32 {
        self.readings
            .iter()
            .enumerate()
            .fold(0, |mask, (i, r)| mask | (u32::from(r.is_some()) << i))
    }
}

pub struct Backoff {
    seconds: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub const fn new() -> Self {
        Self { seconds: 1 }
    }
    pub fn reset(&mut self) {
        self.seconds = 1;
    }
    pub fn next_seconds(&mut self) -> u64 {
        let result = self.seconds;
        self.seconds = (self.seconds * 2).min(60);
        result
    }
}
