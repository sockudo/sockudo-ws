#![cfg(feature = "tokio-runtime")]

mod support;

use futures_util::StreamExt;
use rstest::rstest;
use sockudo_ws::{Config, Error, Http1, Message};
use sockudo_ws::{
    client::WebSocketClient,
    handshake::{build_request_with_headers, client_handshake_with_headers, generate_accept_key},
};
use support::{extract_header, read_http_request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn http1_request_includes_custom_headers_in_order() {
    let headers = vec![
        ("Authorization".to_string(), "Bearer token".to_string()),
        ("X-Route".to_string(), "blue".to_string()),
        ("X-Route".to_string(), "green".to_string()),
    ];

    let request = build_request_with_headers(
        "example.com:8443",
        "/ws?feed=trades",
        "dGhlIHNhbXBsZSBub25jZQ==",
        None,
        None,
        Some(&headers),
    )
    .unwrap();
    let request = String::from_utf8(request.to_vec()).unwrap();

    assert!(request.starts_with("GET /ws?feed=trades HTTP/1.1\r\n"));
    assert!(request.contains("Host: example.com:8443\r\n"));
    assert!(request.contains("Authorization: Bearer token\r\n"));
    assert!(request.contains("X-Route: blue\r\nX-Route: green\r\n"));
}

#[test]
fn http1_request_accepts_horizontal_tabs_and_obs_text_values() {
    let headers = vec![("X-Note".to_string(), "café\tvalue".to_string())];

    let request = build_request_with_headers(
        "example.com",
        "/ws",
        "dGhlIHNhbXBsZSBub25jZQ==",
        None,
        None,
        Some(&headers),
    )
    .unwrap();

    let expected = "X-Note: café\tvalue".as_bytes();
    assert!(
        request
            .windows(expected.len())
            .any(|window| window == expected)
    );
}

#[rstest]
#[case::host("Host")]
#[case::upgrade("UPGRADE")]
#[case::connection("Connection")]
#[case::key("Sec-WebSocket-Key")]
#[case::version("Sec-WebSocket-Version")]
#[case::protocol("Sec-WebSocket-Protocol")]
#[case::extensions("Sec-WebSocket-Extensions")]
fn http1_request_rejects_reserved_handshake_headers(#[case] name: &str) {
    let headers = vec![(name.to_string(), "value".to_string())];
    let error = build_request_with_headers(
        "example.com",
        "/ws",
        "dGhlIHNhbXBsZSBub25jZQ==",
        None,
        None,
        Some(&headers),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("reserved handshake header")
    ));
}

#[rstest]
#[case::empty("")]
#[case::space("bad header")]
#[case::colon("x:test")]
#[case::line_break("x\r\ninjected")]
#[case::non_ascii("ümlaut")]
fn http1_request_rejects_invalid_header_names(#[case] name: &str) {
    let headers = vec![(name.to_string(), "value".to_string())];
    let error = build_request_with_headers(
        "example.com",
        "/ws",
        "dGhlIHNhbXBsZSBub25jZQ==",
        None,
        None,
        Some(&headers),
    )
    .unwrap_err();

    assert!(matches!(error, Error::InvalidHttp("invalid header name")));
}

#[rstest]
#[case::line_break("value\r\nInjected: true")]
#[case::nul("value\0suffix")]
#[case::unit_separator("value\u{1f}suffix")]
#[case::delete("value\u{7f}suffix")]
fn http1_request_rejects_invalid_header_values(#[case] value: &str) {
    let headers = vec![("X-Test".to_string(), value.to_string())];
    let error = build_request_with_headers(
        "example.com",
        "/ws",
        "dGhlIHNhbXBsZSBub25jZQ==",
        None,
        None,
        Some(&headers),
    )
    .unwrap_err();

    assert!(matches!(error, Error::InvalidHttp("invalid header value")));
}

