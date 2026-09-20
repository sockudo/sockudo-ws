use sockudo_ws::Error;
use sockudo_ws::handshake::{build_request_with_headers, parse_request, parse_response};

#[cfg(feature = "tokio-runtime")]
use sockudo_ws::handshake::{client_handshake, server_handshake};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

fn request_with(header: &str) -> Vec<u8> {
    format!(
        "GET /chat HTTP/1.1\r\n\
         Host: example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {header}\r\n\
         \r\n"
    )
    .into_bytes()
}

fn response_with(header: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {ACCEPT}\r\n\
         {header}\r\n\
         \r\n"
    )
    .into_bytes()
}

#[test]
fn parsers_accept_valid_optional_header_grammar() {
    let request = request_with(
        "Sec-WebSocket-Protocol: chat,, superchat,\r\n\
         Sec-WebSocket-Extensions: , permessage-deflate; mode=fast; escaped=\"f\\ast\",",
    );
    assert!(parse_request(&request).unwrap().is_some());

    let response = response_with(
        "Sec-WebSocket-Protocol: chat\r\n\
         Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast\"",
    );
    assert!(parse_response(&response).unwrap().is_some());
}

#[test]
fn request_parser_rejects_invalid_optional_header_grammar() {
    for header in [
        "Sec-WebSocket-Protocol: chat, invalid protocol",
        "Sec-WebSocket-Protocol: chat,\u{00a0}superchat",
        "Sec-WebSocket-Protocol: chat, chat",
        "Sec-WebSocket-Protocol: , ,",
        "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast mode\"",
        "Sec-WebSocket-Extensions: permessage-deflate;\u{00a0}mode=fast",
        "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast\\\"",
        "Sec-WebSocket-Extensions: ; mode=fast",
    ] {
        assert!(
            matches!(
                parse_request(&request_with(header)),
                Err(Error::HandshakeFailed(_))
            ),
            "unexpected result for {header}"
        );
    }
}

#[test]
fn response_parser_rejects_invalid_optional_header_grammar() {
    for header in [
        "Sec-WebSocket-Protocol: chat, superchat",
        "Sec-WebSocket-Protocol: invalid protocol",
        "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast mode\"",
        "Sec-WebSocket-Extensions: permessage-deflate; =fast",
    ] {
        assert!(
            matches!(
                parse_response(&response_with(header)),
                Err(Error::HandshakeFailed(_))
            ),
            "unexpected result for {header}"
        );
    }
}

#[test]
fn checked_request_builder_rejects_invalid_optional_header_grammar() {
    for protocol in [
        "chat, invalid protocol",
        "chat,\u{00a0}superchat",
        "chat, chat",
        "chat,,superchat",
        ", ,",
    ] {
        assert!(
            matches!(
                build_request_with_headers("example.com", "/chat", KEY, Some(protocol), None, None,),
                Err(Error::InvalidHttp(_))
            ),
            "unexpected result for {protocol}"
        );
    }

    for extensions in [
        "permessage-deflate; mode=\"fast mode\"",
        "permessage-deflate;\u{00a0}mode=fast",
        "permessage-deflate,,x-example",
    ] {
        assert!(
            matches!(
                build_request_with_headers(
                    "example.com",
                    "/chat",
                    KEY,
                    None,
                    Some(extensions),
                    None,
                ),
                Err(Error::InvalidHttp(_))
            ),
            "unexpected result for {extensions}"
        );
    }
}

#[test]
fn checked_request_builder_accepts_valid_optional_header_grammar() {
    assert!(
        build_request_with_headers(
            "example.com",
            "/chat",
            KEY,
            Some("chat, superchat"),
            Some("permessage-deflate; mode=\"fast\"; escaped=\"f\\ast\""),
            None,
        )
        .is_ok()
    );
}

#[cfg(feature = "tokio-runtime")]
async fn default_tokio_server_round_trip(
    protocol: Option<&str>,
) -> (Option<String>, Option<String>) {
    let (mut client_io, mut server_io) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move { server_handshake(&mut server_io).await });

    let client_result = client_handshake(&mut client_io, "example.com", "/chat", protocol)
        .await
        .unwrap();
    let server_result = server.await.unwrap().unwrap();

    (client_result.protocol, server_result.protocol)
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn default_tokio_server_omits_protocol_when_none_is_offered() {
    assert_eq!(default_tokio_server_round_trip(None).await, (None, None));
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn default_tokio_server_preserves_single_protocol_negotiation() {
    assert_eq!(
        default_tokio_server_round_trip(Some("chat")).await,
        (Some("chat".to_owned()), Some("chat".to_owned()))
    );
}

#[cfg(feature = "tokio-runtime")]
#[tokio::test]
async fn default_tokio_server_selects_the_first_offered_protocol() {
    assert_eq!(
        default_tokio_server_round_trip(Some("chat, superchat")).await,
        (Some("chat".to_owned()), Some("chat".to_owned()))
    );
}
