#![cfg(feature = "tokio-runtime")]

use std::time::Duration;

use futures_util::poll;
use sockudo_ws::{Config, Error, Message, WebSocketStream};

async fn read_masked_control_frame(peer: &mut tokio::io::DuplexStream) -> (u8, Vec<u8>) {
    use tokio::io::AsyncReadExt;

    let mut header = [0; 2];
    peer.read_exact(&mut header).await.unwrap();
    assert_ne!(header[1] & 0x80, 0, "client control frames must be masked");
    let len = usize::from(header[1] & 0x7f);
    assert!(len <= 125, "control frames cannot use extended lengths");
    let mut mask = [0; 4];
    peer.read_exact(&mut mask).await.unwrap();
    let mut payload = vec![0; len];
    peer.read_exact(&mut payload).await.unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
    }
    (header[0] & 0x0f, payload)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_close_timeout_still_writes_the_close_frame() {
    use tokio::io::AsyncReadExt;

    let (io, mut peer) = tokio::io::duplex(4096);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (_reader, mut writer) = WebSocketStream::server(io, config).split();

    writer.close(1000, "bye").await.unwrap();

    let mut first = [0u8; 1];
    tokio::time::timeout(Duration::from_millis(200), peer.read_exact(&mut first))
        .await
        .expect("the immediate best-effort Close must reach the peer")
        .unwrap();
    assert_eq!(first[0], 0x88);
}

#[tokio::test]
async fn zero_close_timeout_does_not_wait_for_the_sink() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(200), writer.close(1000, "done"))
        .await
        .expect("a zero closing budget must not wait for the shared sink");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
    assert!(writer.is_closed());
}

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

#[tokio::test(start_paused = true)]
async fn application_close_budget_includes_waiting_for_the_sink() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();
    let close = writer.close(1000, "done");
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(1)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), close)
        .await
        .expect("waiting behind a blocked Pong must use the closing budget");

    assert!(matches!(result, Err(Error::ConnectionClosed)));
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_started_close_keeps_the_connection_closing() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();

    {
        let close = writer.close(1000, "done");
        tokio::pin!(close);
        assert!(poll!(&mut close).is_pending());
    }

    assert!(writer.is_closed());
    tokio::time::advance(Duration::from_secs(1)).await;
    let terminal = tokio::time::timeout(Duration::from_millis(1), reader.next())
        .await
        .expect("the original closing budget must terminate the reader");
    assert!(terminal.is_none());
    assert!(matches!(
        writer.send_binary(b"later".as_slice().into()).await,
        Err(Error::ConnectionClosed)
    ));
}

#[tokio::test]
async fn peer_close_during_local_close_does_not_queue_a_second_close() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (io, mut peer) = tokio::io::duplex(8);
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(1)
        .build();
    let (mut reader, mut writer) = WebSocketStream::client(io, config).split();
    let sending = tokio::spawn(async move {
        let mut ping = vec![0x89, 64];
        ping.extend_from_slice(&[b'p'; 64]);
        peer.write_all(&ping).await.unwrap();
        peer
    });
    assert!(reader.next().await.unwrap().unwrap().is_ping());
    let mut peer = sending.await.unwrap();
    let mut prefix = [0; 8];
    peer.read_exact(&mut prefix).await.unwrap();

    let close = writer.close(1000, "done");
    tokio::pin!(close);
    assert!(poll!(&mut close).is_pending());

    peer.write_all(&[0x88, 2, 3, 232]).await.unwrap();
    assert!(matches!(
        reader.next().await,
        Some(Ok(Message::Close(Some(reason)))) if reason.code == 1000
    ));
    tokio::task::yield_now().await;

    let mut rest_of_pong = [0; 62];
    peer.read_exact(&mut rest_of_pong).await.unwrap();
    let (close_result, (opcode, payload)) =
        tokio::join!(&mut close, read_masked_control_frame(&mut peer));
    close_result.unwrap();
    assert_eq!(opcode, 0x08);
    assert_eq!(payload, b"\x03\xe8done");

    let mut next = [0; 1];
    let read = tokio::time::timeout(Duration::from_millis(200), peer.read(&mut next))
        .await
        .expect("the closing handshake must finish after the local Close")
        .unwrap();
    assert_eq!(read, 0, "the driver must not append a second Close frame");
}
