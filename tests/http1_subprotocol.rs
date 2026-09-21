#![cfg(feature = "tokio-runtime")]

mod support;

use sockudo_ws::Error;
use sockudo_ws::handshake::{client_handshake, generate_accept_key};
use support::{extract_header, read_http_request};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn client_rejects_an_unoffered_subprotocol() {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
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
             Sec-WebSocket-Protocol: unoffered\r\n\
             \r\n"
        );
        server_io.write_all(response.as_bytes()).await.unwrap();
    });

    let error = client_handshake(&mut client_io, "example.com", "/ws", Some("chat"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        Error::HandshakeFailed("server returned an unoffered subprotocol")
    ));
    server.await.unwrap();
}
