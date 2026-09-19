#![cfg(feature = "tokio-runtime")]

mod support;

use futures_util::StreamExt;
use sockudo_ws::{Config, Http1, Message};
use sockudo_ws::{
    client::WebSocketClient,
    handshake::{build_request, generate_accept_key},
    server::WebSocketServer,
};
use support::{extract_header, read_http_request};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::{Duration, timeout};

async fn write_upgrade_response_with_text_frame<S>(stream: &mut S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = read_http_request(stream).await;
    let request = String::from_utf8(request).unwrap();
    let key = extract_header(&request, "Sec-WebSocket-Key").unwrap();
    let accept = generate_accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n"
    );

    let mut response_and_frame = response.into_bytes();
    response_and_frame.extend_from_slice(b"\x81\x05hello");
    stream.write_all(&response_and_frame).await.unwrap();
}

#[tokio::test]
async fn http1_client_replays_frame_read_with_upgrade_response() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        write_upgrade_response_with_text_frame(&mut server_io).await;
    });

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (mut websocket, handshake) = client
        .connect(client_io, "example.com", "/ws", None)
        .await
        .unwrap();

    assert_eq!(
        handshake.leftover.as_deref(),
        Some(b"\x81\x05hello".as_slice())
    );
    assert!(matches!(
        websocket.next().await,
        Some(Ok(Message::Text(payload))) if payload == "hello"
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn http1_split_client_replays_frame_read_with_upgrade_response() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let (release_server, wait_for_release) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        write_upgrade_response_with_text_frame(&mut server_io).await;
        let _ = wait_for_release.await;
    });

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (websocket, handshake) = client
        .connect(client_io, "example.com", "/ws", None)
        .await
        .unwrap();
    let (mut reader, _writer) = websocket.split();

    assert_eq!(
        handshake.leftover.as_deref(),
        Some(b"\x81\x05hello".as_slice())
    );
    let message = timeout(Duration::from_secs(1), reader.next())
        .await
        .expect("split reader did not process handshake leftover")
        .expect("split reader closed before returning handshake leftover")
        .unwrap();
    assert!(matches!(message, Message::Text(payload) if payload == "hello"));

    release_server.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn http1_server_replays_frame_read_with_upgrade_request() {
    let (mut client_io, server_io) = tokio::io::duplex(4096);
    let request =
        build_request("example.com", "/ws", "dGhlIHNhbXBsZSBub25jZQ==", None, None).unwrap();
    let masked_text_frame = b"\x81\x85\x01\x02\x03\x04\x69\x67\x6f\x68\x6e";
    client_io.write_all(&request).await.unwrap();
    client_io.write_all(masked_text_frame).await.unwrap();

    let server = WebSocketServer::<Http1>::new(Config::default());
    let (mut websocket, handshake) = server.accept(server_io).await.unwrap();

    assert_eq!(
        handshake.leftover.as_deref(),
        Some(masked_text_frame.as_slice())
    );
    assert!(matches!(
        websocket.next().await,
        Some(Ok(Message::Text(payload))) if payload == "hello"
    ));
}
