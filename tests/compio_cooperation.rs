#![cfg(feature = "compio-runtime")]

use bytes::Bytes;
use futures_util::poll;
use sockudo_ws::Config;

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
