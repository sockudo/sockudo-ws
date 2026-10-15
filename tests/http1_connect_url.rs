#![cfg(feature = "tokio-runtime")]

mod support;

use std::time::Duration;

use rstest::rstest;
use sockudo_ws::{Config, Error, Http1, client::WebSocketClient, handshake::generate_accept_key};
use support::{extract_header, read_http_request};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// Accepts one upgrade and returns its request line and Host header.
async fn accept_upgrade(listener: TcpListener) -> (String, String) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = String::from_utf8(read_http_request(&mut stream).await).unwrap();
    let key = extract_header(&request, "Sec-WebSocket-Key").unwrap();
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        generate_accept_key(key)
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    let request_line = request.lines().next().unwrap().to_owned();
    let host = extract_header(&request, "Host").unwrap().to_owned();
    (request_line, host)
}

#[rstest]
#[case::path("127.0.0.1:0", "ws://{addr}/ws", "GET /ws HTTP/1.1")]
#[case::no_path("127.0.0.1:0", "ws://{addr}", "GET / HTTP/1.1")]
#[case::query_without_path("127.0.0.1:0", "ws://{addr}?feed=1", "GET /?feed=1 HTTP/1.1")]
#[case::uppercase_scheme("127.0.0.1:0", "WS://{addr}/a?b", "GET /a?b HTTP/1.1")]
#[case::ipv6("[::1]:0", "ws://{addr}/x", "GET /x HTTP/1.1")]
#[tokio::test]
async fn url_sets_address_target_and_host(
    #[case] bind: &str,
    #[case] url: &str,
    #[case] request_line: &str,
) {
    let listener = TcpListener::bind(bind).await.unwrap();
    // `SocketAddr` formats IPv6 with brackets, as URLs require.
    let addr = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(accept_upgrade(listener));

    let client = WebSocketClient::<Http1>::new(Config::default());
    tokio::time::timeout(
        Duration::from_secs(5),
        client.connect_to_url(&url.replace("{addr}", &addr), None),
    )
    .await
    .unwrap()
    .unwrap();

    let (received_line, host) = server.await.unwrap();
    assert_eq!(received_line, request_line);
    assert_eq!(host, addr, "Host must keep the URL's non-default port");
}

#[rstest]
#[case::wss("127.0.0.1:0", "wss://{addr}/ws")]
#[case::uppercase_wss("127.0.0.1:0", "WSS://{addr}/ws")]
#[case::other_scheme("127.0.0.1:0", "http://{addr}/ws")]
#[case::user_information("127.0.0.1:0", "ws://user:secret@{addr}/ws")]
#[case::fragment("127.0.0.1:0", "ws://{addr}/ws#part")]
#[case::empty_port("127.0.0.1:0", "ws://127.0.0.1:/ws")]
#[case::non_numeric_port("127.0.0.1:0", "ws://127.0.0.1:http/ws")]
#[case::port_out_of_range("127.0.0.1:0", "ws://127.0.0.1:65536/ws")]
#[case::signed_port("127.0.0.1:0", "ws://127.0.0.1:+{port}/ws")]
#[case::text_after_ipv6_host("[::1]:0", "ws://[::1]evil:{port}/ws")]
#[tokio::test]
async fn rejected_url_opens_no_connection(#[case] bind: &str, #[case] url: &str) {
    let listener = TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = url
        .replace("{addr}", &addr.to_string())
        .replace("{port}", &addr.port().to_string());

    let client = WebSocketClient::<Http1>::new(Config::default());
    // A URL that is not rejected would wait for a handshake response forever.
    let result = tokio::time::timeout(Duration::from_secs(5), client.connect_to_url(&url, None))
        .await
        .expect("a rejected URL must fail before connecting");

    assert!(matches!(result, Err(Error::HandshakeFailed(_))));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "a rejected URL must not reach the network"
    );
}
