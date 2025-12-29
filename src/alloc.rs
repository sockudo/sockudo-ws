//! Custom memory pool and arena allocator for zero-allocation hot paths
//!
//! Features:
//! - Slab allocator for fixed-size objects (frames, connections)
//! - Arena allocator for variable-size buffers
//! - Thread-local pools to avoid contention
//! - Cache-line aligned allocations

use std::alloc::{Layout, alloc, dealloc};
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::{MaybeUninit, align_of, size_of};
use std::ptr::NonNull;

use crate::CACHE_LINE_SIZE;

/// A slab allocator for fixed-size objects
///
/// Pre-allocates a pool of objects and recycles them without
/// calling the system allocator in the hot path.
#[repr(C, align(64))]
pub struct SlabPool<T, const N: usize> {
    /// Free list head (index into slots, or N if empty)
    free_head: usize,
    /// Number of allocated objects
    allocated: usize,
    /// Pre-allocated slots
    slots: [UnsafeCell<SlabSlot<T>>; N],
}

use std::mem::ManuallyDrop;

union SlabSlot<T> {
    /// When free: index of next free slot
    next_free: usize,
    /// When allocated: the actual value
    value: ManuallyDrop<MaybeUninit<T>>,
}

impl<T, const N: usize> SlabPool<T, N> {
    /// Create a new slab pool with all slots free
    pub fn new() -> Self {
        // Initialize free list: each slot points to the next
        let slots: [UnsafeCell<SlabSlot<T>>; N] = std::array::from_fn(|i| {
            UnsafeCell::new(SlabSlot {
                next_free: if i + 1 < N { i + 1 } else { N },
            })
        });

        Self {
            free_head: 0,
            allocated: 0,
            slots,
        }
    }

    /// Allocate an object from the pool
    /// Returns None if pool is exhausted
    #[inline]
    pub fn alloc(&mut self) -> Option<SlabHandle<T>> {
        if self.free_head >= N {
            return None;
        }

        let idx = self.free_head;
        let slot = unsafe { &mut *self.slots[idx].get() };

        // Update free list
        self.free_head = unsafe { slot.next_free };
        self.allocated += 1;

        Some(SlabHandle {
            index: idx,
            _marker: PhantomData,
        })
    }

    /// Initialize an allocated slot with a value
    #[inline]
    pub fn init(&mut self, handle: SlabHandle<T>, value: T) -> &mut T {
        let slot = unsafe { &mut *self.slots[handle.index].get() };
        unsafe {
            (*slot.value).write(value);
            (*slot.value).assume_init_mut()
        }
    }

    /// Get a reference to an allocated object
    #[inline]
    pub fn get(&self, handle: SlabHandle<T>) -> &T {
        let slot = unsafe { &*self.slots[handle.index].get() };
        unsafe { (*slot.value).assume_init_ref() }
    }

    /// Get a mutable reference to an allocated object
    #[inline]
    pub fn get_mut(&mut self, handle: SlabHandle<T>) -> &mut T {
        let slot = unsafe { &mut *self.slots[handle.index].get() };
        unsafe { (*slot.value).assume_init_mut() }
    }

    /// Free an object back to the pool
    #[inline]
    pub fn free(&mut self, handle: SlabHandle<T>) {
        let slot = unsafe { &mut *self.slots[handle.index].get() };

        // Drop the value
        unsafe {
            std::ptr::drop_in_place((*slot.value).assume_init_mut());
        }

        // Add to free list
        slot.next_free = self.free_head;
        self.free_head = handle.index;
        self.allocated -= 1;
    }

    /// Get number of allocated objects
    #[inline]
    pub fn allocated(&self) -> usize {
        self.allocated
    }

    /// Get number of free slots
    #[inline]
    pub fn available(&self) -> usize {
        N - self.allocated
    }
}

impl<T, const N: usize> Default for SlabPool<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to an allocated slab object
#[derive(Clone, Copy)]
pub struct SlabHandle<T> {
    index: usize,
    _marker: PhantomData<T>,
}

impl<T> SlabHandle<T> {
    /// Get the raw index
    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }
}

