#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use sockudo_ws::{Config, WebSocketStream};
use tokio::io::AsyncWriteExt;

#[cfg_attr(feature = "test-util", tokio::test(start_paused = true))]
#[cfg_attr(not(feature = "test-util"), tokio::test)]
async fn data_received_while_idle_timer_sleeps_postpones_expiry() {
    let (io, mut peer) = tokio::io::duplex(128);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, writer) = WebSocketStream::client(io, config).split();
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    peer.write_all(b"\x82\x01a").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");

    tokio::time::sleep(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;

    assert!(!writer.is_closed());
    peer.write_all(b"\x82\x01b").await.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"b");
}