#[tokio::test]
async fn http1_client_sends_custom_headers_and_replays_first_frame() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let request = read_http_request(&mut server_io).await;
        let request = String::from_utf8(request).unwrap();
        assert_eq!(
            extract_header(&request, "Authorization"),
            Some("Bearer token")
        );
        assert_eq!(extract_header(&request, "User-Agent"), Some("sockudo-test"));

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
        server_io.write_all(&response_and_frame).await.unwrap();
    });
    let headers = vec![
        ("Authorization".to_string(), "Bearer token".to_string()),
        ("User-Agent".to_string(), "sockudo-test".to_string()),
    ];

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (mut websocket, _) = client
        .connect_with_headers(
            client_io,
            "example.com",
            "/ws?feed=trades",
            None,
            Some(&headers),
        )
        .await
        .unwrap();

    assert!(matches!(
        websocket.next().await,
        Some(Ok(Message::Text(payload))) if payload == "hello"
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn http1_raw_client_sends_custom_headers() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let request = read_http_request(&mut server_io).await;
        let request = String::from_utf8(request).unwrap();
        assert_eq!(extract_header(&request, "X-Request-ID"), Some("request-1"));

        let key = extract_header(&request, "Sec-WebSocket-Key").unwrap();
        let accept = generate_accept_key(key);
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        server_io.write_all(response.as_bytes()).await.unwrap();
    });
    let headers = vec![("X-Request-ID".to_string(), "request-1".to_string())];

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (_, handshake) = client
        .connect_raw_with_headers(client_io, "example.com", "/ws", None, Some(&headers))
        .await
        .unwrap();

    assert_eq!(handshake.path, "/ws");
    server.await.unwrap();
}

#[tokio::test]
async fn http1_url_client_sends_custom_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /ws?feed=trades HTTP/1.1\r\n"));
        assert_eq!(extract_header(&request, "X-Request-ID"), Some("request-2"));

        let key = extract_header(&request, "Sec-WebSocket-Key").unwrap();
        let accept = generate_accept_key(key);
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let headers = vec![("X-Request-ID".to_string(), "request-2".to_string())];

    let client = WebSocketClient::<Http1>::new(Config::default());
    let (_, handshake) = client
        .connect_to_url_with_headers(&format!("ws://{addr}/ws?feed=trades"), None, Some(&headers))
        .await
        .unwrap();

    assert_eq!(handshake.path, "/ws?feed=trades");
    server.await.unwrap();
}

#[tokio::test]
async fn http1_client_rejects_invalid_headers_before_writing() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let headers = vec![("Host".to_string(), "attacker.example".to_string())];
    let client = WebSocketClient::<Http1>::new(Config::default());

    let error = match client
        .connect_with_headers(client_io, "example.com", "/ws", None, Some(&headers))
        .await
    {
        Ok(_) => panic!("reserved header should be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        Error::InvalidHttp("reserved handshake header")
    ));
    let mut received = [0u8; 1];
    assert_eq!(server_io.read(&mut received).await.unwrap(), 0);
}

#[tokio::test]
async fn http1_client_rejects_injected_host_before_writing() {
    let (client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut received = [0u8; 1];
        server_io.read(&mut received).await.unwrap()
    });
    let client = WebSocketClient::<Http1>::new(Config::default());

    let error = match client
        .connect_with_headers(
            client_io,
            "example.com\r\nX-Injected: true",
            "/ws",
            None,
            None,
        )
        .await
    {
        Ok(_) => panic!("injected host should be rejected"),
        Err(error) => error,
    };

    assert!(matches!(error, Error::InvalidHttp("invalid Host")));
    assert_eq!(server.await.unwrap(), 0);
}

#[tokio::test]
async fn http1_handshake_with_headers_rejects_missing_accept() {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let _ = read_http_request(&mut server_io).await;
        server_io
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  \r\n",
            )
            .await
            .unwrap();
    });

    let error = client_handshake_with_headers(
        &mut client_io,
        "example.com",
        "/ws",
        None,
        Some(&[("User-Agent".to_string(), "sockudo-test".to_string())]),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        Error::HandshakeFailed("missing Sec-WebSocket-Accept")
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn http1_handshake_with_headers_rejects_invalid_accept() {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let _ = read_http_request(&mut server_io).await;
        server_io
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\n\
                  Upgrade: websocket\r\n\
                  Connection: Upgrade\r\n\
                  Sec-WebSocket-Accept: invalid\r\n\
                  \r\n",
            )
            .await
            .unwrap();
    });

    let error = client_handshake_with_headers(
        &mut client_io,
        "example.com",
        "/ws",
        None,
        Some(&[("User-Agent".to_string(), "sockudo-test".to_string())]),
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        Error::HandshakeFailed("invalid Sec-WebSocket-Accept")
    ));
    server.await.unwrap();
}
