#![cfg(feature = "tokio-runtime")]

mod support;

use sockudo_ws::handshake::{
    HandshakeSelection, build_request, client_handshake, generate_accept_key, parse_request,
    parse_response, server_handshake, server_handshake_with,
};
use sockudo_ws::{Config, Error, Http1};
use support::{extract_header, read_http_request};
use tokio::io::AsyncWriteExt;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn request_with(upgrade: &str, connection: &str) -> Vec<u8> {
    format!(
        "GET /ws HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: {upgrade}\r\n\
         Connection: {connection}\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .into_bytes()
}

#[test]
fn request_requires_exact_upgrade_and_connection_tokens() {
    for request in [
        request_with("notwebsocket", "Upgrade"),
        request_with("websocket", "keep-alive, x-upgrade"),
    ] {
        assert!(matches!(
            parse_request(&request),
            Err(Error::HandshakeFailed(
                "missing Upgrade: websocket" | "missing Connection: Upgrade"
            ))
        ));
    }
}

#[test]
fn request_requires_http_11_host_and_a_16_byte_key() {
    let http_10 = request_with("websocket", "Upgrade")
        .windows(8)
        .position(|window| window == b"HTTP/1.1")
        .map(|offset| {
            let mut request = request_with("websocket", "Upgrade");
            request[offset + 7] = b'0';
            request
        })
        .unwrap();
    assert!(matches!(
        parse_request(&http_10),
        Err(Error::InvalidHttp("HTTP version must be 1.1"))
    ));

    let without_host = request_with("websocket", "Upgrade")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.starts_with(b"Host:"))
        .flat_map(|line| line.iter().copied().chain(std::iter::once(b'\n')))
        .collect::<Vec<_>>();
    assert!(matches!(
        parse_request(&without_host),
        Err(Error::HandshakeFailed("missing Host"))
    ));

    let invalid_key = request_with("websocket", "Upgrade")
        .windows(KEY.len())
        .position(|window| window == KEY.as_bytes())
        .map(|offset| {
            let mut request = request_with("websocket", "Upgrade");
            request.splice(offset..offset + KEY.len(), b"YWJjZA==".iter().copied());
            request
        })
        .unwrap();
    assert!(matches!(
        parse_request(&invalid_key),
        Err(Error::HandshakeFailed("invalid Sec-WebSocket-Key"))
    ));
}

#[test]
fn request_combines_repeatable_websocket_headers() {
    let request = format!(
        "GET /ws HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: chat\r\n\
         Sec-WebSocket-Protocol: superchat\r\n\
         Sec-WebSocket-Extensions: extension-one\r\n\
         Sec-WebSocket-Extensions: extension-two; mode=fast\r\n\
         \r\n"
    );

    let (request, _) = parse_request(request.as_bytes()).unwrap().unwrap();
    assert_eq!(request.protocol.as_deref(), Some("chat, superchat"));
    assert_eq!(
        request.extensions.as_deref(),
        Some("extension-one, extension-two; mode=fast")
    );
}

