#![cfg(feature = "tokio-runtime")]

use futures_util::StreamExt;
use sockudo_ws::{Config, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn peer() -> (tokio::net::TcpStream, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut server, _) = listener.accept().await.unwrap();
    let task = tokio::spawn(async move {
        let mut header = [0; 2];
        server.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 0x88);
        let mut mask = [0; 4];
        server.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0; usize::from(header[1] & 0x7f)];
        server.read_exact(&mut payload).await.unwrap();
        // TCP must remain writable until the peer's Close, including the
        // automatic Pong required for a crossing Ping. Send Close only after
        // Pong: RFC 6455 permits omitting Pong once Close has been received.
        server.write_all(b"\x89\x01p").await.unwrap();
        server.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0], 0x8a);
        server.read_exact(&mut mask).await.unwrap();
        let mut pong = [0; 1];
        server.read_exact(&mut pong).await.unwrap();
        assert_eq!(pong[0] ^ mask[0], b'p');
        server.write_all(b"\x88\x02\x03\xe8").await.unwrap();
        assert_eq!(server.read(&mut pong).await.unwrap(), 0);
    });
    (client, task)
}

#[tokio::test]
async fn close_waits_for_peer_close_and_answers_crossing_ping() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (socket, peer) = peer().await;
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut ws = WebSocketStream::client(socket, config);
        ws.close(1000, "bye").await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_ping());
        assert!(ws.next().await.unwrap().unwrap().is_close());
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_close_waits_for_peer_close_and_answers_crossing_ping() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (socket, peer) = peer().await;
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut ws =
            sockudo_ws::CompressedWebSocketStream::client(socket, config, Default::default());
        ws.close(1000, "bye").await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_ping());
        assert!(ws.next().await.unwrap().unwrap().is_close());
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn close_stops_local_heartbeat_while_waiting_for_peer_close() {
    let (io, mut peer) = tokio::io::duplex(128);
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(0)
        .idle_timeout(0)
        .build();
    let mut ws = WebSocketStream::client(io, config);
    ws.close(1000, "").await.unwrap();
    let mut local_close = [0; 8];
    peer.read_exact(&mut local_close).await.unwrap();
    assert_eq!(local_close[0], 0x88);

    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    peer.write_all(b"\x88\x02\x03\xe8").await.unwrap();
    assert!(ws.next().await.unwrap().unwrap().is_close());
    let mut extra = [0; 1];
    assert_eq!(peer.read(&mut extra).await.unwrap(), 0);
}
