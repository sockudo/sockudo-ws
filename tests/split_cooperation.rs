#![cfg(feature = "tokio-runtime")]

use futures_util::poll;
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn buffered_split_reads_yield_before_draining_a_large_batch() {
    let (io, mut peer) = tokio::io::duplex(4096);
    let wire: Vec<u8> = (0..256)
        .flat_map(|sequence| [0x82, 1, sequence as u8])
        .collect();
    peer.write_all(&wire).await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(0).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();
    let mut drain = std::pin::pin!(async {
        for sequence in 0..256 {
            assert_eq!(
                reader.next().await.unwrap().unwrap().as_bytes(),
                &[sequence as u8]
            );
        }
    });
    // Buffered messages must not bypass the runtime's cooperative scheduling.
    assert!(poll!(drain.as_mut()).is_pending());
    drain.await;
}