/// Arena allocator for variable-size allocations
///
/// Allocations are bump-pointer style and cannot be individually freed.
/// The entire arena is reset at once.
#[repr(C, align(64))]
pub struct Arena {
    /// Current allocation pointer
    ptr: NonNull<u8>,
    /// End of current chunk
    end: NonNull<u8>,
    /// Start of current chunk (for reset)
    start: NonNull<u8>,
    /// Size of current chunk
    chunk_size: usize,
    /// List of additional chunks
    chunks: Vec<NonNull<u8>>,
}

impl Arena {
    /// Create a new arena with the specified initial capacity
    pub fn new(capacity: usize) -> Self {
        let layout = Layout::from_size_align(capacity, CACHE_LINE_SIZE).unwrap();
        let ptr = unsafe { alloc(layout) };

        if ptr.is_null() {
            panic!("Arena allocation failed");
        }

        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        let end = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(capacity)) };

        Self {
            ptr,
            end,
            start: ptr,
            chunk_size: capacity,
            chunks: Vec::new(),
        }
    }

    /// Allocate bytes with the specified alignment
    #[inline]
    pub fn alloc_bytes(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        // Align the current pointer
        let aligned = (self.ptr.as_ptr() as usize + align - 1) & !(align - 1);
        let new_ptr = aligned + size;

        if new_ptr <= self.end.as_ptr() as usize {
            let result = unsafe { NonNull::new_unchecked(aligned as *mut u8) };
            self.ptr = unsafe { NonNull::new_unchecked(new_ptr as *mut u8) };
            Some(result)
        } else {
            // Need a new chunk
            self.grow(size, align)
        }
    }

    /// Allocate space for a value of type T
    #[inline]
    pub fn alloc<T>(&mut self) -> Option<NonNull<T>> {
        self.alloc_bytes(size_of::<T>(), align_of::<T>())
            .map(|ptr| ptr.cast())
    }

    /// Allocate and initialize a value
    #[inline]
    pub fn alloc_with<T>(&mut self, value: T) -> Option<&mut T> {
        let ptr = self.alloc::<T>()?;
        unsafe {
            std::ptr::write(ptr.as_ptr(), value);
            Some(&mut *ptr.as_ptr())
        }
    }

    /// Allocate a slice
    #[inline]
    pub fn alloc_slice<T>(&mut self, len: usize) -> Option<&mut [MaybeUninit<T>]> {
        let ptr = self.alloc_bytes(size_of::<T>() * len, align_of::<T>())?;
        Some(unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr() as *mut MaybeUninit<T>, len) })
    }

    /// Allocate and copy a slice
    #[inline]
    pub fn alloc_slice_copy<T: Copy>(&mut self, slice: &[T]) -> Option<&mut [T]> {
        let ptr = self.alloc_bytes(std::mem::size_of_val(slice), align_of::<T>())?;
        let dest = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr() as *mut T, slice.len()) };
        dest.copy_from_slice(slice);
        Some(dest)
    }

    /// Grow the arena with a new chunk
    fn grow(&mut self, size: usize, align: usize) -> Option<NonNull<u8>> {
        // Calculate new chunk size (at least double, or enough for allocation)
        let new_size = self.chunk_size.max(size + align).next_power_of_two();
        let layout = Layout::from_size_align(new_size, CACHE_LINE_SIZE).unwrap();

        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return None;
        }

        // Save old chunk
        self.chunks.push(self.start);

        // Set up new chunk
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        self.start = ptr;
        self.end = unsafe { NonNull::new_unchecked(ptr.as_ptr().add(new_size)) };
        self.chunk_size = new_size;

        // Allocate from new chunk
        let aligned = (ptr.as_ptr() as usize + align - 1) & !(align - 1);
        let new_ptr = aligned + size;
        self.ptr = unsafe { NonNull::new_unchecked(new_ptr as *mut u8) };

        Some(unsafe { NonNull::new_unchecked(aligned as *mut u8) })
    }

    /// Reset the arena, invalidating all allocations
    /// This is O(1) and doesn't free memory
    #[inline]
    pub fn reset(&mut self) {
        self.ptr = self.start;
    }

    /// Clear the arena and free all extra chunks
    pub fn clear(&mut self) {
        // Free extra chunks
        for chunk in self.chunks.drain(..) {
            unsafe {
                let layout = Layout::from_size_align(self.chunk_size, CACHE_LINE_SIZE).unwrap();
                dealloc(chunk.as_ptr(), layout);
            }
        }

        self.ptr = self.start;
    }

    /// Get total allocated bytes in current chunk
    #[inline]
    pub fn used(&self) -> usize {
        self.ptr.as_ptr() as usize - self.start.as_ptr() as usize
    }

    /// Get remaining capacity in current chunk
    #[inline]
    pub fn remaining(&self) -> usize {
        self.end.as_ptr() as usize - self.ptr.as_ptr() as usize
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // Free current chunk
        let layout = Layout::from_size_align(self.chunk_size, CACHE_LINE_SIZE).unwrap();
        unsafe {
            dealloc(self.start.as_ptr(), layout);
        }

        // Free all extra chunks
        for chunk in &self.chunks {
            unsafe {
                dealloc(chunk.as_ptr(), layout);
            }
        }
    }
}

