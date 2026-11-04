//! Receive buffer sizing shared by every WebSocket stream type.

use bytes::BytesMut;

/// Smallest accepted `Config::read_buffer_size`.
const MIN_RECEIVE_WINDOW: usize = 1024;

/// Spare capacity kept before each transport read, capped at 4KB.
const MAX_READ_SPARE: usize = 4096;

/// Tracks the receive window of one connection.
///
/// The window is the configured `read_buffer_size`: the initial allocation
/// and the amount reserved when spare capacity runs low. A large frame can
/// grow the buffer past it; once that input is consumed and reads are small
/// again, the buffer is replaced with a fresh window so an idle connection
/// does not keep the grown allocation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReceiveWindow {
    size: usize,
    // Reclaim once after buffered input reaches half a receive window.
    reclaim: bool,
    // The last transport read was smaller than the spare we keep, which is
    // what control frames and sparse traffic look like, not bulk input.
    last_read_small: bool,
}

impl ReceiveWindow {
    pub(crate) fn new(size: usize) -> Self {
        Self {
            size: size.max(MIN_RECEIVE_WINDOW),
            reclaim: false,
            last_read_small: false,
        }
    }

    pub(crate) fn allocate(&self) -> BytesMut {
        BytesMut::with_capacity(self.size)
    }

    fn min_spare(&self) -> usize {
        (self.size / 4).min(MAX_READ_SPARE)
    }

    /// Make room in `buf` before the next transport read.
    ///
    /// `grow_retained` reserves a full window when the reclaim fails because
    /// delivered messages still own the old allocation, so a later reserve
    /// does not copy a partial frame.
    pub(crate) fn prepare(&mut self, buf: &mut BytesMut, grow_retained: bool) {
        if buf.is_empty() {
            if self.last_read_small && buf.capacity() > self.size.saturating_mul(2) {
                // Messages still holding the grown allocation keep it alive
                // on their own; the stream only drops its handle.
                *buf = self.allocate();
                self.reclaim = false;
            } else if self.reclaim {
                self.reclaim = false;
                // Reuse an empty receive window when no delivered payload still owns it.
                if !buf.try_reclaim(self.size) && grow_retained {
                    buf.reserve(self.size);
                }
            }
        }

        if buf.capacity() - buf.len() < self.min_spare() {
            buf.reserve(self.size);
        }
    }

    /// Record a completed transport read of `read` bytes that left
    /// `buffered` bytes in the receive buffer.
    pub(crate) fn record_read(&mut self, buffered: usize, read: usize) {
        // Keep the hint across partial frames until the receive window is empty.
        if buffered >= self.size / 2 {
            self.reclaim = true;
        }
        self.last_read_small = read < self.min_spare();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_matches_previous_constants() {
        let window = ReceiveWindow::new(crate::RECV_BUFFER_SIZE);
        assert_eq!(window.allocate().capacity(), crate::RECV_BUFFER_SIZE);
        assert_eq!(window.min_spare(), 4096);
    }

    #[test]
    fn small_window_starts_small_and_grows_for_large_input() {
        let mut window = ReceiveWindow::new(4096);
        let mut buf = window.allocate();
        assert_eq!(buf.capacity(), 4096);

        // A burst fills the window; the next prepare grows it.
        buf.extend_from_slice(&[0; 4000]);
        window.record_read(buf.len(), 4000);
        window.prepare(&mut buf, true);
        assert!(buf.capacity() - buf.len() >= 1024);
    }

    #[test]
    fn grown_buffer_is_released_after_a_small_read() {
        let mut window = ReceiveWindow::new(4096);
        // A large frame grew the buffer and has been consumed.
        let mut buf = BytesMut::with_capacity(64 * 1024);
        buf.extend_from_slice(&[0; 6]);
        window.record_read(buf.len(), 6);
        buf.clear();

        window.prepare(&mut buf, true);
        assert_eq!(buf.capacity(), 4096);
    }

    #[test]
    fn grown_buffer_is_kept_while_reads_are_bulk() {
        let mut window = ReceiveWindow::new(4096);
        let mut buf = BytesMut::with_capacity(64 * 1024);
        buf.extend_from_slice(&[0; 16 * 1024]);
        window.record_read(buf.len(), 16 * 1024);
        buf.clear();

        window.prepare(&mut buf, true);
        assert!(buf.capacity() >= 64 * 1024);
    }

    #[test]
    fn tiny_windows_are_clamped() {
        assert_eq!(
            ReceiveWindow::new(10).allocate().capacity(),
            MIN_RECEIVE_WINDOW
        );
    }
}
