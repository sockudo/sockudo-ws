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
/// - Small writes accumulate in a buffer and go out in one `writev`
/// - Large payloads are queued as `Bytes` segments without copying
/// - Output order is always the order of the `write*` calls
///
/// Internally the pending output is an ordered list of frozen `Bytes`
/// segments followed by the open `buffer` that new small writes append to.
/// Queueing a large `Bytes` freezes the open buffer into a segment first, so a
/// header written just before it and a frame written just after it stay in
/// order around it.
#[repr(C, align(64))] // Cache-line aligned
pub struct CorkBuffer {
    /// Open tail buffer that small writes append to
    buffer: BytesMut,
    /// Soft limit for the open buffer; `write` reports when it is exceeded
    max_size: usize,
    /// Current cork state
    state: CorkState,
    /// Frozen segments queued before `buffer`, in output order
    segments: VecDeque<Bytes>,
    /// Total bytes in `segments`
    segment_bytes: usize,
}

/// Payloads at least this large are queued by reference instead of copied.
pub const ZERO_COPY_MIN: usize = 8 * 1024;

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
            segments: VecDeque::new(),
            segment_bytes: 0,
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
        !self.buffer.is_empty() || !self.segments.is_empty()
    }

    /// Whether writing requires traversing separately owned segments.
    #[inline]
    pub(crate) fn has_segments(&self) -> bool {
        !self.segments.is_empty()
    }

    /// Get total pending bytes
    #[inline]
    pub fn pending_bytes(&self) -> usize {
        self.buffer.len() + self.segment_bytes
    }

    /// Copy data into the buffer
    ///
    /// Returns `true` while the open buffer is within its configured size and
    /// `false` once it has grown past it, which is the caller's cue to flush.
    #[inline]
    pub fn write(&mut self, data: &[u8]) -> bool {
        self.buffer.extend_from_slice(data);
        self.buffer.len() <= self.max_size
    }

    /// Queue bytes for writing, without copying large payloads
    ///
    /// Small payloads are copied into the open buffer. Payloads of at least
    /// [`ZERO_COPY_MIN`] bytes are queued by reference behind everything
    /// written so far, so a frame header written just before stays in front.
    #[inline]
    pub fn write_bytes(&mut self, data: Bytes) {
        if data.len() < ZERO_COPY_MIN {
            self.buffer.extend_from_slice(&data);
            return;
        }
        self.push_segment(data);
    }

    /// Queue bytes by reference regardless of size, preserving order.
    #[inline]
    pub fn push_segment(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }
        if !self.buffer.is_empty() {
            let closed = self.buffer.split().freeze();
            self.segment_bytes += closed.len();
            self.segments.push_back(closed);
        }
        self.segment_bytes += data.len();
        self.segments.push_back(data);
    }

    /// Fill `out` with pending data as IoSlices, in output order, without
    /// allocating.
    ///
    /// Returns the number of slices written. At most `out.len()` slices are
    /// produced; anything left over is picked up by the next call after
    /// `consume`. Use this in hot paths instead of
    /// [`CorkBuffer::get_write_slices`].
    #[inline]
    pub fn fill_write_slices<'a>(&'a self, out: &mut [IoSlice<'a>]) -> usize {
        let mut n = 0;
        for segment in &self.segments {
            if n == out.len() {
                return n;
            }
            out[n] = IoSlice::new(segment);
            n += 1;
        }
        if n < out.len() && !self.buffer.is_empty() {
            out[n] = IoSlice::new(&self.buffer);
            n += 1;
        }
        n
    }

    /// Get data for writing as IoSlices (for vectored I/O)
    ///
    /// Allocates a `Vec`; prefer [`CorkBuffer::fill_write_slices`] in hot paths.
    pub fn get_write_slices(&self) -> Vec<IoSlice<'_>> {
        let mut slices = Vec::with_capacity(self.segments.len() + 1);
        for segment in &self.segments {
            slices.push(IoSlice::new(segment));
        }
        if !self.buffer.is_empty() {
            slices.push(IoSlice::new(&self.buffer));
        }
        slices
    }

    /// Consume bytes that have been written
    ///
    /// Call this after a successful write to remove sent data.
    pub fn consume(&mut self, mut n: usize) {
        while n > 0 {
            let Some(front) = self.segments.front_mut() else {
                break;
            };
            if n >= front.len() {
                n -= front.len();
                self.segment_bytes -= front.len();
                self.segments.pop_front();
            } else {
                front.advance(n);
                self.segment_bytes -= n;
                return;
            }
        }
        if n > 0 {
            let take = n.min(self.buffer.len());
            self.buffer.advance(take);
        }
    }

    /// Take the open buffer contents
    ///
    /// This is used when we need to move the data elsewhere. Queued segments
    /// are unaffected.
    pub fn take_buffer(&mut self) -> BytesMut {
        std::mem::replace(&mut self.buffer, BytesMut::with_capacity(self.max_size))
    }

    /// Clear all pending data
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.segments.clear();
        self.segment_bytes = 0;
    }

    /// Reserve additional capacity in the open buffer
    pub fn reserve(&mut self, additional: usize) {
        self.buffer.reserve(additional);
    }

    /// Get mutable access to the open buffer
    ///
    /// This is for direct frame encoding into the cork buffer. Data appended
    /// here is ordered after every segment queued so far.
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
        if let Some(front) = self.segments.front() {
            front
        } else {
            &self.buffer
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

        // Second write grows past the soft limit; data is still one slice
        assert!(!cork.write(b"world! this is a longer message"));

        let slices = cork.get_write_slices();
        assert_eq!(slices.len(), 1);
        assert_eq!(cork.pending_bytes(), 5 + 31);
    }

    #[test]
    fn test_cork_zero_copy_keeps_order() {
        let mut cork = CorkBuffer::with_capacity(1024);
        let big = Bytes::from(vec![0xAB; ZERO_COPY_MIN]);

        cork.write(b"header");
        cork.write_bytes(big.clone());
        cork.write(b"next frame");

        let slices = cork.get_write_slices();
        assert_eq!(slices.len(), 3);
        assert_eq!(&slices[0][..], b"header");
        assert_eq!(slices[1].len(), ZERO_COPY_MIN);
        assert_eq!(slices[1].as_ptr(), big.as_ptr());
        assert_eq!(&slices[2][..], b"next frame");
        assert_eq!(cork.pending_bytes(), 6 + ZERO_COPY_MIN + 10);

        // Partial consume inside the large segment, then the rest
        cork.consume(6 + 100);
        let slices = cork.get_write_slices();
        assert_eq!(slices.len(), 2);
        assert_eq!(slices[0].len(), ZERO_COPY_MIN - 100);
        cork.consume(ZERO_COPY_MIN - 100 + 10);
        assert!(!cork.has_data());
        assert_eq!(cork.pending_bytes(), 0);

        // Buffer is reusable after the segments are gone
        cork.write(b"again");
        assert_eq!(cork.get_write_slices().len(), 1);
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
