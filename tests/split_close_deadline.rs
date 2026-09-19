#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::poll;
use sockudo_ws::{Config, Error, WebSocketStream};

#[tokio::test(start_paused = true)]
async fn application_close_write_uses_the_closing_budget() {
    let (io, _peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();
    let reason = "x".repeat(80);
    let close = writer.close(1000, &reason);
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), close)
        .await
        .expect("a blocked Close must not outlive the closing budget");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
}
