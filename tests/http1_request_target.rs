use sockudo_ws::handshake::parse_request;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn request_with(target: &str, host: &str) -> Vec<u8> {
    format!(
        "GET {target} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .into_bytes()
}

#[test]
fn request_accepts_origin_form() {
    let input = request_with("/chat/room%20one?format=json", "example.com");
    let (request, _) = parse_request(&input).unwrap().unwrap();

    assert_eq!(request.path, "/chat/room%20one?format=json");
    assert_eq!(request.host, Some("example.com"));
}

#[test]
fn request_accepts_absolute_form_and_uses_its_authority() {
    for (target, expected_path, expected_host) in [
        (
            "http://example.com/chat?room=one",
            "/chat?room=one",
            "example.com",
        ),
        ("HTTPS://example.com", "/", "example.com"),
        ("https://example.com?room=one", "/?room=one", "example.com"),
        (
            "http://[2001:db8::1]:8080/chat",
            "/chat",
            "[2001:db8::1]:8080",
        ),
        ("http://example.com:/chat", "/chat", "example.com:"),
    ] {
        let input = request_with(target, "proxy.example.com");
        let (request, _) = parse_request(&input).unwrap().unwrap();

        assert_eq!(request.path, expected_path, "target: {target}");
        assert_eq!(request.host, Some(expected_host), "target: {target}");
    }
}

#[test]
fn request_rejects_unsupported_or_malformed_targets() {
    for target in [
        "chat",
        "*",
        "example.com:80",
        "ws://example.com/chat",
        "wss://example.com/chat",
        "http:///chat",
        "http://user@example.com/chat",
        "http://example.com:port/chat",
        "http://[2001:db8::1/chat",
        "http://example.com/chat#fragment",
        "/chat#fragment",
        "/chat/%",
        "/chat/%zz",
        "/chat?value=%",
        "http://example.com?value=%zz",
    ] {
        assert!(
            parse_request(&request_with(target, "example.com")).is_err(),
            "unexpectedly accepted {target}"
        );
    }
}

#[test]
fn query_only_absolute_target_rejects_fragment() {
    assert!(
        parse_request(&request_with(
            "http://example.com?room=one#fragment",
            "example.com"
        ))
        .is_err()
    );
}

#[rstest::rstest]
#[case("/chat?room=one", "/chat?room=one", false)]
#[case("http://example.com/chat?room=one", "/chat?room=one", false)]
#[case("http://example.com", "/", false)]
#[case("http://example.com?room=one", "/?room=one", true)]
fn normalized_path_only_owns_synthesized_query(
    #[case] target: &str,
    #[case] expected: &str,
    #[case] owned: bool,
) {
    let input = request_with(target, "example.com");
    let (request, _) = parse_request(&input).unwrap().unwrap();
    assert_eq!(request.path, expected);
    assert_eq!(matches!(request.path, std::borrow::Cow::Owned(_)), owned);
}

#[test]
fn absolute_target_still_requires_host_header() {
    let input = String::from_utf8(request_with("http://example.com/chat", "proxy.example.com"))
        .unwrap()
        .replace("Host: proxy.example.com\r\n", "");
    assert!(parse_request(input.as_bytes()).is_err());
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn server_normalizes_absolute_target_and_preserves_first_frame() {
    use futures_util::StreamExt;
    use sockudo_ws::{Config, Http1, server::WebSocketServer};
    use tokio::io::AsyncWriteExt;
    let (mut client, server) = tokio::io::duplex(4096);
    let mut input = request_with("http://example.com?room=one", "proxy.example.com");
    input.extend_from_slice(b"\x81\x82\x00\x00\x00\x00ok");
    client.write_all(&input).await.unwrap();
    let (mut ws, handshake) = WebSocketServer::<Http1>::new(Config::default())
        .accept(server)
        .await
        .unwrap();
    assert_eq!(handshake.path, "/?room=one");
    assert_eq!(ws.next().await.unwrap().unwrap().as_bytes(), b"ok");
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn server_rejects_invalid_target_before_upgrade() {
    use sockudo_ws::{Config, Http1, server::WebSocketServer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut client, server) = tokio::io::duplex(4096);
    client
        .write_all(&request_with("*", "example.com"))
        .await
        .unwrap();
    assert!(
        WebSocketServer::<Http1>::new(Config::default())
            .accept(server)
            .await
            .is_err()
    );
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert!(!response.starts_with(b"HTTP/1.1 101"));
}

#[cfg(feature = "compio-runtime")]
#[compio::test]
async fn compio_server_normalizes_absolute_target_and_preserves_first_frame() {
    use compio::{
        io::AsyncWriteExt,
        net::{TcpListener, TcpStream},
    };
    use sockudo_ws::{CompioWebSocketStream, Config, compio::server_handshake};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut server, _) = listener.accept().await.unwrap();
    let mut input = request_with("http://example.com?room=one", "proxy.example.com");
    input.extend_from_slice(b"\x81\x82\x00\x00\x00\x00ok");
    client.write_all(input).await.0.unwrap();
    let handshake = server_handshake(&mut server).await.unwrap();
    assert_eq!(handshake.path, "/?room=one");
    let mut ws =
        CompioWebSocketStream::server_with_leftover(server, Config::default(), handshake.leftover);
    let message = compio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(message.as_bytes(), b"ok");
}

#[rstest::rstest]
#[case("http://:80/chat")]
#[case("https://:443/chat")]
fn absolute_target_rejects_empty_host(#[case] target: &str) {
    assert!(parse_request(&request_with(target, "example.com")).is_err());
}

#[rstest::rstest]
#[case(r#"/chat/{"a":1}"#)]
#[case("/行情?市场=现货")]
fn request_preserves_compatible_path_characters(#[case] target: &str) {
    let input = request_with(target, "example.com");
    let (request, _) = parse_request(&input).unwrap().unwrap();
    assert_eq!(request.path, target);
}

#[test]
fn request_rejects_bare_percent_in_query() {
    assert!(parse_request(&request_with("/chat?discount=10%", "example.com")).is_err());
}
