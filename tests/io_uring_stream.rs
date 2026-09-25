#![cfg(all(feature = "io-uring", target_os = "linux"))]

use std::io;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::io_uring::{UringStream, UringStreamAdapter};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn poll_io_makes_progress() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let listener =
                tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio_uring::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut stream = UringStreamAdapter::new(UringStream::new(stream));
                let mut request = vec![0; 256 * 1024];
                stream.read_exact(&mut request).await?;
                assert!(request.iter().all(|&byte| byte == 0x5a));
                stream.write_all(&vec![0xa5; 1024]).await?;
                stream.shutdown().await?;
                Ok::<_, io::Error>(())
            });

            let stream = tokio_uring::net::TcpStream::connect(address).await?;
            let mut stream = UringStream::new(stream);
            stream.write_all(&vec![0x5a; 256 * 1024]).await?;
            stream.flush().await?;
            let mut first_response = [0; 7];
            stream.read_exact(&mut first_response).await?;
            assert_eq!(first_response, [0xa5; 7]);
            let mut remaining_response = [0; 1017];
            stream.read_exact(&mut remaining_response).await?;
            assert_eq!(remaining_response, [0xa5; 1017]);
            let mut eof = [0; 1];
            assert_eq!(stream.read(&mut eof).await?, 0);
            assert_eq!(stream.read(&mut eof).await?, 0);
            server.await.unwrap()?;
            Ok::<_, io::Error>(())
        })
        .await
        .expect("poll-based io_uring I/O did not make progress")
        .unwrap();
    });
}

#[test]
fn websocket_echo_uses_poll_io_bridge() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let listener =
                tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio_uring::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket =
                    WebSocketStream::server(UringStream::new(stream), test_config());
                let message = websocket.next().await.unwrap().unwrap();
                websocket.send(message).await.unwrap();
            });

            let stream = tokio_uring::net::TcpStream::connect(address).await.unwrap();
            let mut websocket = WebSocketStream::client(UringStream::new(stream), test_config());
            websocket.send(Message::text("hello")).await.unwrap();
            let message = websocket.next().await.unwrap().unwrap();
            assert_eq!(message.as_bytes(), b"hello");
            server.await.unwrap();
        })
        .await
        .expect("WebSocket echo over the io_uring bridge did not make progress");
    });
}

fn test_config() -> Config {
    Config::builder().auto_ping(false).idle_timeout(0).build()
}

#[test]
fn native_write_preserves_preceding_poll_write() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let listener =
                tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio_uring::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = UringStream::new(stream);
                let mut bytes = vec![0; 2];
                stream.read_exact(&mut bytes).await.unwrap();
                bytes
            });
            let mut stream =
                UringStream::new(tokio_uring::net::TcpStream::connect(address).await.unwrap());
            stream.write_all(b"A").await.unwrap();
            stream.write_all_native(b"B".to_vec()).await.0.unwrap();
            stream.flush().await.unwrap();
            assert_eq!(peer.await.unwrap(), b"AB");
        })
        .await
        .unwrap();
    });
}

#[test]
fn native_read_preserves_poll_read_ahead() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let listener =
                tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio_uring::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"AB".to_vec()).await.0.unwrap();
                stream.shutdown(std::net::Shutdown::Write).unwrap();
            });
            let mut stream =
                UringStream::new(tokio_uring::net::TcpStream::connect(address).await.unwrap());
            peer.await.unwrap();
            let mut first = [0];
            stream.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"A");
            assert!(
                stream.has_buffered_data(),
                "peer sent the complete input before reading"
            );
            let (result, bytes) = stream.read_native(vec![0; 1]).await;
            assert_eq!(result.unwrap(), 1);
            assert_eq!(bytes, b"B");
        })
        .await
        .unwrap();
    });
}

async fn connected_streams() -> (UringStream, UringStream) {
    let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let client = tokio_uring::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (UringStream::new(client), UringStream::new(server))
}

#[test]
fn native_read_resumes_a_cancelled_pending_poll_read() {
    use std::pin::Pin;
    use std::task::Poll;
    use tokio::io::{AsyncRead, ReadBuf};

    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (mut stream, mut peer) = connected_streams().await;
            let mut byte = [0];
            let mut read_buf = ReadBuf::new(&mut byte);
            // The peer has not sent data, so this must leave a real read in flight.
            std::future::poll_fn(|cx| {
                assert!(
                    Pin::new(&mut stream)
                        .poll_read(cx, &mut read_buf)
                        .is_pending()
                );
                Poll::Ready(())
            })
            .await;
            peer.write_all_native(b"x".to_vec()).await.0.unwrap();

            let (result, bytes) = stream.read_native(Vec::with_capacity(8)).await;

            assert_eq!(result.unwrap(), 1);
            assert_eq!(bytes, b"x");
        })
        .await
        .unwrap();
    });
}

#[test]
fn native_write_resumes_a_cancelled_pending_flush() {
    use std::pin::Pin;
    use std::task::Poll;
    use tokio::io::AsyncWrite;

    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (mut stream, mut peer) = connected_streams().await;
            stream.write_all(b"A").await.unwrap();
            // Submit the owned write before switching APIs; the driver has not
            // processed its completion when the operation is first polled.
            std::future::poll_fn(|cx| {
                assert!(Pin::new(&mut stream).poll_flush(cx).is_pending());
                Poll::Ready(())
            })
            .await;

            stream.write_all_native(b"B".to_vec()).await.0.unwrap();

            let mut bytes = [0; 2];
            peer.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"AB");
        })
        .await
        .unwrap();
    });
}

#[test]
fn empty_poll_read_does_not_turn_an_open_stream_into_eof() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (mut stream, mut peer) = connected_streams().await;

            assert_eq!(stream.read(&mut []).await.unwrap(), 0);

            peer.write_all_native(b"x".to_vec()).await.0.unwrap();
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            assert_eq!(&byte, b"x");
        })
        .await
        .unwrap();
    });
}

#[test]
fn native_partial_write_preserves_preceding_poll_write() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (mut stream, mut peer) = connected_streams().await;
            stream.write_all(b"A").await.unwrap();

            let (result, _) = stream.write_native(b"B".to_vec()).await;

            assert_eq!(result.unwrap(), 1);
            stream.flush().await.unwrap();
            let mut bytes = [0; 2];
            peer.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"AB");
        })
        .await
        .unwrap();
    });
}

#[test]
fn native_read_returns_eof_after_poll_read_eof() {
    tokio_uring::start(async {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (mut stream, mut peer) = connected_streams().await;
            peer.shutdown().await.unwrap();
            assert_eq!(stream.read(&mut [0; 1]).await.unwrap(), 0);

            let (result, buffer) = stream.read_native(vec![0; 1]).await;

            assert_eq!(result.unwrap(), 0);
            assert_eq!(buffer, [0]);
        })
        .await
        .unwrap();
    });
}
