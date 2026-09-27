#![cfg(feature = "compio-runtime")]

use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use futures_util::poll;
use sockudo_ws::Config;
use std::io;
use std::task::Poll;

#[path = "support/cooperation.rs"]
mod cooperation;
use cooperation::{buffered_frames, payload};

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

#[compio::test]
async fn buffered_unified_reads_yield() {
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut reader = sockudo_ws::compio::CompioWebSocketStream::client_with_leftover(
        compio::io::null(),
        config,
        Some(buffered_frames(false)),
    );
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &payload(sequence)
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
        Some(buffered_frames(false)),
    );
    let (mut reader, _writer) = stream.split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &payload(sequence)
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
        Some(buffered_frames(true)),
    );
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &payload(sequence)
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
        Some(buffered_frames(true)),
    );
    let (mut reader, _writer) = stream.split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &payload(sequence)
            );
        }
    });
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[compio::test]
async fn pending_io_does_not_add_a_buffered_delivery_yield() {
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

macro_rules! cancellation_case {
    ($name:ident, $compressed:expr, $make:expr) => {
        #[compio::test]
        async fn $name() {
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let (mut reader, _writer) = ($make)(config, buffered_frames($compressed));
            let mut delivered = 0;
            {
                let mut drain = std::pin::pin!(async {
                    while delivered < 256 {
                        let message = reader.next().await.unwrap().unwrap();
                        assert_eq!(message.as_bytes(), &payload(delivered));
                        delivered += 1;
                    }
                });
                assert!(poll!(drain.as_mut()).is_pending());
            }
            assert!((1..256).contains(&delivered));
            // A cancelled yield must neither drop a message nor yield again
            // without making progress on the same buffered message.
            let Poll::Ready(Some(Ok(message))) = poll!(std::pin::pin!(reader.next())) else {
                panic!("cancelled cooperative yield stalled the same message");
            };
            assert_eq!(message.as_bytes(), &payload(delivered));
            for sequence in delivered + 1..256 {
                assert_eq!(
                    reader.next().await.unwrap().unwrap().as_bytes(),
                    &payload(sequence)
                );
            }
        }
    };
}

cancellation_case!(
    cancelled_unified_yield_preserves_order,
    false,
    |cfg, wire| (
        sockudo_ws::CompioWebSocketStream::client_with_leftover(
            compio::io::null(),
            cfg,
            Some(wire)
        ),
        ()
    )
);
cancellation_case!(cancelled_split_yield_preserves_order, false, |cfg, wire| {
    sockudo_ws::CompioWebSocketStream::client_with_leftover(compio::io::null(), cfg, Some(wire))
        .split()
});
#[cfg(feature = "permessage-deflate")]
cancellation_case!(
    cancelled_compressed_unified_yield_preserves_order,
    true,
    |cfg, wire| (
        sockudo_ws::compio::CompioCompressedWebSocketStream::client_with_leftover(
            compio::io::null(),
            cfg,
            Default::default(),
            Some(wire)
        ),
        ()
    )
);
#[cfg(feature = "permessage-deflate")]
cancellation_case!(
    cancelled_compressed_split_yield_preserves_order,
    true,
    |cfg, wire| sockudo_ws::compio::CompioCompressedWebSocketStream::client_with_leftover(
        compio::io::null(),
        cfg,
        Default::default(),
        Some(wire)
    )
    .split()
);

macro_rules! split_budget_case {
    ($name:ident, $compressed:expr, $make:expr) => {
        #[compio::test]
        async fn $name() {
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let mut stream = ($make)(config, buffered_frames($compressed));
            // The first call parses a batch; the next 32 spend its delivery
            // budget. Splitting must not grant another fresh burst.
            for sequence in 0..33 {
                assert_eq!(
                    stream.next().await.unwrap().unwrap().as_bytes(),
                    &payload(sequence)
                );
            }
            let (mut reader, _writer) = stream.split();
            let mut next = std::pin::pin!(reader.next());
            assert!(poll!(next.as_mut()).is_pending());
            assert_eq!(next.await.unwrap().unwrap().as_bytes(), &payload(33));
        }
    };
}

split_budget_case!(
    split_inherits_remaining_delivery_budget,
    false,
    |cfg, wire| sockudo_ws::CompioWebSocketStream::client_with_leftover(
        compio::io::null(),
        cfg,
        Some(wire)
    )
);
#[cfg(feature = "permessage-deflate")]
split_budget_case!(
    compressed_split_inherits_remaining_delivery_budget,
    true,
    |cfg, wire| sockudo_ws::compio::CompioCompressedWebSocketStream::client_with_leftover(
        compio::io::null(),
        cfg,
        Default::default(),
        Some(wire)
    )
);