#[test]
fn request_accepts_absolute_form_and_derives_resource_name() {
    let request = format!(
        "GET http://example.com/chat?room=one HTTP/1.1\r\n\
         Host: proxy.example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );

    let (request, _) = parse_request(request.as_bytes()).unwrap().unwrap();
    assert_eq!(request.path, "/chat?room=one");
    assert_eq!(request.host, Some("example.com"));
}

#[test]
fn request_normalizes_empty_and_query_only_absolute_paths() {
    for (target, expected_path) in [
        ("https://example.com", "/"),
        ("https://example.com?room=one", "/?room=one"),
    ] {
        let request = format!(
            "GET {target} HTTP/1.1\r\n\
             Host: proxy.example.com\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );

        let (request, _) = parse_request(request.as_bytes()).unwrap().unwrap();
        assert_eq!(request.path, expected_path);
        assert_eq!(request.host, Some("example.com"));
    }
}

#[test]
fn request_rejects_websocket_schemes_in_absolute_form() {
    for scheme in ["ws", "wss"] {
        let request = format!(
            "GET {scheme}://example.com/chat HTTP/1.1\r\n\
             Host: example.com\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
        );

        assert!(parse_request(request.as_bytes()).is_err());
    }
}

#[test]
fn request_rejects_websocket_upgrade_body_framing() {
    let with_header = |header: &str| {
        format!(
            "GET /ws HTTP/1.1\r\n\
             Host: example.com\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {KEY}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             {header}\r\n\
             \r\n"
        )
    };

    assert!(parse_request(with_header("Content-Length: 0").as_bytes()).is_ok());
    assert!(
        parse_request(with_header("Content-Length: 0\r\nContent-Length: 0").as_bytes()).is_ok()
    );
    assert!(parse_request(with_header("Content-Length: 0, 0").as_bytes()).is_ok());

    for header in [
        "Content-Length: 1",
        "Content-Length: 0, 1",
        "Transfer-Encoding: chunked",
    ] {
        assert!(
            parse_request(with_header(header).as_bytes()).is_err(),
            "unexpectedly accepted {header}"
        );
    }
}

#[test]
fn request_builder_rejects_invalid_handshake_fields() {
    for error in [
        build_request("example.com\r\nX: injected", "/ws", KEY, None, None).unwrap_err(),
        build_request("example.com", "/ws\r\nX: injected", KEY, None, None).unwrap_err(),
        build_request("example.com", "/ws", KEY, Some("chat\r\nX: injected"), None).unwrap_err(),
        build_request(
            "example.com",
            "/ws",
            KEY,
            None,
            Some("permessage-deflate\r\nX: injected"),
        )
        .unwrap_err(),
    ] {
        assert!(matches!(error, Error::InvalidHttp(_)));
    }
}

#[test]
fn response_builder_validates_quoted_extension_values() {
    for extension in ["extension; mode=\"fast\"", "extension; mode=\"f\\ast\""] {
        assert!(
            sockudo_ws::handshake::build_response(
                "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
                None,
                Some(extension),
            )
            .is_ok(),
            "unexpectedly rejected {extension}"
        );
    }

    for extension in [
        "extension; mode=\"fast mode\"",
        "extension; mode=\"fast\\\"",
    ] {
        assert!(
            sockudo_ws::handshake::build_response(
                "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=",
                None,
                Some(extension),
            )
            .is_err(),
            "unexpectedly accepted {extension}"
        );
    }
}

#[test]
fn response_requires_http_11_and_exact_upgrade_tokens() {
    for response in [
        b"HTTP/1.0 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
            .as_slice(),
        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n".as_slice(),
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".as_slice(),
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: notwebsocket\r\nConnection: Upgrade\r\n\r\n"
            .as_slice(),
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: x-upgrade\r\n\r\n"
            .as_slice(),
    ] {
        assert!(parse_response(response).is_err());
    }
}

#[test]
fn response_rejects_multiple_selected_subprotocols() {
    let response = b"HTTP/1.1 101 Switching Protocols\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\
        Sec-WebSocket-Protocol: chat, superchat\r\n\
        \r\n";

    assert!(matches!(
        parse_response(response),
        Err(Error::HandshakeFailed("invalid Sec-WebSocket-Protocol"))
    ));
}

async fn client_error_for_response(protocol: Option<&str>, response_headers: &str) -> Error {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let response_headers = response_headers.to_string();
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
             {response_headers}\
             \r\n"
        );
        server_io.write_all(response.as_bytes()).await.unwrap();
    });

    let error = client_handshake(&mut client_io, "example.com", "/ws", protocol)
        .await
        .unwrap_err();
    server.await.unwrap();
    error
}

#[tokio::test]
async fn client_rejects_unoffered_subprotocol_and_extension() {
    let error = client_error_for_response(Some("chat"), "Sec-WebSocket-Protocol: other\r\n").await;
    assert!(matches!(
        error,
        Error::HandshakeFailed("server returned an unoffered subprotocol")
    ));

    let error =
        client_error_for_response(None, "Sec-WebSocket-Extensions: permessage-deflate\r\n").await;
    assert!(matches!(
        error,
        Error::HandshakeFailed("server returned an unoffered extension")
    ));
}

