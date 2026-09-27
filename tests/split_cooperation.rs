#![cfg(feature = "tokio-runtime")]

use futures_util::{StreamExt, poll};
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[path = "support/cooperation.rs"]
mod cooperation;
use cooperation::{buffered_frames, payload};

#[tokio::test]
async fn buffered_split_reads_yield_before_draining_a_large_batch() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let wire = buffered_frames(false);
    peer.write_all(&wire).await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &payload(sequence)
            );
        }
    });
    // Buffered messages must not bypass the runtime's cooperative scheduling.
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}

#[tokio::test]
async fn buffered_unified_reads_yield_before_draining_a_large_batch() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let wire = buffered_frames(false);
    peer.write_all(&wire).await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let mut reader = WebSocketStream::client(io, config);
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
#[tokio::test]
async fn buffered_compressed_unified_reads_yield_before_draining_a_large_batch() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let wire = buffered_frames(true);
    peer.write_all(&wire).await.unwrap();
    let mut reader =
        sockudo_ws::CompressedWebSocketStream::client(io, Config::default(), Default::default());
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
#[tokio::test]
async fn buffered_compressed_split_reads_yield_before_draining_a_large_batch() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let wire = buffered_frames(true);
    peer.write_all(&wire).await.unwrap();
    let stream =
        sockudo_ws::CompressedWebSocketStream::client(io, Config::default(), Default::default());
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

macro_rules! cancellation_case {
    ($name:ident, $compressed:expr, $make:expr) => {
        #[tokio::test]
        async fn $name() {
            let (io, mut peer) = tokio::io::duplex(65536);
            peer.write_all(&buffered_frames($compressed)).await.unwrap();
            let config = Config::builder().auto_ping(false).idle_timeout(0).build();
            let (mut reader, _writer) = ($make)(io, config);
            let mut delivered = 0;
            {
                let drain = async {
                    while delivered < 256 {
                        let message = reader.next().await.unwrap().unwrap();
                        assert_eq!(message.as_bytes(), &payload(delivered));
                        delivered += 1;
                    }
                };
                tokio::pin!(drain);
                assert!(poll!(&mut drain).is_pending());
            }
            assert!((1..256).contains(&delivered));
            // Dropping the pending call must not remove the next message.
            tokio::task::yield_now().await;
            for sequence in delivered..256 {
                assert_eq!(
                    reader.next().await.unwrap().unwrap().as_bytes(),
                    &payload(sequence)
                );
            }
        }
    };
}

cancellation_case!(cancelled_unified_yield_preserves_order, false, |io, cfg| (
    WebSocketStream::client(io, cfg),
    ()
));
cancellation_case!(cancelled_split_yield_preserves_order, false, |io, cfg| {
    WebSocketStream::client(io, cfg).split()
});
#[cfg(feature = "permessage-deflate")]
cancellation_case!(
    cancelled_compressed_unified_yield_preserves_order,
    true,
    |io, cfg| (
        sockudo_ws::CompressedWebSocketStream::client(io, cfg, Default::default()),
        ()
    )
);
#[cfg(feature = "permessage-deflate")]
cancellation_case!(
    cancelled_compressed_split_yield_preserves_order,
    true,
    |io, cfg| sockudo_ws::CompressedWebSocketStream::client(io, cfg, Default::default()).split()
);
