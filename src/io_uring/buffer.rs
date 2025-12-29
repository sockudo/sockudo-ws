//! Registered buffer pool for io_uring zero-copy I/O
//!
//! This module provides a buffer pool for io_uring operations that can be
//! registered with the kernel for zero-copy I/O.

use std::collections::VecDeque;

/// A pool of registered buffers for io_uring operations
///
/// Registered buffers allow the kernel to avoid copying data between
/// user and kernel space, improving performance for high-throughput
/// applications.
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::io_uring::RegisteredBufferPool;
///
/// let mut pool = RegisteredBufferPool::new(16, 64 * 1024);
///
/// // Acquire a buffer for I/O
/// if let Some(buf) = pool.acquire() {
///     // Use buffer for read/write operations
///     // ...
///
///     // Return buffer to pool when done
///     pool.release(buf);
/// }
/// ```
pub struct RegisteredBufferPool {
    /// Free buffers available for use
    free_list: VecDeque<RegisteredBuffer>,
    /// Size of each buffer
    buffer_size: usize,
    /// Maximum number of buffers in the pool
    max_buffers: usize,
    /// Total number of allocated buffers
    allocated_count: usize,
}

impl RegisteredBufferPool {
    /// Create a new buffer pool
    ///
    /// # Arguments
    ///
    /// * `count` - Number of buffers to pre-allocate
    /// * `buffer_size` - Size of each buffer in bytes
    pub fn new(count: usize, buffer_size: usize) -> Self {
        let mut free_list = VecDeque::with_capacity(count);

        for index in 0..count {
            free_list.push_back(RegisteredBuffer::new(index, buffer_size));
        }

        Self {
            free_list,
            buffer_size,
            max_buffers: count,
            allocated_count: count,
        }
    }

    /// Acquire a buffer from the pool
    ///
    /// Returns `None` if no buffers are available.
    pub fn acquire(&mut self) -> Option<RegisteredBuffer> {
        self.free_list.pop_front()
    }

    /// Try to acquire a buffer, allocating a new one if the pool is empty
    ///
    /// This will allocate a new buffer if the pool is empty and we haven't
    /// reached the maximum buffer count.
    pub fn acquire_or_alloc(&mut self) -> Option<RegisteredBuffer> {
        if let Some(buf) = self.free_list.pop_front() {
            return Some(buf);
        }

        // Try to allocate a new buffer
        if self.allocated_count < self.max_buffers * 2 {
            let buf = RegisteredBuffer::new(self.allocated_count, self.buffer_size);
            self.allocated_count += 1;
            return Some(buf);
        }

        None
    }

    /// Release a buffer back to the pool
    pub fn release(&mut self, mut buf: RegisteredBuffer) {
        buf.clear();
        self.free_list.push_back(buf);
    }

    /// Get the number of available buffers
    pub fn available(&self) -> usize {
        self.free_list.len()
    }

    /// Get the buffer size
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Get the maximum number of buffers
    pub fn max_buffers(&self) -> usize {
        self.max_buffers
    }

    /// Check if the pool is empty
    pub fn is_empty(&self) -> bool {
        self.free_list.is_empty()
    }
}

/// A buffer from the registered pool
///
/// This buffer can be used for io_uring read and write operations.
/// When done, return it to the pool using `RegisteredBufferPool::release`.
pub struct RegisteredBuffer {
    /// Index in the pool (for identification)
    index: usize,
    /// The actual buffer data
    data: Vec<u8>,
    /// Current length of valid data
    len: usize,
    /// Read position for consuming data
    pos: usize,
}

impl RegisteredBuffer {
    /// Create a new registered buffer
    fn new(index: usize, size: usize) -> Self {
        Self {
            index,
            data: vec![0u8; size],
            len: 0,
            pos: 0,
        }
    }

    /// Get the buffer index
    pub fn index(&self) -> usize {
        self.index
    }

