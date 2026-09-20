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