#[tokio::test]
async fn default_server_does_not_echo_offered_subprotocols() {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move { server_handshake(&mut server_io).await.unwrap() });
    let request = build_request("example.com", "/ws", KEY, Some("chat, superchat"), None).unwrap();
    client_io.write_all(&request).await.unwrap();

    let response = read_http_request(&mut client_io).await;
    let response = String::from_utf8(response).unwrap();
    assert!(!response.contains("Sec-WebSocket-Protocol"));

    let handshake = server.await.unwrap();
    assert!(handshake.protocol.is_none());
}

#[tokio::test]
async fn request_aware_server_selects_an_offered_subprotocol() {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        server_handshake_with(&mut server_io, |request| {
            assert_eq!(request.protocol.as_deref(), Some("chat, superchat"));
            Ok(HandshakeSelection {
                protocol: Some("superchat".to_string()),
                extensions: None,
            })
        })
        .await
        .unwrap()
    });

    let client = client_handshake(
        &mut client_io,
        "example.com",
        "/ws",
        Some("chat, superchat"),
    )
    .await
    .unwrap();
    let server = server.await.unwrap();

    assert_eq!(client.protocol.as_deref(), Some("superchat"));
    assert_eq!(server.protocol.as_deref(), Some("superchat"));
}

#[tokio::test]
async fn request_aware_server_rejects_unoffered_selections() {
    for selection in [
        HandshakeSelection {
            protocol: Some("other".to_string()),
            extensions: None,
        },
        HandshakeSelection {
            protocol: None,
            extensions: Some("other-extension".to_string()),
        },
    ] {
        let (mut client_io, mut server_io) = tokio::io::duplex(4096);
        let request = build_request(
            "example.com",
            "/ws",
            KEY,
            Some("chat"),
            Some("permessage-deflate"),
        )
        .unwrap();
        client_io.write_all(&request).await.unwrap();

        assert!(
            server_handshake_with(&mut server_io, |_| Ok(selection))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn configured_server_negotiates_with_tungstenite() {
    use sockudo_ws::server::WebSocketServer;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (client_io, server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let server = WebSocketServer::<Http1>::new(Config::default())
            .protocols(["superchat", "chat"])
            .unwrap();
        let (_websocket, handshake) = server.accept_raw(server_io).await.unwrap();
        handshake
    });

    let mut request = "ws://example.com/ws".into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "chat, superchat".parse().unwrap());
    let (_websocket, response) = tokio_tungstenite::client_async(request, client_io)
        .await
        .unwrap();
    let handshake = server.await.unwrap();

    assert_eq!(
        response.headers().get("sec-websocket-protocol").unwrap(),
        "superchat"
    );
    assert_eq!(handshake.protocol.as_deref(), Some("superchat"));
}

#[test]
fn configured_server_rejects_invalid_subprotocols() {
    use sockudo_ws::server::WebSocketServer;

    assert!(
        WebSocketServer::<Http1>::new(Config::default())
            .protocols(["not a token"])
            .is_err()
    );
    assert!(
        WebSocketServer::<Http1>::new(Config::default())
            .protocols(["chat", "chat"])
            .is_ok()
    );
}

#[test]
fn request_limit_excludes_upgraded_frame_bytes() {
    let mut input = request_with("websocket", "Upgrade");
    let header_len = input.len();
    input.extend_from_slice(&vec![0; 16 * 1024]);
    assert_eq!(parse_request(&input).unwrap().unwrap().1, header_len);
}

#[test]
fn response_limit_excludes_upgraded_frame_bytes() {
    let mut input =
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
            .to_vec();
    let header_len = input.len();
    input.extend_from_slice(&vec![0; 16 * 1024]);
    assert_eq!(parse_response(&input).unwrap().unwrap().1, header_len);
}

#[test]
fn oversized_complete_and_partial_headers_are_rejected() {
    for suffix in ["", "\r\n\r\n"] {
        let request = format!("GET / HTTP/1.1\r\nX-Large: {}{suffix}", "x".repeat(8192));
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nX-Large: {}{suffix}",
            "x".repeat(8192)
        );
        assert!(matches!(
            parse_request(request.as_bytes()),
            Err(Error::InvalidHttp("request too large"))
        ));
        assert!(matches!(
            parse_response(response.as_bytes()),
            Err(Error::InvalidHttp("response too large"))
        ));
    }
}
