#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use sockudo_ws::http2::stream::Http2Stream;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn explicit_shutdown_delivers_data_and_ends_the_send_half() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let (received, wait_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
            let send = response
                .send_response(http::Response::new(()), false)
                .unwrap();
            let mut stream = Http2Stream::new(send, request.into_body());
            stream.write_all(b"queued response").await.unwrap();
            stream.shutdown().await.unwrap();
            // Keep the opposite receive half alive while observing END_STREAM.
            // Dropping it is a distinct operation and may reset that direction.
            wait_received.await.unwrap();
            drop(stream);
            driver.abort();
        });
        let (mut client, connection) = h2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(connection);
        let (response, _send) = client
            .send_request(
                http::Request::builder()
                    .uri("https://localhost/")
                    .body(())
                    .unwrap(),
                false,
            )
            .unwrap();
        let mut recv = response.await.unwrap().into_body();
        let mut bytes = Vec::new();
        while let Some(chunk) = recv.data().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, b"queued response");
        received.send(()).unwrap();
        server.await.unwrap();
        driver.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn abandoning_an_open_stream_keeps_the_cancel_reset() {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let (client_io, server_io) = tokio::io::duplex(65536);
        let (received, wait_received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connection = h2::server::handshake(server_io).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            let driver = tokio::spawn(async move { while connection.accept().await.is_some() {} });
            let send = response
                .send_response(http::Response::new(()), false)
                .unwrap();
            let mut stream = Http2Stream::new(send, request.into_body());
            stream.write_all(b"queued response").await.unwrap();
            drop(stream);
            // h2 schedules an implicit reset only after all stream references
            // are gone, including the handler's SendResponse.
            drop(response);
            wait_received.await.unwrap();
            driver.abort();
        });
        let (mut client, connection) = h2::client::handshake(client_io).await.unwrap();
        let driver = tokio::spawn(connection);
        let (response, mut send) = client
            .send_request(
                http::Request::builder()
                    .uri("https://localhost/")
                    .body(())
                    .unwrap(),
                false,
            )
            .unwrap();
        let _response = response.await;
        let reset = std::future::poll_fn(|cx| send.poll_reset(cx))
            .await
            .unwrap();
        assert_eq!(reset, h2::Reason::CANCEL);
        // Dropping every server stream reference causes the reset.
        received.send(()).unwrap();
        server.await.unwrap();
        driver.abort();
    })
    .await
    .unwrap();
}
