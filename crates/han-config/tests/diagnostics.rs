use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use han_config::diagnostics::{LOG_CAPACITY, Log, MESSAGE_CAPACITY, PAGE_SIZE, RateLimit};

#[test]
fn log_is_bounded_paginated_and_reports_overwritten_entries() {
    let log = Log::<NoopRawMutex>::new();
    assert!(log.page(0).entries.is_empty());
    assert!(!log.page(0).more);
    for i in 1..=LOG_CAPACITY + 5 {
        log.record(i as u64 * 1000, format_args!("event {i}"));
    }
    let first = log.page(0);
    assert!(first.lost);
    assert!(first.more);
    assert_eq!(first.entries.len(), PAGE_SIZE);
    assert_eq!(first.entries[0].sequence, 6);
    assert_eq!(first.entries[0].uptime_ms, 6000);
    let mut after = first.next_after;
    let mut sequences: Vec<_> = first.entries.iter().map(|e| e.sequence).collect();
    loop {
        let page = log.page(after);
        assert!(!page.lost);
        sequences.extend(page.entries.iter().map(|e| e.sequence));
        after = page.next_after;
        if !page.more {
            break;
        }
    }
    assert_eq!(sequences, (6..=69).collect::<Vec<_>>());
    let caught_up = log.page(after);
    assert!(caught_up.entries.is_empty());
    assert_eq!(caught_up.next_after, after);
    assert!(!caught_up.more);
    log.record(70_000, format_args!("UART FifoOverflowed"));
    assert_eq!(log.page(after).entries[0].message, "UART FifoOverflowed");
}

#[test]
fn stale_cursors_and_long_unicode_messages_are_safe() {
    let log = Log::<NoopRawMutex>::new();
    assert_eq!(log.page(u64::MAX).next_after, 0);
    log.record(0, format_args!("{} trailing", "å🦀".repeat(100)));
    let page = log.page(u64::MAX);
    assert_eq!(page.entries.len(), 1);
    let message = &page.entries[0].message;
    assert!(message.len() <= MESSAGE_CAPACITY);
    assert!(message.ends_with("..."));
    assert!(message.starts_with("å🦀"));
    assert_eq!(page.next_after, 1);
}

#[test]
fn error_logging_is_limited_to_once_per_second() {
    let mut limit = RateLimit::default();
    assert!(limit.admit(0));
    assert!(!limit.admit(0));
    assert!(!limit.admit(999));
    assert!(limit.admit(1000));
    assert!(!limit.admit(1999));
    assert!(limit.admit(2500));
}
