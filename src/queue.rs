//! Lock-free SPSC and MPMC queues for the event loop
//!
//! These queues are designed for ultra-low latency with:
//! - Cache-line aligned head/tail pointers to prevent false sharing
//! - Power-of-2 sizing for fast modulo operations
//! - Memory barriers using acquire/release semantics
//! - No allocations in hot path

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::CACHE_LINE_SIZE;

/// Cache-line aligned atomic counter to prevent false sharing
#[repr(C, align(64))]
struct CacheAlignedAtomic {
    value: AtomicUsize,
    _padding: [u8; CACHE_LINE_SIZE - std::mem::size_of::<AtomicUsize>()],
}

impl CacheAlignedAtomic {
    const fn new(val: usize) -> Self {
        Self {
            value: AtomicUsize::new(val),
            _padding: [0; CACHE_LINE_SIZE - std::mem::size_of::<AtomicUsize>()],
        }
    }

    #[inline(always)]
    fn load(&self, order: Ordering) -> usize {
        self.value.load(order)
    }

    #[inline(always)]
    fn store(&self, val: usize, order: Ordering) {
        self.value.store(val, order);
    }

    #[inline(always)]
    fn compare_exchange(
        &self,
        current: usize,
        new: usize,
        success: Ordering,
        failure: Ordering,
    ) -> std::result::Result<usize, usize> {
        self.value.compare_exchange(current, new, success, failure)
    }
}

/// Single-Producer Single-Consumer lock-free ring buffer
///
/// Optimized for the common case of one event loop thread producing
/// and one worker thread consuming.
#[repr(C, align(64))]
pub struct SpscQueue<T, const N: usize> {
    /// Write position (only modified by producer)
    head: CacheAlignedAtomic,
    /// Read position (only modified by consumer)
    tail: CacheAlignedAtomic,
    /// Ring buffer storage
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

// SAFETY: SpscQueue is designed for single producer / single consumer
// The buffer is only accessed through atomic head/tail with proper ordering
unsafe impl<T: Send, const N: usize> Send for SpscQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for SpscQueue<T, N> {}

impl<T, const N: usize> SpscQueue<T, N> {
    const ASSERT_POWER_OF_TWO: () = assert!(N.is_power_of_two(), "N must be power of 2");

    /// Create a new empty queue
    pub const fn new() -> Self {
        let _ = Self::ASSERT_POWER_OF_TWO;
        Self {
            head: CacheAlignedAtomic::new(0),
            tail: CacheAlignedAtomic::new(0),
            // SAFETY: MaybeUninit doesn't require initialization
            buffer: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }

    /// Mask for fast modulo operation (N must be power of 2)
    #[inline(always)]
    const fn mask() -> usize {
        N - 1
    }

    /// Try to push an item (producer only)
    /// Returns Err(item) if queue is full
    #[inline]
    pub fn try_push(&self, item: T) -> std::result::Result<(), T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        // Check if queue is full
        if head.wrapping_sub(tail) >= N {
            return Err(item);
        }

        // Write item
        unsafe {
            let slot = &*self.buffer[head & Self::mask()].get();
            std::ptr::write(slot.as_ptr() as *mut T, item);
        }

        // Publish the write
        self.head.store(head.wrapping_add(1), Ordering::Release);

        Ok(())
    }

    /// Try to pop an item (consumer only)
    /// Returns None if queue is empty
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        // Check if queue is empty
        if tail == head {
            return None;
        }

        // Read item
        let item = unsafe {
            let slot = &*self.buffer[tail & Self::mask()].get();
            std::ptr::read(slot.as_ptr())
        };

        // Publish the read
        self.tail.store(tail.wrapping_add(1), Ordering::Release);

        Some(item)
    }

    /// Check if queue is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail == head
    }

    /// Get approximate number of items in queue
    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    /// Get queue capacity
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }
}

impl<T, const N: usize> Default for SpscQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for SpscQueue<T, N> {
    fn drop(&mut self) {
        // Drop any remaining items
        while self.try_pop().is_some() {}
    }
}