    /// Get the buffer capacity
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Get the current length of valid data
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.len == 0 || self.pos >= self.len
    }

    /// Get the remaining bytes to read
    pub fn remaining(&self) -> usize {
        if self.pos >= self.len {
            0
        } else {
            self.len - self.pos
        }
    }

    /// Get a slice of the unread data
    pub fn as_slice(&self) -> &[u8] {
        &self.data[self.pos..self.len]
    }

    /// Get a mutable slice for writing
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data[..]
    }

    /// Get the spare capacity for writing
    pub fn spare_capacity(&self) -> usize {
        self.data.len() - self.len
    }

    /// Get a mutable slice of spare capacity
    pub fn spare_capacity_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.len..]
    }

    /// Set the length of valid data (after a read operation)
    pub fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.data.len());
        self.len = len;
    }

    /// Add to the length (after appending data)
    pub fn add_len(&mut self, additional: usize) {
        self.len = std::cmp::min(self.len + additional, self.data.len());
    }

    /// Advance the read position
    pub fn advance(&mut self, count: usize) {
        self.pos = std::cmp::min(self.pos + count, self.len);
    }

    /// Clear the buffer (reset length and position)
    pub fn clear(&mut self) {
        self.len = 0;
        self.pos = 0;
    }

    /// Compact the buffer (move unread data to the beginning)
    pub fn compact(&mut self) {
        if self.pos > 0 && self.pos < self.len {
            self.data.copy_within(self.pos..self.len, 0);
            self.len -= self.pos;
            self.pos = 0;
        } else if self.pos >= self.len {
            self.clear();
        }
    }

    /// Write data to the buffer
    ///
    /// Returns the number of bytes written.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let to_write = std::cmp::min(data.len(), self.spare_capacity());
        self.data[self.len..self.len + to_write].copy_from_slice(&data[..to_write]);
        self.len += to_write;
        to_write
    }

    /// Read data from the buffer
    ///
    /// Returns the number of bytes read.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let to_read = std::cmp::min(buf.len(), self.remaining());
        buf[..to_read].copy_from_slice(&self.data[self.pos..self.pos + to_read]);
        self.pos += to_read;
        to_read
    }

    /// Get the underlying Vec for io_uring operations
    ///
    /// # Safety
    ///
    /// The caller must ensure proper synchronization with io_uring operations.
    pub fn into_inner(self) -> Vec<u8> {
        self.data
    }

    /// Create from an existing Vec (for io_uring completion)
    ///
    /// # Arguments
    ///
    /// * `index` - Buffer pool index
    /// * `data` - The buffer data
    /// * `len` - Number of valid bytes in the buffer
    pub fn from_vec(index: usize, data: Vec<u8>, len: usize) -> Self {
        Self {
            index,
            data,
            len,
            pos: 0,
        }
    }
}

impl std::fmt::Debug for RegisteredBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredBuffer")
            .field("index", &self.index)
            .field("capacity", &self.data.len())
            .field("len", &self.len)
            .field("pos", &self.pos)
            .field("remaining", &self.remaining())
            .finish()
    }
}

impl std::fmt::Debug for RegisteredBufferPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredBufferPool")
            .field("available", &self.free_list.len())
            .field("buffer_size", &self.buffer_size)
            .field("max_buffers", &self.max_buffers)
            .field("allocated_count", &self.allocated_count)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_pool() {
        let mut pool = RegisteredBufferPool::new(4, 1024);

        assert_eq!(pool.available(), 4);
        assert_eq!(pool.buffer_size(), 1024);

        let buf = pool.acquire().unwrap();
        assert_eq!(pool.available(), 3);

        pool.release(buf);
        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn test_registered_buffer() {
        let mut buf = RegisteredBuffer::new(0, 1024);

        assert_eq!(buf.capacity(), 1024);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());

        let written = buf.write(b"hello");
        assert_eq!(written, 5);
        assert_eq!(buf.len(), 5);
        assert!(!buf.is_empty());

        let mut out = [0u8; 10];
        let read = buf.read(&mut out);
        assert_eq!(read, 5);
        assert_eq!(&out[..5], b"hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_buffer_compact() {
        let mut buf = RegisteredBuffer::new(0, 1024);

        buf.write(b"hello world");
        buf.advance(6); // Skip "hello "

        assert_eq!(buf.as_slice(), b"world");

        buf.compact();

        assert_eq!(buf.pos, 0);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.as_slice(), b"world");
    }
}
