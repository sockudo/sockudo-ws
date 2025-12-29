//! Write batching (corking) mechanism
//!
//! This module implements a corking mechanism similar to uWebSockets,
//! which batches small writes into larger chunks to minimize syscalls.
//!
//! Key features:
//! - 16KB cork buffer (configurable)
//! - Automatic flushing when buffer is full
//! - Zero-copy for large messages (bypass cork)
//! - Support for vectored I/O (writev)

use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;
use std::io::IoSlice;

use crate::CORK_BUFFER_SIZE;

/// Cork buffer state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorkState {
    /// Not corked, writes go directly to socket
    Uncorked,
    /// Corked, writes accumulate in buffer
    Corked,
}

/// Write buffer for batching small writes
///
/// This implements the "corking" optimization from uWebSockets:
/// - Small writes accumulate in a buffer
/// - When buffer is full or uncorked, all data is flushed at once
/// - Large writes bypass the cork for efficiency
#[repr(C, align(64))] // Cache-line aligned
pub struct CorkBuffer {
    /// Main cork buffer for small writes
    buffer: BytesMut,
    /// Maximum cork buffer size
    max_size: usize,
    /// Current cork state
    state: CorkState,
    /// Overflow queue for large messages and backpressure
    overflow: VecDeque<Bytes>,
    /// Total bytes in overflow queue
    overflow_bytes: usize,
}

impl CorkBuffer {
    /// Create a new cork buffer with default size
    pub fn new() -> Self {
        Self::with_capacity(CORK_BUFFER_SIZE)
    }

    /// Create a new cork buffer with specified capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: BytesMut::with_capacity(capacity),
            max_size: capacity,
            state: CorkState::Uncorked,
            overflow: VecDeque::new(),
            overflow_bytes: 0,
        }
    }

    /// Enter corked state
    ///
    /// While corked, writes accumulate in the buffer instead of
    /// being sent immediately. Call `uncork()` to flush.
    #[inline]
    pub fn cork(&mut self) {
        self.state = CorkState::Corked;
    }

    /// Exit corked state
    ///
    /// Returns true if there's data to flush.
    #[inline]
    pub fn uncork(&mut self) -> bool {
        self.state = CorkState::Uncorked;
        self.has_data()
    }

    /// Check if currently corked
    #[inline]
    pub fn is_corked(&self) -> bool {
        self.state == CorkState::Corked
    }

    /// Check if there's any pending data
    #[inline]
    pub fn has_data(&self) -> bool {
        !self.buffer.is_empty() || !self.overflow.is_empty()
    }

    /// Get total pending bytes
    #[inline]
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len() + self.overflow_bytes
    }

    /// Write data to the buffer
    ///
    /// Returns:
    /// - `Ok(true)` if data fits in cork buffer
    /// - `Ok(false)` if data was queued (needs flush)
    #[inline]
    pub fn write(&mut self, data: &[u8]) -> bool {
        let len = data.len();

        // Fast path: data fits in cork buffer
        if self.buffer.len() + len <= self.max_size {
            self.buffer.extend_from_slice(data);
            return true;
        }

        // Large message or buffer full: queue for later
        self.overflow.push_back(Bytes::copy_from_slice(data));
        self.overflow_bytes += len;
        false
    }

    /// Write bytes (zero-copy for Bytes)
    #[inline]
    pub fn write_bytes(&mut self, data: Bytes) {
        let len = data.len();

        // Small data: try to fit in cork buffer
        if len <= 128 && self.buffer.len() + len <= self.max_size {
            self.buffer.extend_from_slice(&data);
            return;
        }

        // Large or doesn't fit: queue directly
        self.overflow.push_back(data);
        self.overflow_bytes += len;
    }

    /// Get data for writing as IoSlices (for vectored I/O)
    ///
    /// This is optimized for `writev` syscall to minimize copies.
    pub fn get_write_slices(&self) -> Vec<IoSlice<'_>> {
        let mut slices = Vec::with_capacity(1 + self.overflow.len());

        if !self.buffer.is_empty() {
            slices.push(IoSlice::new(&self.buffer));
        }

        for chunk in &self.overflow {
            slices.push(IoSlice::new(chunk));
        }

        slices
    }

    /// Consume bytes that have been written
    ///
    /// Call this after a successful write to remove sent data.
    pub fn consume(&mut self, mut n: usize) {
        // First consume from main buffer
        if !self.buffer.is_empty() {
            let to_consume = n.min(self.buffer.len());
            self.buffer.advance(to_consume);
            n -= to_consume;
        }

        // Then consume from overflow
        while n > 0 && !self.overflow.is_empty() {
            let front_len = self.overflow.front().unwrap().len();
            if n >= front_len {
                self.overflow.pop_front();
                self.overflow_bytes -= front_len;
                n -= front_len;
            } else {
                // Partial consume - this is rare
                let front = self.overflow.pop_front().unwrap();
                self.overflow_bytes -= front.len();
                self.overflow.push_front(front.slice(n..));
                self.overflow_bytes += self.overflow.front().unwrap().len();
                break;
            }
        }
    }

    /// Take the cork buffer contents
    ///
    /// This is used when we need to move the data elsewhere.
    pub fn take_buffer(&mut self) -> BytesMut {
        std::mem::replace(&mut self.buffer, BytesMut::with_capacity(self.max_size))
    }

    /// Clear all pending data
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.overflow.clear();
        self.overflow_bytes = 0;
    }

    /// Reserve additional capacity in the main buffer
    pub fn reserve(&mut self, additional: usize) {
        self.buffer.reserve(additional);
    }

    /// Get mutable access to the internal buffer
    ///
    /// This is for direct frame encoding into the cork buffer.
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }
}

