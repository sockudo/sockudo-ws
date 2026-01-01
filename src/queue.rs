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
///
/// # Safety Model
///
/// This queue uses UnsafeCell for interior mutability of the buffer slots.
/// The safety invariants are:
///
/// 1. Only the producer writes to slots (after claiming via head CAS)
/// 2. Only the consumer reads from slots (after claiming via tail CAS)
/// 3. Acquire/Release ordering ensures visibility:
///    - Producer: Release on head store publishes the write
///    - Consumer: Acquire on head load sees the published write
/// 4. The head/tail indices ensure a slot is never accessed by both sides simultaneously
#[repr(C, align(64))]
pub struct SpscQueue<T, const N: usize> {
    /// Write position (only modified by producer)
    head: CacheAlignedAtomic,
    /// Read position (only modified by consumer)
    tail: CacheAlignedAtomic,
    /// Ring buffer storage - UnsafeCell allows mutation through &self
    /// which is required for lock-free producer/consumer access
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

// SAFETY: SpscQueue is designed for single producer / single consumer.
// - T: Send is required because values of T are moved between threads
// - The producer and consumer operate on disjoint slots at any given time
// - Atomic operations with proper ordering ensure memory visibility
unsafe impl<T: Send, const N: usize> Send for SpscQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for SpscQueue<T, N> {}

impl<T, const N: usize> SpscQueue<T, N> {
    const ASSERT_POWER_OF_TWO: () = assert!(N.is_power_of_two(), "N must be power of 2");

    /// Create a new empty queue
    #[allow(path_statements)]
    pub const fn new() -> Self {
        Self::ASSERT_POWER_OF_TWO;
        Self {
            head: CacheAlignedAtomic::new(0),
            tail: CacheAlignedAtomic::new(0),
            // SAFETY: An array of MaybeUninit doesn't require initialization.
            // MaybeUninit<T> has no validity requirements for its contents.
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
    ///
    /// # Safety Contract
    /// This method must only be called from a single producer thread.
    #[inline]
    pub fn try_push(&self, item: T) -> std::result::Result<(), T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        // Check if queue is full
        if head.wrapping_sub(tail) >= N {
            return Err(item);
        }

        // SAFETY: We have exclusive access to this slot because:
        // 1. head > tail means the slot at head index is not being read by consumer
        // 2. We haven't published head+1 yet, so consumer can't access this slot
        // 3. Only one producer exists (SPSC invariant)
        // The UnsafeCell allows us to get a mutable pointer through &self.
        unsafe {
            let slot = self.buffer[head & Self::mask()].get();
            std::ptr::write((*slot).as_mut_ptr(), item);
        }

        // Publish the write with Release ordering
        // This ensures the write above is visible before the head update
        self.head.store(head.wrapping_add(1), Ordering::Release);

        Ok(())
    }

    /// Try to pop an item (consumer only)
    /// Returns None if queue is empty
    ///
    /// # Safety Contract
    /// This method must only be called from a single consumer thread.
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        // Check if queue is empty
        if tail == head {
            return None;
        }

        // SAFETY: We have exclusive access to this slot because:
        // 1. tail < head means the producer has finished writing to this slot
        // 2. Acquire ordering on head load synchronizes with producer's Release store
        // 3. Only one consumer exists (SPSC invariant)
        // 4. We haven't published tail+1 yet, so producer can't reuse this slot
        let item = unsafe {
            let slot = self.buffer[tail & Self::mask()].get();
            std::ptr::read((*slot).as_ptr())
        };

        // Publish the read with Release ordering
        // This ensures the read above completes before making the slot available
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
        // SAFETY: We have &mut self, so no concurrent access is possible.
        // We need to drop any items remaining in the queue.
        while self.try_pop().is_some() {}
    }
}

/// Multi-Producer Multi-Consumer lock-free queue
///
/// Uses a bounded ring buffer with compare-and-swap for thread safety.
/// Optimized for low contention scenarios.
///
/// # Safety Model
///
/// This queue uses sequence numbers per slot to coordinate access:
/// 1. Each slot has a sequence number that tracks its state
/// 2. Producers CAS the head to claim a slot, then write, then update sequence
/// 3. Consumers CAS the tail to claim a slot, then read, then update sequence
/// 4. The sequence number prevents ABA problems and ensures proper synchronization
#[repr(C, align(64))]
pub struct MpmcQueue<T, const N: usize> {
    /// Write position
    head: CacheAlignedAtomic,
    /// Read position
    tail: CacheAlignedAtomic,
    /// Sequence numbers for each slot (for ABA prevention and synchronization)
    sequences: [AtomicUsize; N],
    /// Ring buffer storage
    buffer: [UnsafeCell<MaybeUninit<T>>; N],
}

// SAFETY: MpmcQueue is designed for multiple producers and consumers.
// - T: Send is required because values cross thread boundaries
// - CAS operations on head/tail serialize slot access
// - Sequence numbers with acquire/release ordering ensure visibility
unsafe impl<T: Send, const N: usize> Send for MpmcQueue<T, N> {}
unsafe impl<T: Send, const N: usize> Sync for MpmcQueue<T, N> {}

impl<T, const N: usize> MpmcQueue<T, N> {
    const ASSERT_POWER_OF_TWO: () = assert!(N.is_power_of_two(), "N must be power of 2");

