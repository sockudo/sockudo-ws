#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use bytes::Bytes;
use sockudo_ws::http2::stream::Http2Stream;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn small_reads_preserve_h2_data_and_buffered_remainder() {
    let expected = (0..4097).map(|i| (i % 251) as u8).collect::<Vec<_>>();
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
    let mut stream = Http2Stream::new(send, response.await.unwrap().into_body());
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
