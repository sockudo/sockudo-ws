//! Receive storage: borrow complete transport chunks, join only incomplete frames.

use std::ops::Deref;

use bytes::{Buf, Bytes, BytesMut};

use crate::Result;
use crate::frame::{Frame, FrameInput, FrameParser};

pub(super) struct ChunkReader<S> {
    #[cfg(feature = "http2")]
    pub(super) read: Option<crate::http2::stream::ChunkReader<S>>,
    marker: std::marker::PhantomData<fn(S)>,
}

impl<S> Default for ChunkReader<S> {
    fn default() -> Self {
        Self {
            #[cfg(feature = "http2")]
            read: None,
            marker: std::marker::PhantomData,
        }
    }
}

pub(super) struct ReceiveBuffer {
    copied: BytesMut,
    #[cfg(feature = "http2")]
    owned: Bytes,
}

impl ReceiveBuffer {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            copied: BytesMut::with_capacity(capacity),
            #[cfg(feature = "http2")]
            owned: Bytes::new(),
        }
    }

    pub(super) fn writable(&mut self) -> &mut BytesMut {
        #[cfg(feature = "http2")]
        if !self.owned.is_empty() {
            // A frame spanning chunks, or requiring unmasking, needs writable
            // contiguous storage. Complete unmasked frames never enter here.
            debug_assert!(self.copied.is_empty());
            self.copied.extend_from_slice(&self.owned);
            self.owned = Bytes::new();
        }
        &mut self.copied
    }

    #[cfg(feature = "http2")]
    pub(super) fn receive_chunk(&mut self, chunk: Bytes) -> usize {
        let len = chunk.len();
        if self.is_empty() {
            // Reclaim a previously assembled frame at the empty boundary, before
            // the next partial frame could require moving a large prefix (#53).
            if self.copied.capacity() != 0 {
                let _ = self.copied.try_reclaim(crate::RECV_BUFFER_SIZE);
            }
        } else {
            // Preserve only the unfinished prefix. The parser will take just
            // enough of the new chunk to complete it, keeping later frames owned.
            let _ = self.writable();
        }
        self.owned = chunk;
        len
    }

    pub(super) fn clear(&mut self) {
        #[cfg(feature = "http2")]
        {
            self.owned = Bytes::new();
        }
        self.copied.clear();
    }

    #[cfg(feature = "http2")]
    pub(super) fn release_empty_window(&mut self) {
        if self.copied.is_empty() {
            self.copied = BytesMut::new();
        }
    }

    pub(super) fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.writable().extend_from_slice(bytes);
    }

    pub(super) fn capacity(&self) -> usize {
        self.copied.capacity()
    }
    pub(super) fn reserve(&mut self, additional: usize) {
        self.writable().reserve(additional);
    }
    pub(super) fn try_reclaim(&mut self, additional: usize) -> bool {
        self.writable().try_reclaim(additional)
    }
}

impl Deref for ReceiveBuffer {
    type Target = [u8];
    #[cfg(not(feature = "http2"))]
    fn deref(&self) -> &[u8] {
        &self.copied
    }

    #[cfg(feature = "http2")]
    fn deref(&self) -> &[u8] {
        if !self.copied.is_empty() || self.owned.is_empty() {
            &self.copied
        } else {
            &self.owned
        }
    }
}

impl Buf for ReceiveBuffer {
    fn remaining(&self) -> usize {
        #[cfg(feature = "http2")]
        {
            self.copied.len() + self.owned.len()
        }
        #[cfg(not(feature = "http2"))]
        {
            self.copied.len()
        }
    }
    fn chunk(&self) -> &[u8] {
        self
    }
    #[cfg(not(feature = "http2"))]
    fn advance(&mut self, count: usize) {
        self.copied.advance(count);
    }

    #[cfg(feature = "http2")]
    fn advance(&mut self, count: usize) {
        assert!(count <= self.remaining());
        let copied = count.min(self.copied.len());
        self.copied.advance(copied);
        let owned = count - copied;
        if owned == self.owned.len() {
            // A zero-length cursor must not pin the h2 connection allocation.
            self.owned = Bytes::new();
        } else {
            self.owned.advance(owned);
        }
    }
}

impl FrameInput for ReceiveBuffer {
    #[cfg(not(feature = "http2"))]
    fn parse_frame(&mut self, parser: &mut FrameParser) -> Result<Option<Frame>> {
        parser.parse(&mut self.copied)
    }

    #[cfg(feature = "http2")]
    fn parse_frame(&mut self, parser: &mut FrameParser) -> Result<Option<Frame>> {
        loop {
            if self.copied.is_empty() && !self.owned.is_empty() {
                return parser.parse_input(self);
            }
            match parser.parse(&mut self.copied)? {
                Some(frame) => return Ok(Some(frame)),
                None if self.owned.is_empty() => return Ok(None),
                None => {}
            }
            // Partial headers need at most fourteen bytes. Once the header is
            // known, join only this frame's missing payload, never the next frame.
            let needed = parser
                .pending_payload()
                .map_or(1, |pending| pending.total - self.copied.len());
            let count = needed.min(self.owned.len());
            self.copied.extend_from_slice(&self.owned[..count]);
            self.owned.advance(count);
            if self.owned.is_empty() {
                self.owned = Bytes::new();
            }
        }
    }

    fn unmask(&mut self, start: usize, end: usize, mask: [u8; 4]) {
        self.writable().unmask(start, end, mask);
    }

    #[cfg(not(feature = "http2"))]
    fn take_payload(&mut self, len: usize) -> Bytes {
        self.copied.take_payload(len)
    }

    #[cfg(feature = "http2")]
    fn take_payload(&mut self, len: usize) -> Bytes {
        if !self.copied.is_empty() || self.owned.is_empty() {
            self.copied.take_payload(len)
        } else {
            let payload = self.owned.split_to(len);
            if self.owned.is_empty() {
                self.owned = Bytes::new();
            }
            payload
        }
    }
}
