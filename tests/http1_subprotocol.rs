#![cfg(feature = "tokio-runtime")]

mod support;

use sockudo_ws::Error;
use sockudo_ws::handshake::{client_handshake, generate_accept_key};
use sockudo_ws::server::WebSocketServer;
use sockudo_ws::{Config, Http1};
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

async fn configured_server_round_trip(
    supported: &[&str],
    offered: Option<&str>,
) -> (Option<String>, Option<String>) {
    let (mut client_io, server_io) = tokio::io::duplex(4096);
    let server = WebSocketServer::<Http1>::new(Config::default())
        .protocols(supported)
        .unwrap();
    // Exercise the cloned configuration used by serve's per-connection tasks.
    let server = server.clone();
    let server = tokio::spawn(async move {
        let (_websocket, handshake) = server.accept_raw(server_io).await.unwrap();
        handshake
    });

    let client = client_handshake(&mut client_io, "example.com", "/ws", offered)
        .await
        .unwrap();
    let server = server.await.unwrap();

    (client.protocol, server.protocol)
}

#[tokio::test]
async fn configured_server_selects_in_server_preference_order() {
    assert_eq!(
        configured_server_round_trip(&["superchat", "chat"], Some("chat, superchat")).await,
        (Some("superchat".to_owned()), Some("superchat".to_owned()))
    );
}

#[tokio::test]
async fn configured_server_does_not_select_unoffered_or_case_mismatched_protocols() {
    assert_eq!(
        configured_server_round_trip(&["SUPERCHAT", "video"], Some("chat, superchat")).await,
        (None, None)
    );
}

#[tokio::test]
async fn explicit_empty_supported_protocols_disable_default_selection() {
    assert_eq!(
        configured_server_round_trip(&[], Some("chat, superchat")).await,
        (None, None)
    );
}

#[test]
fn server_rejects_invalid_supported_protocols_before_network_io() {
    let error = WebSocketServer::<Http1>::new(Config::default())
        .protocols(["invalid protocol"])
        .err()
        .unwrap();

    assert!(matches!(
        error,
        Error::InvalidHttp("invalid supported subprotocol")
    ));
}

#[test]
fn server_accepts_borrowed_owned_supported_protocols() {
    let protocols = vec!["chat".to_owned(), "superchat".to_owned()];

    assert!(
        WebSocketServer::<Http1>::new(Config::default())
            .protocols(&protocols)
            .is_ok()
    );
}
