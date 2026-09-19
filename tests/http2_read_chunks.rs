#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use bytes::Bytes;
use sockudo_ws::http2::stream::Http2Stream;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn small_reads_preserve_h2_data_and_buffered_remainder() {
    check_buffered_remainder(false).await;
}

#[tokio::test]
async fn generic_transport_preserves_h2_data_and_buffered_remainder() {
    check_buffered_remainder(true).await;
}

async fn check_buffered_remainder(generic_transport: bool) {
    // Exceed the initial flow-control window so progress requires returned capacity.
    let expected = (0..128 * 1024 + 1)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let payload = expected.clone();
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut connection = h2::server::handshake(server_io).await.unwrap();
        let (_, mut response) = connection.accept().await.unwrap().unwrap();
        let mut send = response
            .send_response(http::Response::new(()), false)
            .unwrap();
        send.send_data(Bytes::from(payload), true).unwrap();
        // Continue driving the connection until the client has consumed the DATA.
        while connection.accept().await.is_some() {}
    });
    let (mut client, connection) = h2::client::handshake(client_io).await.unwrap();
    let driver = tokio::spawn(connection);
    let (response, send) = client
        .send_request(
            http::Request::builder()
                .uri("https://localhost/")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let recv = response.await.unwrap().into_body();
    let mut stream: Box<dyn tokio::io::AsyncRead + Unpin + Send> = if generic_transport {
        Box::new(sockudo_ws::Stream::<sockudo_ws::Http2>::from_h2(send, recv))
    } else {
        Box::new(Http2Stream::new(send, recv))
    };
    let mut actual = Vec::new();
    let mut chunk = [0; 7];
    loop {
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        actual.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(actual, expected);
    drop(stream);
    drop(client);
    driver.abort();
    server.abort();
}