impl Default for CorkBuffer {
    fn default() -> Self {
        Self::new()
    }
}

use bytes::Buf;

impl Buf for CorkBuffer {
    fn remaining(&self) -> usize {
        self.pending_bytes()
    }

    fn chunk(&self) -> &[u8] {
        if !self.buffer.is_empty() {
            &self.buffer
        } else if let Some(front) = self.overflow.front() {
            front
        } else {
            &[]
        }
    }

    fn advance(&mut self, cnt: usize) {
        self.consume(cnt);
    }
}

/// Batch writer for zero-syscall message encoding
///
/// This accumulates multiple WebSocket frames before flushing,
/// using the cork buffer underneath.
pub struct BatchWriter<'a> {
    cork: &'a mut CorkBuffer,
    was_corked: bool,
}

impl<'a> BatchWriter<'a> {
    /// Create a new batch writer
    pub fn new(cork: &'a mut CorkBuffer) -> Self {
        let was_corked = cork.is_corked();
        cork.cork();
        Self { cork, was_corked }
    }

    /// Write data to the batch
    #[inline]
    pub fn write(&mut self, data: &[u8]) {
        self.cork.write(data);
    }

    /// Write bytes to the batch
    #[inline]
    pub fn write_bytes(&mut self, data: Bytes) {
        self.cork.write_bytes(data);
    }

    /// Get the underlying buffer for direct encoding
    #[inline]
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        self.cork.buffer_mut()
    }
}

impl<'a> Drop for BatchWriter<'a> {
    fn drop(&mut self) {
        if !self.was_corked {
            self.cork.uncork();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cork_basic() {
        let mut cork = CorkBuffer::with_capacity(1024);

        assert!(!cork.has_data());
        assert!(cork.write(b"hello"));
        assert!(cork.has_data());
        assert_eq!(cork.pending_bytes(), 5);

        cork.consume(5);
        assert!(!cork.has_data());
    }

    #[test]
    fn test_cork_overflow() {
        let mut cork = CorkBuffer::with_capacity(16);

        // First write fits
        assert!(cork.write(b"hello"));

        // Second write overflows
        assert!(!cork.write(b"world! this is a longer message"));

        let slices = cork.get_write_slices();
        assert_eq!(slices.len(), 2);
    }

    #[test]
    fn test_cork_state() {
        let mut cork = CorkBuffer::new();

        assert!(!cork.is_corked());
        cork.cork();
        assert!(cork.is_corked());
        cork.uncork();
        assert!(!cork.is_corked());
    }

    #[test]
    fn test_batch_writer() {
        let mut cork = CorkBuffer::with_capacity(1024);

        {
            let mut batch = BatchWriter::new(&mut cork);
            batch.write(b"message1");
            batch.write(b"message2");
            // Note: can't check cork.is_corked() here due to borrow rules
        }

        // Should be uncorked after BatchWriter drops
        assert!(!cork.is_corked());
        assert!(cork.has_data());
        assert_eq!(cork.pending_bytes(), 16);
    }

    #[test]
    fn test_consume_partial() {
        let mut cork = CorkBuffer::with_capacity(16);

        cork.write_bytes(Bytes::from_static(b"hello"));
        cork.write_bytes(Bytes::from_static(b"world"));

        cork.consume(7); // "hello" + "wo"
        assert_eq!(cork.pending_bytes(), 3); // "rld"
    }
}
