#![cfg(feature = "compio-runtime")]

use std::time::Duration;

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use sockudo_ws::{CompioWebSocketStream, Config};

#[compio::test]
async fn data_received_while_idle_timer_sleeps_postpones_expiry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let io = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let config = Config::builder().auto_ping(false).idle_timeout(2).build();
    let (mut reader, writer) = CompioWebSocketStream::client(io, config).split();
    compio::time::sleep(Duration::from_millis(1100)).await;
    peer.write_all(b"\x82\x01a".to_vec()).await.0.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"a");

    compio::time::sleep(Duration::from_millis(1100)).await;

    assert!(!writer.is_closed());
    peer.write_all(b"\x82\x01b".to_vec()).await.0.unwrap();
    assert_eq!(reader.next().await.unwrap().unwrap().as_bytes(), b"b");
}
