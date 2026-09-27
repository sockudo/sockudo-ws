#![cfg(feature = "tokio-runtime")]

use sockudo_ws::{Config, Error, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[tokio::test(start_paused = true)]
async fn elapsed_idle_timeout_precedes_ready_transport_data() {
    let (io, mut peer) = tokio::io::duplex(128);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, _writer) = WebSocketStream::client(io, config).split();

    peer.write_all(b"\x82\x01a").await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    tokio::task::yield_now().await;

    assert!(matches!(reader.next().await, Some(Err(Error::IdleTimeout))));
    assert!(reader.next().await.is_none());
}