// SAFETY: Arena is not Sync (single-threaded use only)
unsafe impl Send for Arena {}

/// Buffer pool for reusing I/O buffers
///
/// Maintains a free list of fixed-size buffers for zero-allocation I/O.
pub struct BufferPool {
    /// Free list of buffers
    free_list: Vec<Box<[u8]>>,
    /// Size of each buffer
    buffer_size: usize,
    /// Maximum number of cached buffers
    max_cached: usize,
}

impl BufferPool {
    /// Create a new buffer pool
    pub fn new(buffer_size: usize, max_cached: usize) -> Self {
        Self {
            free_list: Vec::with_capacity(max_cached),
            buffer_size,
            max_cached,
        }
    }

    /// Get a buffer from the pool
    #[inline]
    pub fn get(&mut self) -> Box<[u8]> {
        self.free_list
            .pop()
            .unwrap_or_else(|| vec![0u8; self.buffer_size].into_boxed_slice())
    }

    /// Return a buffer to the pool
    #[inline]
    pub fn put(&mut self, buffer: Box<[u8]>) {
        if self.free_list.len() < self.max_cached && buffer.len() == self.buffer_size {
            self.free_list.push(buffer);
        }
        // Otherwise buffer is dropped
    }

    /// Get number of cached buffers
    #[inline]
    pub fn cached(&self) -> usize {
        self.free_list.len()
    }
}

impl Default for BufferPool {
    fn default() -> Self {
        Self::new(4096, 64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slab_pool() {
        let mut pool: SlabPool<i32, 4> = SlabPool::new();

        let h1 = pool.alloc().unwrap();
        let h2 = pool.alloc().unwrap();

        pool.init(h1, 10);
        pool.init(h2, 20);

        assert_eq!(*pool.get(h1), 10);
        assert_eq!(*pool.get(h2), 20);

        pool.free(h1);

        let h3 = pool.alloc().unwrap();
        pool.init(h3, 30);
        assert_eq!(*pool.get(h3), 30);
    }

    #[test]
    fn test_arena() {
        let mut arena = Arena::new(1024);

        let x = arena.alloc_with(42i32).unwrap();
        assert_eq!(*x, 42);

        let slice = arena.alloc_slice_copy(&[1, 2, 3, 4, 5]).unwrap();
        assert_eq!(slice, &[1, 2, 3, 4, 5]);

        arena.reset();
        // After reset, memory is reused (but values are invalidated)
    }

    #[test]
    fn test_buffer_pool() {
        let mut pool = BufferPool::new(1024, 4);

        let buf1 = pool.get();
        assert_eq!(buf1.len(), 1024);

        pool.put(buf1);
        assert_eq!(pool.cached(), 1);

        let buf2 = pool.get();
        assert_eq!(pool.cached(), 0);
        assert_eq!(buf2.len(), 1024);
    }
}
