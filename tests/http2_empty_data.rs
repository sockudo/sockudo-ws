#![cfg(all(feature = "tokio-runtime", feature = "http2"))]

use bytes::Bytes;
use rstest::rstest;
use sockudo_ws::http2::stream::Http2Stream;
use tokio::io::AsyncReadExt;

#[rstest]
#[case(false, 7)]
#[case(true, 7)]
#[case(false, 65536)]
#[case(true, 65536)]
#[tokio::test]
async fn empty_data_does_not_end_reads_before_payload(
    #[case] generic_transport: bool,
    #[case] read_size: usize,
) {
    // Exceed the initial flow-control window so progress requires returned capacity.
    let expected = (0..128 * 1024 + 1)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let payload = expected.clone();
    let (ready, receiver) = tokio::sync::oneshot::channel();
    let (client_io, server_io) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut connection = h2::server::handshake(server_io).await.unwrap();
        let (_, mut response) = connection.accept().await.unwrap().unwrap();
        let send = response
            .send_response(http::Response::new(()), false)
            .unwrap();
        ready.send(send).unwrap();
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
    let mut peer = receiver.await.unwrap();
    let mut empty = [];
    let mut empty_read = Box::pin(stream.read(&mut empty));
    assert!(matches!(
        futures_util::poll!(&mut empty_read),
        std::task::Poll::Ready(Ok(0))
    ));
    drop(empty_read);
    let mut first = [0; 1];
    let mut pending = Box::pin(stream.read(&mut first));
    assert!(futures_util::poll!(&mut pending).is_pending());
    drop(pending);
    peer.send_data(Bytes::new(), false).unwrap();
    peer.send_data(Bytes::from(payload), true).unwrap();
    let mut actual = Vec::new();
    let mut chunk = vec![0; read_size];
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