/// Multi-Producer Multi-Consumer lock-free queue
///
/// Uses a bounded ring buffer with compare-and-swap for thread safety.
/// Optimized for low contention scenarios.
#[repr(C, align(64))]
pub struct MpmcQueue<T, const N: usize> {
    /// Write position
    head: CacheAlignedAtomic,
    /// Read position
    tail: CacheAlignedAtomic,
    /// Sequence numbers for each slot (for ABA prevention)
    sequences: [AtomicUsize; N],
    /// Ring buffer storage
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

unsafe impl<T: Send, const N: usize> Send for MpmcQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for MpmcQueue<T, N> {}

impl<T, const N: usize> MpmcQueue<T, N> {
    const ASSERT_POWER_OF_TWO: () = assert!(N.is_power_of_two(), "N must be power of 2");

    /// Create a new empty queue
    pub fn new() -> Self {
        let _ = Self::ASSERT_POWER_OF_TWO;

        // Initialize sequences to their index positions
        let sequences: [AtomicUsize; N] = std::array::from_fn(|i| AtomicUsize::new(i));

        Self {
            head: CacheAlignedAtomic::new(0),
            tail: CacheAlignedAtomic::new(0),
            sequences,
            buffer: unsafe { MaybeUninit::uninit().assume_init() },
        }
    }

    #[inline(always)]
    const fn mask() -> usize {
        N - 1
    }

    /// Try to push an item
    /// Returns Err(item) if queue is full
    #[inline]
    pub fn try_push(&self, item: T) -> std::result::Result<(), T> {
        let mut head = self.head.load(Ordering::Relaxed);

        loop {
            let idx = head & Self::mask();
            let seq = self.sequences[idx].load(Ordering::Acquire);
            let diff = seq as isize - head as isize;

            if diff == 0 {
                // Slot is available for writing
                match self.head.compare_exchange(
                    head,
                    head.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Write the item
                        unsafe {
                            let slot = &*self.buffer[idx].get();
                            std::ptr::write(slot.as_ptr() as *mut T, item);
                        }
                        // Update sequence to signal completion
                        self.sequences[idx].store(head.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(h) => head = h, // Retry with new head
                }
            } else if diff < 0 {
                // Queue is full
                return Err(item);
            } else {
                // Another producer is writing, reload head
                head = self.head.load(Ordering::Relaxed);
            }
        }
    }

    /// Try to pop an item
    /// Returns None if queue is empty
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let mut tail = self.tail.load(Ordering::Relaxed);

        loop {
            let idx = tail & Self::mask();
            let seq = self.sequences[idx].load(Ordering::Acquire);
            let diff = seq as isize - (tail.wrapping_add(1)) as isize;

            if diff == 0 {
                // Slot has data ready
                match self.tail.compare_exchange(
                    tail,
                    tail.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // Read the item
                        let item = unsafe {
                            let slot = &*self.buffer[idx].get();
                            std::ptr::read(slot.as_ptr())
                        };
                        // Update sequence for next round
                        self.sequences[idx].store(tail.wrapping_add(N), Ordering::Release);
                        return Some(item);
                    }
                    Err(t) => tail = t, // Retry with new tail
                }
            } else if diff < 0 {
                // Queue is empty
                return None;
            } else {
                // Another consumer is reading, reload tail
                tail = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    /// Check if queue is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        tail == head
    }

    /// Get approximate length
    #[inline]
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }
}

impl<T, const N: usize> Default for MpmcQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for MpmcQueue<T, N> {
    fn drop(&mut self) {
        while self.try_pop().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spsc_basic() {
        let queue: SpscQueue<i32, 16> = SpscQueue::new();

        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);

        queue.try_push(1).unwrap();
        queue.try_push(2).unwrap();
        queue.try_push(3).unwrap();

        assert_eq!(queue.len(), 3);
        assert!(!queue.is_empty());

        assert_eq!(queue.try_pop(), Some(1));
        assert_eq!(queue.try_pop(), Some(2));
        assert_eq!(queue.try_pop(), Some(3));
        assert_eq!(queue.try_pop(), None);
    }

    #[test]
    fn test_spsc_full() {
        let queue: SpscQueue<i32, 4> = SpscQueue::new();

        queue.try_push(1).unwrap();
        queue.try_push(2).unwrap();
        queue.try_push(3).unwrap();
        queue.try_push(4).unwrap();

        assert!(queue.try_push(5).is_err());
    }

    #[test]
    fn test_mpmc_basic() {
        let queue: MpmcQueue<i32, 16> = MpmcQueue::new();

        queue.try_push(1).unwrap();
        queue.try_push(2).unwrap();

        assert_eq!(queue.try_pop(), Some(1));
        assert_eq!(queue.try_pop(), Some(2));
        assert_eq!(queue.try_pop(), None);
    }
}
