use rstest::rstest;
use sockudo_ws::Error;
use sockudo_ws::handshake::{build_request_with_headers, parse_request, parse_response};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

fn request(version: &str, host: Option<&str>, key: &str) -> Vec<u8> {
    let host = host
        .map(|value| format!("Host: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "GET /ws HTTP/{version}\r\n\
         {host}\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .into_bytes()
}

#[test]
fn request_requires_http_11() {
    let error = parse_request(&request("1.0", Some("example.com"), KEY)).unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("HTTP version must be 1.1")
    ));
}

#[rstest]
#[case::missing(None)]
#[case::empty(Some(""))]
fn request_requires_host(#[case] host: Option<&str>) {
    let error = parse_request(&request("1.1", host, KEY)).unwrap_err();

    assert!(matches!(error, Error::HandshakeFailed("missing Host")));
}

#[rstest]
#[case::not_base64("not base64")]
#[case::too_short("YWJjZA==")]
#[case::too_long("YWJjZGVmZ2hpamtsbW5vcHE=")]
fn request_requires_a_base64_encoded_16_byte_key(#[case] key: &str) {
    let error = parse_request(&request("1.1", Some("example.com"), key)).unwrap_err();

    assert!(matches!(
        error,
        Error::HandshakeFailed("invalid Sec-WebSocket-Key")
    ));
}

#[test]
fn request_accepts_ows_around_required_header_values() {
    let request = b"GET /ws HTTP/1.1\r\n\
        Host:\t example.com \t\r\n\
        Upgrade: WebSocket\r\n\
        Connection: keep-alive, UpGrAdE\r\n\
        Sec-WebSocket-Key:\t dGhlIHNhbXBsZSBub25jZQ== \t\r\n\
        Sec-WebSocket-Version:\t 13 \t\r\n\
        \r\n";

    let (request, _) = parse_request(request).unwrap().unwrap();

    assert_eq!(request.host, Some("example.com"));
    assert_eq!(request.key, KEY);
    assert_eq!(request.version, "13");
}

#[test]
fn request_rejects_non_ascii_whitespace_around_upgrade_token() {
    let request = format!(
        "GET /ws HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: \u{00a0}websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );

    assert!(matches!(
        parse_request(request.as_bytes()),
        Err(Error::HandshakeFailed("missing Upgrade: websocket"))
    ));
}

#[rstest]
#[case::empty_host("", KEY)]
#[case::invalid_key("example.com", "not base64")]
fn checked_request_builder_rejects_unparseable_required_fields(
    #[case] host: &str,
    #[case] key: &str,
) {
    assert!(build_request_with_headers(host, "/ws", key, None, None, None).is_err());
}

#[test]
fn response_requires_http_11() {
    let response = b"HTTP/1.0 101 Switching Protocols\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\
        \r\n";

    let error = parse_response(response).unwrap_err();

    assert!(matches!(
        error,
        Error::InvalidHttp("HTTP version must be 1.1")
    ));
}

#[test]
fn response_reports_non_switching_status_before_http_version() {
    let response = b"HTTP/1.0 404 Not Found\r\n\r\n";

    assert!(matches!(
        parse_response(response),
        Err(Error::HandshakeFailed("expected 101 Switching Protocols"))
    ));
}

#[rstest]
#[case::missing_upgrade(
    "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\r\n",
    "missing Upgrade: websocket"
)]
#[case::missing_connection(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n",
    "missing Connection: Upgrade"
)]
#[case::wrong_upgrade(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: notwebsocket\r\nConnection: Upgrade\r\n\r\n",
    "missing Upgrade: websocket"
)]
#[case::wrong_connection(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: keep-alive\r\n\r\n",
    "missing Connection: Upgrade"
)]
fn response_requires_upgrade_and_connection_tokens(#[case] response: &str, #[case] expected: &str) {
    assert!(matches!(
        parse_response(response.as_bytes()),
        Err(Error::HandshakeFailed(reason)) if reason == expected
    ));
}

#[test]
fn response_accepts_case_insensitive_upgrade_and_connection_token_list() {
    let response = b"HTTP/1.1 101 Switching Protocols\r\n\
        Upgrade:\t WebSocket \t\r\n\
        Connection: keep-alive, UpGrAdE\r\n\
        \r\n";

    assert!(parse_response(response).unwrap().is_some());
}

#[rstest]
#[case::protocol_list(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: h2c, WebSocket\r\nConnection: Upgrade\r\n\r\n"
)]
#[case::mixed_fields(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nUpgrade: h2c\r\nConnection: Upgrade\r\n\r\n"
)]
fn response_rejects_upgrade_protocol_list(#[case] response: &str) {
    assert!(matches!(
        parse_response(response.as_bytes()),
        Err(Error::HandshakeFailed("missing Upgrade: websocket"))
    ));
}

#[test]
fn response_rejects_non_ascii_whitespace_around_connection_token() {
    let response = "HTTP/1.1 101 Switching Protocols\r\n\
        Upgrade: websocket\r\n\
        Connection: \u{00a0}Upgrade\r\n\
        \r\n";

    assert!(matches!(
        parse_response(response.as_bytes()),
        Err(Error::HandshakeFailed("missing Connection: Upgrade"))
    ));
}
