#![cfg(all(feature = "compio-runtime", feature = "http2"))]

use bytes::Bytes;
use compio::buf::BufResult;
use compio::io::AsyncRead;
use compio::net::{TcpListener, TcpStream};
use compio::runtime::{self, CancelToken, FutureExt};
use futures_channel::oneshot;
use rstest::rstest;
use sockudo_ws::compio::CompioHttp2Stream;
use std::future::Future;
use std::task::Poll;
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[rstest]
#[case(7)]
#[case(65536)]
#[compio::test]
async fn empty_data_does_not_end_reads_before_payload(#[case] read_size: usize) {
    with_open_stream(async move |mut stream, mut peer| {
        // Exceed the initial flow-control window to require capacity release.
        let expected = (0..128 * 1024 + 1)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        peer.send_data(Bytes::new(), false).unwrap();
        peer.send_data(Bytes::copy_from_slice(&expected), false)
            .unwrap();
        peer.send_data(Bytes::new(), true).unwrap();

        let mut actual = Vec::new();
        let mut buffer = Vec::with_capacity(read_size);
        loop {
            let BufResult(result, returned) = stream.read(buffer).await;
            buffer = returned;
            let len = result.unwrap();
            if len == 0 {
                break;
            }
            actual.extend_from_slice(&buffer[..len]);
            buffer.clear();
        }
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual, expected);
    })
    .await;
}

#[compio::test]
async fn zero_capacity_buffer_completes_without_waiting_for_data() {
    with_open_stream(async |mut stream, _peer| {
        // The peer keeps the stream open without sending DATA or END_STREAM.
        let mut read = Box::pin(stream.read(Vec::<u8>::new()));
        let Poll::Ready(BufResult(result, returned)) = futures_util::poll!(&mut read) else {
            panic!("a zero-capacity read must not wait for DATA");
        };
        assert_eq!(result.unwrap(), 0);
        assert_eq!(returned.capacity(), 0);
    })
    .await;
}

#[compio::test]
async fn cancelled_read_returns_buffer_and_preserves_following_data() {
    with_open_stream(async |mut stream, mut peer| {
        let buffer = Vec::<u8>::with_capacity(32);
        let original = buffer.as_ptr();
        let cancel = CancelToken::new();
        let mut read = Box::pin(stream.read(buffer).with_cancel(cancel.clone()));
        assert!(futures_util::poll!(&mut read).is_pending());
        cancel.cancel();
        let BufResult(result, buffer) = read.await;
        assert!(result.is_err());
        assert_eq!(buffer.as_ptr(), original);

        peer.send_data(Bytes::new(), false).unwrap();
        peer.send_data(Bytes::from_static(b"after cancellation"), true)
            .unwrap();
        let BufResult(result, buffer) = stream.read(buffer).await;
        let len = result.unwrap();
        assert_eq!(&buffer[..len], b"after cancellation");
        let BufResult(result, _) = stream.read(buffer).await;
        assert_eq!(result.unwrap(), 0);
    })
    .await;
}

// Drive real h2 over Compio TCP while each test controls the response DATA.
async fn with_open_stream<F, Fut>(test: F)
where
    F: FnOnce(CompioHttp2Stream, h2::SendStream<Bytes>) -> Fut,
    Fut: Future<Output = ()>,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready, receiver) = oneshot::channel();
    let server = runtime::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let socket = Box::pin(compio::io::compat::AsyncStream::new(socket)).compat();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (_, mut response) = connection.accept().await.unwrap().unwrap();
        let send = response
            .send_response(http::Response::new(()), false)
            .unwrap();
        ready.send(send).unwrap();
        while connection.accept().await.is_some() {}
    });
    let socket = TcpStream::connect(addr).await.unwrap();
    let socket = Box::pin(compio::io::compat::AsyncStream::new(socket)).compat();
    let (mut client, connection) = h2::client::handshake(socket).await.unwrap();
    let driver = runtime::spawn(connection);
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
    test(CompioHttp2Stream::new(send, recv), receiver.await.unwrap()).await;
    drop(client);
    driver.cancel().await;
    server.cancel().await;
}
