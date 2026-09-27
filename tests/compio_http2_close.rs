#![cfg(all(feature = "compio-runtime", feature = "http2"))]

use bytes::Bytes;
use futures_channel::oneshot;
use sockudo_ws::Config;
use sockudo_ws::compio::net::{TcpListener, TcpStream};
use sockudo_ws::compio::{runtime, serve_http2};
use std::cell::RefCell;
use std::rc::Rc;
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[compio::test]
async fn handler_return_delivers_queued_data_and_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (handler_returning, wait_for_handler) = oneshot::channel();
    let handler_returning = Rc::new(RefCell::new(Some(handler_returning)));
    let (release_handler, wait_for_release) = oneshot::channel();
    let wait_for_release = Rc::new(RefCell::new(Some(wait_for_release)));

    let server = runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_http2(stream, Config::default(), move |mut ws, _request| {
            let handler_returning = handler_returning.borrow_mut().take().unwrap();
            let wait_for_release = wait_for_release.borrow_mut().take().unwrap();
            async move {
                wait_for_release.await.unwrap();
                ws.send_binary(Bytes::from_static(b"queued response"))
                    .await
                    .unwrap();
                ws.close(1000, "bye").await.unwrap();
                drop(ws);
                handler_returning.send(()).unwrap();
            }
        })
        .await
        .unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let stream = Box::pin(compio::io::compat::AsyncStream::new(stream)).compat();
    let (mut client, connection) = h2::client::handshake(stream).await.unwrap();
    let client_driver = runtime::spawn(async move {
        connection.await.unwrap();
    });

    let request = http::Request::builder()
        .method(http::Method::CONNECT)
        .uri(format!("https://localhost:{}/close", addr.port()))
        .header("sec-websocket-version", "13")
        .extension(h2::ext::Protocol::from_static("websocket"))
        .body(())
        .unwrap();
    let (response, mut send) = client.send_request(request, false).unwrap();
    let mut recv = response.await.unwrap().into_body();
    // End the request half cleanly so this test isolates the response half
    // instead of observing a reset caused by abandoning unread request data.
    send.send_data(Bytes::new(), true).unwrap();
    release_handler.send(()).unwrap();
    wait_for_handler.await.unwrap();

    // Read only after the handler has released its WebSocket. The queued
    // frames must survive that drop and be followed by a clean END_STREAM.
    let mut wire = Vec::new();
    while let Some(chunk) = recv.data().await {
        wire.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(wire, b"\x82\x0fqueued response\x88\x05\x03\xe8bye");

    drop(recv);
    drop(send);
    drop(client);
    client_driver.cancel().await;
    server.await.unwrap();
}
