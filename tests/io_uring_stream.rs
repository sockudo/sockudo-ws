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
