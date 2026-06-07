//! Port of `DataBatcher` in `src/main/session.ts` — coalesce pty output and flush on
//! 16ms OR 200KB, whichever comes first. Unit-testable with an injected clock.
//!
//! Mirrors vercel/hyper's DataBatcher: collapses many tiny pty chunks into one IPC
//! message, cutting message count and GC pressure dramatically.
//!
//! ## Design vs. the TS original
//! The TS version owns a `setTimeout`. Here the timer is externalized so the core is
//! a pure state machine: `write()` returns any data flushed *synchronously* (the
//! size-triggered flush), and [`DataBatcher::deadline`] exposes the armed timer so an
//! async driver (the session task) can `sleep_until` it and call [`DataBatcher::flush`]
//! when it fires. Time is passed in as `now_ms` so tests need no real clock.
//!
//! Thresholds are measured in **UTF-16 code units** (not bytes), matching JS
//! `String.length` so flush boundaries line up 1:1 with the original.

/// Max duration to batch before a time-triggered flush (ms).
pub const BATCH_DURATION_MS: u64 = 16;
/// Max accumulated size (UTF-16 code units) before a size-triggered flush.
pub const BATCH_MAX_SIZE: usize = 200 * 1024;

/// Coalescing buffer for pty output. Not `Clone` — there is one per live session.
#[derive(Debug, Default)]
pub struct DataBatcher {
    data: String,
    /// Length of `data` in UTF-16 code units (kept incrementally so `write` is O(chunk)).
    len_u16: usize,
    /// `now_ms + BATCH_DURATION_MS` when a time flush is armed; `None` when idle.
    deadline: Option<u64>,
}

impl DataBatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk`. If adding it would reach [`BATCH_MAX_SIZE`], the *currently
    /// buffered* data is flushed first and returned (the new chunk then starts a
    /// fresh batch with a fresh timer) — exactly the TS order of operations. Returns
    /// `None` when nothing was flushed synchronously (the common case: the time
    /// flush will carry this data out later).
    pub fn write(&mut self, chunk: &str, now_ms: u64) -> Option<String> {
        let chunk_len = chunk.encode_utf16().count();
        let mut flushed = None;
        if self.len_u16 + chunk_len >= BATCH_MAX_SIZE {
            // Cancel the pending timer and flush what we have (may be empty → None).
            flushed = self.flush();
        }
        self.data.push_str(chunk);
        self.len_u16 += chunk_len;
        if self.deadline.is_none() {
            self.deadline = Some(now_ms + BATCH_DURATION_MS);
        }
        flushed
    }

    /// Flush now, clearing any armed timer. Returns the buffered data, or `None` when
    /// the buffer is empty (matching TS `flush()`'s early return — no empty emit).
    pub fn flush(&mut self) -> Option<String> {
        self.deadline = None;
        if self.data.is_empty() {
            return None;
        }
        self.len_u16 = 0;
        Some(std::mem::take(&mut self.data))
    }

    /// The armed time-flush deadline (`now_ms` basis), if any — for the async driver
    /// to `sleep_until`. `None` means no data is pending.
    pub fn deadline(&self) -> Option<u64> {
        self.deadline
    }

    /// Whether any data is currently buffered.
    pub fn has_pending(&self) -> bool {
        !self.data.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_writes_do_not_flush_synchronously_and_arm_a_timer() {
        let mut b = DataBatcher::new();
        assert_eq!(b.write("hello", 1000), None);
        assert_eq!(b.write(" world", 1002), None);
        // One timer, armed at the FIRST write (1000 + 16); later writes don't re-arm.
        assert_eq!(b.deadline(), Some(1016));
        assert!(b.has_pending());
    }

    #[test]
    fn time_flush_returns_the_coalesced_data_and_clears_state() {
        let mut b = DataBatcher::new();
        b.write("a", 0);
        b.write("b", 5);
        b.write("c", 10);
        // Driver fires at the deadline.
        assert_eq!(b.deadline(), Some(16));
        assert_eq!(b.flush(), Some("abc".to_string()));
        assert_eq!(b.deadline(), None);
        assert!(!b.has_pending());
        // A flush of an empty buffer emits nothing.
        assert_eq!(b.flush(), None);
    }

    #[test]
    fn size_overflow_flushes_existing_data_then_rebatches_the_new_chunk() {
        let mut b = DataBatcher::new();
        let half = "x".repeat(BATCH_MAX_SIZE - 10);
        assert_eq!(b.write(&half, 100), None);
        assert_eq!(b.deadline(), Some(116));

        // This write reaches the threshold (len + chunk >= MAX): the buffered `half`
        // is flushed out synchronously, then the new chunk starts a fresh batch.
        let next = "y".repeat(50);
        let flushed = b.write(&next, 200).expect("size-triggered flush");
        assert_eq!(flushed, half);
        // New chunk is buffered with a fresh timer from this write.
        assert_eq!(b.deadline(), Some(216));
        assert_eq!(b.flush(), Some(next));
    }

    #[test]
    fn a_single_oversized_chunk_does_not_flush_immediately() {
        // Faithful to TS: when the buffer is empty, `0 + huge >= MAX` flushes the
        // (empty) buffer — emitting nothing — then buffers the whole chunk for the
        // timer. The big chunk leaves on the next time/size flush, not synchronously.
        let mut b = DataBatcher::new();
        let huge = "z".repeat(BATCH_MAX_SIZE + 100);
        assert_eq!(b.write(&huge, 0), None);
        assert!(b.has_pending());
        assert_eq!(b.deadline(), Some(16));
        assert_eq!(b.flush(), Some(huge));
    }

    #[test]
    fn size_threshold_counts_utf16_code_units_not_bytes() {
        // Astral chars are 2 UTF-16 units each but 4 UTF-8 bytes — the JS `.length`
        // semantics we mirror count them as 2.
        let mut b = DataBatcher::new();
        let emoji = "😀"; // 1 char, 2 UTF-16 units, 4 bytes
        b.write(emoji, 0);
        // len_u16 should be 2, not 4.
        assert_eq!(b.len_u16, 2);
        assert_eq!(b.flush(), Some(emoji.to_string()));
    }

    #[test]
    fn idle_batcher_has_no_deadline() {
        let b = DataBatcher::new();
        assert_eq!(b.deadline(), None);
        assert!(!b.has_pending());
    }
}
