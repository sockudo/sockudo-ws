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

fn pad_header_to(mut header: Vec<u8>, len: usize) -> Vec<u8> {
    assert!(header.ends_with(b"\r\n\r\n"));
    header.truncate(header.len() - 2);
    header.extend_from_slice(b"X-Pad: ");
    header.resize(len - 4, b'a');
    header.extend_from_slice(b"\r\n\r\n");
    assert_eq!(header.len(), len);
    header
}

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
    let request = build_request("example.com", "/ws", "dGhlIHNhbXBsZSBub25jZQ==", None, None);
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

#[tokio::test]
async fn http1_client_replays_frame_after_large_valid_response_header() {
    let (client_io, mut server_io) = tokio::io::duplex(16384);
    let server = tokio::spawn(async move {
        let request = read_http_request(&mut server_io).await;
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
        let mut response_and_frame = pad_header_to(response.into_bytes(), 6000);
        response_and_frame.extend_from_slice(b"\x82\x7e\x10\x00");
        response_and_frame.extend(std::iter::repeat_n(b'x', 4096));
        server_io.write_all(&response_and_frame).await.unwrap();
    });

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (mut websocket, handshake) = client
        .connect(client_io, "example.com", "/ws", None)
        .await
        .unwrap();
    assert!(handshake.leftover.is_some());
    assert!(matches!(
        websocket.next().await,
        Some(Ok(Message::Binary(payload))) if payload.len() == 4096 && payload.iter().all(|byte| *byte == b'x')
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn http1_server_replays_frame_after_large_valid_request_header() {
    let (mut client_io, server_io) = tokio::io::duplex(16384);
    let request = build_request("example.com", "/ws", "dGhlIHNhbXBsZSBub25jZQ==", None, None);
    let mut request_and_frame = pad_header_to(request.to_vec(), 6000);
    request_and_frame.extend_from_slice(b"\x82\xfe\x10\x00\x01\x02\x03\x04");
    request_and_frame.extend((0..4096).map(|i| b'x' ^ [1, 2, 3, 4][i % 4]));
    client_io.write_all(&request_and_frame).await.unwrap();

    let server = WebSocketServer::<Http1>::new(Config::default());
    let (mut websocket, handshake) = server.accept(server_io).await.unwrap();
    assert!(handshake.leftover.is_some());
    assert!(matches!(
        websocket.next().await,
        Some(Ok(Message::Binary(payload))) if payload.len() == 4096 && payload.iter().all(|byte| *byte == b'x')
    ));
}
