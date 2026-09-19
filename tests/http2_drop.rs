#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use bytes::Bytes;
use sockudo_ws::http2::stream::Http2Stream;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn dropping_stream_preserves_queued_response_data() {
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut connection = h2::server::handshake(server_io).await.unwrap();
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        tokio::spawn(async move { while connection.accept().await.is_some() {} });
        let send = response
            .send_response(http::Response::new(()), false)
            .unwrap();
        let mut stream = Http2Stream::new(send, request.into_body());
        stream.write_all(b"queued response").await.unwrap();
        // A handler can return immediately after its last write.
        drop(stream);
    });
    let (mut client, connection) = h2::client::handshake(client_io).await.unwrap();
    let driver = tokio::spawn(connection);
    let (response, _send) = client
        .send_request(
            http::Request::builder()
                .uri("https://localhost/")
                .body(())
                .unwrap(),
            true,
        )
        .unwrap();
    let mut recv = tokio::time::timeout(std::time::Duration::from_secs(2), response)
        .await
        .expect("response headers were discarded")
        .unwrap()
        .into_body();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), recv.data())
            .await
            .expect("response data or END_STREAM was discarded")
            .unwrap()
            .unwrap(),
        Bytes::from_static(b"queued response")
    );
    while let Some(chunk) = tokio::time::timeout(std::time::Duration::from_secs(2), recv.data())
        .await
        .expect("END_STREAM was discarded")
    {
        assert!(chunk.expect("stream reset after queued DATA").is_empty());
    }
    driver.abort();
    server.abort();
}
