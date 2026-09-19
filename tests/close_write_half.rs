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
        let mut wire = Vec::new();
        server.read_to_end(&mut wire).await.unwrap();
        assert_eq!(wire[0], 0x88);
        // A write-half shutdown must still allow the peer's Close response.
        server.write_all(b"\x88\x02\x03\xe8").await.unwrap();
    });
    (client, task)
}

#[tokio::test]
async fn close_finishes_tcp_writes_but_receives_the_peer_close() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (socket, peer) = peer().await;
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut ws = WebSocketStream::client(socket, config);
        ws.close(1000, "bye").await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_close());
        peer.await.unwrap();
    })
    .await
    .unwrap();
}

#[cfg(feature = "permessage-deflate")]
#[tokio::test]
async fn compressed_close_finishes_tcp_writes_but_receives_the_peer_close() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (socket, peer) = peer().await;
        let config = Config::builder().auto_ping(false).idle_timeout(0).build();
        let mut ws =
            sockudo_ws::CompressedWebSocketStream::client(socket, config, Default::default());
        ws.close(1000, "bye").await.unwrap();
        assert!(ws.next().await.unwrap().unwrap().is_close());
        peer.await.unwrap();
    })
    .await
    .unwrap();
}
