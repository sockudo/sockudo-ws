#![cfg(feature = "compio-runtime")]

use bytes::Bytes;
use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use futures_util::poll;
use sockudo_ws::Config;
use std::io;
use std::task::Poll;

struct OneFramePerPendingRead {
    sequence: u8,
}

impl AsyncRead for OneFramePerPendingRead {
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> BufResult<usize, B> {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;

        let frame = [0x82, 1, self.sequence];
        for (slot, byte) in buf.as_uninit()[..frame.len()].iter_mut().zip(frame) {
            slot.write(byte);
        }
        // SAFETY: The complete frame was initialized above.
        unsafe { buf.advance_to(frame.len()) };
        self.sequence = self.sequence.wrapping_add(1);
        BufResult(Ok(frame.len()), buf)
    }
}

impl AsyncWrite for OneFramePerPendingRead {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        BufResult(Ok(buf.buf_len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn buffered_frames() -> Bytes {
    (0..256)
        .flat_map(|sequence| [0x82, 1, sequence as u8])
        .collect::<Vec<_>>()
        .into()
}

#[compio::test]
async fn buffered_unified_reads_yield() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut reader = sockudo_ws::compio::CompioWebSocketStream::client_with_leftover(
        compio::io::null(),
        config,
        Some(buffered_frames()),
    );
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence as u8]
            );
        }
    });
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[compio::test]
async fn buffered_split_reads_yield() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let stream = sockudo_ws::compio::CompioWebSocketStream::client_with_leftover(
        compio::io::null(),
        config,
        Some(buffered_frames()),
    );
    let (mut reader, _writer) = stream.split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence as u8]
            );
        }
    });
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn buffered_compressed_unified_reads_yield() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut reader = sockudo_ws::compio::CompioCompressedWebSocketStream::client_with_leftover(
        compio::io::null(),
        config,
        Default::default(),
        Some(buffered_frames()),
    );
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence as u8]
            );
        }
    });
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn buffered_compressed_split_reads_yield() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let stream = sockudo_ws::compio::CompioCompressedWebSocketStream::client_with_leftover(
        compio::io::null(),
        config,
        Default::default(),
        Some(buffered_frames()),
    );
    let (mut reader, _writer) = stream.split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence as u8]
            );
        }
    });
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[compio::test]
async fn io_pending_resets_the_unified_ready_burst() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut reader = sockudo_ws::compio::CompioWebSocketStream::client(
        OneFramePerPendingRead { sequence: 0 },
        config,
    );

    for sequence in 0..64 {
        let mut next = std::pin::pin!(reader.next());
        assert!(poll!(next.as_mut()).is_pending());
        let Poll::Ready(Some(Ok(message))) = poll!(next.as_mut()) else {
            panic!("the read budget yielded in addition to pending I/O");
        };
        assert_eq!(message.as_bytes(), &[sequence]);
    }
}
