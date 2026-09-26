use rstest::rstest;
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

#[rstest]
#[case::protocol_with_space("Sec-WebSocket-Protocol: chat, invalid protocol")]
#[case::protocol_with_non_ascii_whitespace("Sec-WebSocket-Protocol: chat,\u{00a0}superchat")]
#[case::duplicate_protocol("Sec-WebSocket-Protocol: chat, chat")]
#[case::empty_protocols("Sec-WebSocket-Protocol: , ,")]
#[case::quoted_extension_with_space(
    "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast mode\""
)]
#[case::extension_with_non_ascii_whitespace(
    "Sec-WebSocket-Extensions: permessage-deflate;\u{00a0}mode=fast"
)]
#[case::extension_with_unclosed_quote(
    "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast\\\""
)]
#[case::extension_without_name("Sec-WebSocket-Extensions: ; mode=fast")]
fn request_parser_rejects_invalid_optional_header_grammar(#[case] header: &str) {
    assert!(
        matches!(
            parse_request(&request_with(header)),
            Err(Error::HandshakeFailed(_))
        ),
        "unexpected result for {header}"
    );
}

#[rstest]
#[case::multiple_protocols("Sec-WebSocket-Protocol: chat, superchat")]
#[case::protocol_with_space("Sec-WebSocket-Protocol: invalid protocol")]
#[case::quoted_extension_with_space(
    "Sec-WebSocket-Extensions: permessage-deflate; mode=\"fast mode\""
)]
#[case::extension_parameter_without_name("Sec-WebSocket-Extensions: permessage-deflate; =fast")]
fn response_parser_rejects_invalid_optional_header_grammar(#[case] header: &str) {
    assert!(
        matches!(
            parse_response(&response_with(header)),
            Err(Error::HandshakeFailed(_))
        ),
        "unexpected result for {header}"
    );
}

#[rstest]
#[case::protocol_with_space("chat, invalid protocol")]
#[case::protocol_with_non_ascii_whitespace("chat,\u{00a0}superchat")]
#[case::duplicate_protocol("chat, chat")]
#[case::empty_protocol_between_commas("chat,,superchat")]
#[case::empty_protocols(", ,")]
fn checked_request_builder_rejects_invalid_protocol_grammar(#[case] protocol: &str) {
    assert!(
        matches!(
            build_request_with_headers("example.com", "/chat", KEY, Some(protocol), None, None,),
            Err(Error::InvalidHttp(_))
        ),
        "unexpected result for {protocol}"
    );
}

#[rstest]
#[case::quoted_value_with_space("permessage-deflate; mode=\"fast mode\"")]
#[case::non_ascii_whitespace("permessage-deflate;\u{00a0}mode=fast")]
#[case::empty_extension_between_commas("permessage-deflate,,x-example")]
fn checked_request_builder_rejects_invalid_extension_grammar(#[case] extensions: &str) {
    assert!(
        matches!(
            build_request_with_headers("example.com", "/chat", KEY, None, Some(extensions), None,),
            Err(Error::InvalidHttp(_))
        ),
        "unexpected result for {extensions}"
    );
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
#[rstest]
#[case::none_offered(None, None)]
#[case::single_protocol(Some("chat"), Some("chat"))]
#[case::selects_first_offered_protocol(Some("chat, superchat"), Some("chat"))]
#[tokio::test]
async fn default_tokio_server_negotiates_expected_protocol(
    #[case] offered: Option<&str>,
    #[case] expected: Option<&str>,
) {
    let expected = expected.map(str::to_owned);

    assert_eq!(
        default_tokio_server_round_trip(offered).await,
        (expected.clone(), expected)
    );
}
