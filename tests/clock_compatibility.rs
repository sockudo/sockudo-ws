#![cfg(feature = "tokio-runtime")]

use std::future::Future;
use std::task::Poll;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, poll};
use sockudo_ws::{Config, Error, Message, Result, WebSocketStream};

#[cfg_attr(not(feature = "test-util"), ignore = "requires test-util clock")]
#[tokio::test(start_paused = true)]
async fn advancing_tokio_time_expires_split_idle_without_real_wait() {
    let (socket, _peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut reader, _writer) = WebSocketStream::server(socket, config).split();
    // Start the driver before moving time; it initializes Heartbeat on first poll.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(2)).await;
    // Let the independent driver observe the advanced clock and publish its error.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let next = reader.next();
    tokio::pin!(next);
    assert!(matches!(
        poll!(next.as_mut()),
        Poll::Ready(Some(Err(Error::IdleTimeout)))
    ));
}

#[tokio::test]
async fn unified_idle_expires_on_the_real_clock() {
    let (socket, _peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let started = Instant::now();
    let mut ws = WebSocketStream::server(socket, config);
    assert_real_idle_expiry(ws.next(), started).await;
}

#[tokio::test]
async fn split_idle_expires_on_the_real_clock() {
    let (socket, _peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let started = Instant::now();
    let (mut reader, _writer) = WebSocketStream::server(socket, config).split();
    assert_real_idle_expiry(reader.next(), started).await;
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_idle_expires_on_the_real_clock() {
    let (socket, _peer) = tokio::io::duplex(1024);
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let started = Instant::now();
    let mut ws = sockudo_ws::CompressedWebSocketStream::server(
        socket,
        config,
        sockudo_ws::deflate::DeflateConfig::default(),
    );
    assert_real_idle_expiry(ws.next(), started).await;
}

async fn assert_real_idle_expiry(
    next: impl Future<Output = Option<Result<Message>>>,
    started: Instant,
) {
    let message = tokio::time::timeout(Duration::from_secs(3), next)
        .await
        .expect("idle expiry must wake the reader");
    assert!(matches!(message, Some(Err(Error::IdleTimeout))));
    assert!(started.elapsed() >= Duration::from_secs(1));
}
