//! Bounded RAM log. Formatting and HTTP serialization happen outside the lock.
use core::{
    cell::RefCell,
    fmt::{self, Write},
};
use embassy_sync::blocking_mutex::{Mutex, raw::RawMutex};
use heapless::{Deque, String, Vec};
use serde::Serialize;

pub const LOG_CAPACITY: usize = 64;
pub const MESSAGE_CAPACITY: usize = 160;
pub const PAGE_SIZE: usize = 4;

#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub sequence: u64,
    pub uptime_ms: u64,
    pub message: String<MESSAGE_CAPACITY>,
}

#[derive(Serialize)]
pub struct Page {
    pub entries: Vec<Entry, PAGE_SIZE>,
    pub next_after: u64,
    pub more: bool,
    pub lost: bool,
}

struct Buffer {
    entries: Deque<Entry, LOG_CAPACITY>,
    sequence: u64,
}

pub struct Log<M: RawMutex> {
    buffer: Mutex<M, RefCell<Buffer>>,
}

impl<M: RawMutex> Default for Log<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: RawMutex> Log<M> {
    pub const fn new() -> Self {
        Self {
            buffer: Mutex::new(RefCell::new(Buffer {
                entries: Deque::new(),
                sequence: 0,
            })),
        }
    }

    /// Callers must only supply diagnostic text, never settings or raw requests.
    pub fn record(&self, uptime_ms: u64, args: fmt::Arguments<'_>) {
        let mut message = Message {
            text: String::new(),
            truncated: false,
        };
        let _ = message.write_fmt(args);
        self.buffer.lock(|buffer| {
            let mut buffer = buffer.borrow_mut();
            if buffer.entries.is_full() {
                buffer.entries.pop_front();
            }
            buffer.sequence += 1;
            let sequence = buffer.sequence;
            let _ = buffer.entries.push_back(Entry {
                sequence,
                uptime_ms,
                message: message.text,
            });
        });
    }

    pub fn page(&self, after: u64) -> Page {
        let (oldest, latest) = self.buffer.lock(|buffer| {
            let buffer = buffer.borrow();
            (
                buffer.entries.front().map_or(1, |e| e.sequence),
                buffer.sequence,
            )
        });
        // A cursor from before a reboot must not prevent new entries appearing.
        let after = if after > latest { 0 } else { after };
        let first = after.saturating_add(1).max(oldest);
        let end = latest.min(first.saturating_add(PAGE_SIZE as u64 - 1));
        let mut page = Page {
            entries: Vec::new(),
            next_after: after,
            more: false,
            lost: after.saturating_add(1) < oldest,
        };
        for sequence in first..=end {
            // Copy at most one short entry with interrupts disabled. Never hold
            // the lock across formatting, serialization, or network writes.
            let entry = self.buffer.lock(|buffer| {
                let buffer = buffer.borrow();
                let oldest = buffer.entries.front()?.sequence;
                let index = usize::try_from(sequence.checked_sub(oldest)?).ok()?;
                buffer.entries.get(index).cloned()
            });
            if let Some(entry) = entry {
                let _ = page.entries.push(entry);
            } else {
                page.lost = true;
            }
            page.next_after = sequence;
        }
        page.more = page.next_after < latest;
        page
    }
}

struct Message {
    text: String<MESSAGE_CAPACITY>,
    truncated: bool,
}

impl Write for Message {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if !self.truncated {
            for character in text.chars() {
                if self.text.len() + character.len_utf8() > MESSAGE_CAPACITY - 3 {
                    let _ = self.text.push_str("...");
                    self.truncated = true;
                    break;
                }
                let _ = self.text.push(character);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub struct UartErrors {
    pub overflow: u64,
    pub framing: u64,
    pub glitch: u64,
    pub parity: u64,
    pub other: u64,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub boot_id: u32,
    pub uptime_ms: u64,
    pub valid_frames: u64,
    pub invalid_frames: u64,
    pub uart_errors: u64,
    pub uart_error_types: UartErrors,
    pub last_valid_age_ms: Option<u64>,
    pub last_interval_ms: Option<u64>,
    pub max_interval_ms: Option<u64>,
    pub log: Page,
}

/// Limit noisy events without preventing their counters from being updated.
#[derive(Default)]
pub struct RateLimit(Option<u64>);

impl RateLimit {
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if self
            .0
            .is_some_and(|last| now_ms.saturating_sub(last) < 1000)
        {
            return false;
        }
        self.0 = Some(now_ms);
        true
    }
}