    /// Create a new empty queue
    #[allow(path_statements)]
    pub fn new() -> Self {
        Self::ASSERT_POWER_OF_TWO;

        // Initialize sequences to their index positions
        let sequences: [AtomicUsize; N] = std::array::from_fn(AtomicUsize::new);

        Self {
            head: CacheAlignedAtomic::new(0),
            tail: CacheAlignedAtomic::new(0),
            sequences,
            // SAFETY: MaybeUninit doesn't require initialization
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
                // Slot is available for writing - try to claim it
                match self.head.compare_exchange(
                    head,
                    head.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: We successfully claimed this slot via CAS.
                        // No other producer can write here until we update the sequence.
                        // No consumer can read here because sequence == head, not head+1.
                        unsafe {
                            let slot = self.buffer[idx].get();
                            std::ptr::write((*slot).as_mut_ptr(), item);
                        }
                        // Signal completion - consumers waiting for seq == head+1 will proceed
                        self.sequences[idx].store(head.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(h) => head = h, // Another producer won, retry with new head
                }
            } else if diff < 0 {
                // Queue is full (sequence hasn't caught up)
                return Err(item);
            } else {
                // diff > 0: Another producer is writing to this slot, reload head
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
                // Slot has data ready - try to claim it
                match self.tail.compare_exchange(
                    tail,
                    tail.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // SAFETY: We successfully claimed this slot via CAS.
                        // No other consumer can read here.
                        // The producer has finished (sequence == tail+1).
                        // Acquire on sequence load synchronized with producer's Release.
                        let item = unsafe {
                            let slot = self.buffer[idx].get();
                            std::ptr::read((*slot).as_ptr())
                        };
                        // Make slot available for producers in the next round
                        // sequence = tail + N means producers looking at head = tail + N can use it
                        self.sequences[idx].store(tail.wrapping_add(N), Ordering::Release);
                        return Some(item);
                    }
                    Err(t) => tail = t, // Another consumer won, retry with new tail
                }
            } else if diff < 0 {
                // Queue is empty (no data at this position yet)
                return None;
            } else {
                // diff > 0: Another consumer is reading from this slot, reload tail
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
        // SAFETY: We have &mut self, so no concurrent access.
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

    #[test]
    fn test_spsc_wraparound() {
        let queue: SpscQueue<i32, 4> = SpscQueue::new();

        // Fill and drain multiple times to test wraparound
        for round in 0..10 {
            for i in 0..4 {
                queue.try_push(round * 4 + i).unwrap();
            }
            for i in 0..4 {
                assert_eq!(queue.try_pop(), Some(round * 4 + i));
            }
        }
    }

    #[test]
    fn test_mpmc_wraparound() {
        let queue: MpmcQueue<i32, 4> = MpmcQueue::new();

        for round in 0..10 {
            for i in 0..4 {
                queue.try_push(round * 4 + i).unwrap();
            }
            for i in 0..4 {
                assert_eq!(queue.try_pop(), Some(round * 4 + i));
            }
        }
    }
}
