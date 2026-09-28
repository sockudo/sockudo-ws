//! Deterministic transports. Pending is self-woken, not a network-readiness model.

use bytes::Bytes;
use std::hint::black_box;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub struct Read {
    pub bytes: usize,
    pub capacity: usize,
}

#[derive(Clone)]
pub struct Input {
    pub wire: Bytes,
    pub limit: usize,
    pub inject_pending: bool,
    offset: usize,
    loop_start: usize,
    cuts: Vec<usize>,
    pub reads: Option<Arc<Mutex<Vec<Read>>>>,
    #[cfg(feature = "tokio-runtime")]
    pending: bool,
}

impl Input {
    pub fn new(wire: Bytes, limit: usize, pending: bool) -> Self {
        assert!(!wire.is_empty() && limit > 0);
        Self {
            wire,
            limit,
            inject_pending: pending,
            offset: 0,
            loop_start: 0,
            cuts: Vec::new(),
            reads: None,
            #[cfg(feature = "tokio-runtime")]
            pending,
        }
    }

    /// Cut at absolute wire offsets; destination capacity may split a chunk further.
    pub fn with_layout(mut self, cuts: Vec<usize>, loop_start: usize) -> Self {
        assert!(loop_start < self.wire.len());
        assert!(cuts.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(cuts.iter().all(|&cut| cut > 0 && cut < self.wire.len()));
        self.cuts = cuts;
        self.loop_start = loop_start;
        self
    }

    fn chunk(&mut self, capacity: usize) -> &[u8] {
        let start = self.offset;
        let boundary = self
            .cuts
            .iter()
            .copied()
            .find(|&cut| cut > start)
            .unwrap_or(self.wire.len());
        let end = start + capacity.min(self.limit).min(boundary - start);
        self.offset = if end == self.wire.len() {
            self.loop_start
        } else {
            end
        };
        if let Some(reads) = &self.reads {
            reads.lock().unwrap().push(Read {
                bytes: end - start,
                capacity,
            });
        }
        &self.wire[start..end]
    }
}

#[derive(Default)]
pub struct Written {
    pub bytes: Vec<u8>,
    pub scalar: usize,
    pub vectored: usize,
    pub max_slices: usize,
    pub pending: usize,
    pub short_writes: usize,
}

#[derive(Clone)]
pub struct Output {
    pub limit: usize,
    pub vectored: bool,
    pub inject_pending: bool,
    pub trace: Option<Arc<Mutex<Written>>>,
    #[cfg(feature = "tokio-runtime")]
    pending: bool,
}

impl Output {
    pub fn new(limit: usize, vectored: bool, pending: bool) -> Self {
        assert!(limit > 0);
        Self {
            limit,
            vectored,
            inject_pending: pending,
            trace: None,
            #[cfg(feature = "tokio-runtime")]
            pending,
        }
    }

    fn accept(&mut self, bufs: &[io::IoSlice<'_>], vectored: bool) -> usize {
        let mut remaining = self.limit;
        let mut trace = self.trace.as_ref().map(|trace| trace.lock().unwrap());
        if let Some(trace) = &mut trace {
            trace.scalar += usize::from(!vectored);
            trace.vectored += usize::from(vectored);
            trace.max_slices = trace.max_slices.max(bufs.len());
            trace.short_writes +=
                usize::from(bufs.iter().map(|buf| buf.len()).sum::<usize>() > self.limit);
        }
        for buf in bufs {
            let n = remaining.min(buf.len());
            black_box(&buf[..n]);
            if let Some(trace) = &mut trace {
                trace.bytes.extend_from_slice(&buf[..n]);
            }
            remaining -= n;
            if remaining == 0 {
                break;
            }
        }
        self.limit - remaining
    }
}

#[cfg(feature = "tokio-runtime")]
mod tokio_io {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    impl AsyncRead for Input {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending = self.inject_pending;
            buf.put_slice(self.chunk(buf.remaining()));
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for Input {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(black_box(buf).len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncRead for Output {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    impl Output {
        fn write(
            &mut self,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
            vectored: bool,
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                if let Some(trace) = &self.trace {
                    trace.lock().unwrap().pending += 1;
                }
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending = self.inject_pending;
            Poll::Ready(Ok(self.accept(bufs, vectored)))
        }
    }
    impl AsyncWrite for Output {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write(cx, &[io::IoSlice::new(buf)], false)
        }
        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[io::IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.write(cx, bufs, true)
        }
        fn is_write_vectored(&self) -> bool {
            self.vectored
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(feature = "compio-runtime")]
mod compio_io {
    use super::*;
    use compio::{
        buf::{BufResult, IoBuf, IoBufMut},
        io::{AsyncRead, AsyncWrite, util::Splittable},
    };
    use std::task::Poll;

    async fn yield_once(enabled: bool) {
        let mut pending = enabled;
        std::future::poll_fn(|cx| {
            if pending {
                pending = false;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }
    impl AsyncRead for Input {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
            yield_once(self.inject_pending).await;
            let chunk = self.chunk(buf.buf_capacity());
            let n = chunk.len();
            for (destination, source) in buf.as_uninit().iter_mut().zip(chunk) {
                destination.write(*source);
            }
            // Exactly the prefix above has been initialized.
            unsafe {
                buf.set_len(n);
            }
            BufResult(Ok(n), buf)
        }
    }
    impl AsyncWrite for Input {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            BufResult(Ok(black_box(buf.as_init()).len()), buf)
        }
        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Splittable for Input {
        type ReadHalf = Self;
        type WriteHalf = Self;
        fn split(self) -> (Self, Self) {
            (self.clone(), self)
        }
    }
    impl AsyncRead for Output {
        async fn read<B: IoBufMut>(&mut self, _: B) -> BufResult<usize, B> {
            std::future::pending().await
        }
    }
    impl AsyncWrite for Output {
        async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
            yield_once(self.inject_pending).await;
            if self.inject_pending
                && let Some(trace) = &self.trace
            {
                trace.lock().unwrap().pending += 1;
            }
            let n = self.accept(&[io::IoSlice::new(buf.as_init())], false);
            BufResult(Ok(n), buf)
        }
        async fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Splittable for Output {
        type ReadHalf = Self;
        type WriteHalf = Self;
        fn split(self) -> (Self, Self) {
            (self.clone(), self)
        }
    }
}
