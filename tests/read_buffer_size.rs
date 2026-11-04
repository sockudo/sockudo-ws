#![cfg(feature = "tokio-runtime")]

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};

fn payload(len: usize) -> Bytes {
    (0..len).map(|i| (i % 251) as u8).collect::<Vec<_>>().into()
}

/// Small and large frames, in both directions, through a 4KB receive window.
#[tokio::test]
async fn small_receive_window_delivers_frames_larger_than_the_window() {
    let config = Config::builder().read_buffer_size(4096).build();
    let (client_io, server_io) = tokio::io::duplex(8 * 1024);
    let mut client = WebSocketStream::client(client_io, config.clone());
    let server = WebSocketStream::server(server_io, config);
    let (mut server_reader, mut server_writer) = server.split();

    let messages = [
        payload(16),
        payload(200 * 1024),
        payload(6),
        payload(70 * 1024),
        payload(1),
    ];
    let expected = messages.clone();
    let echo = tokio::spawn(async move {
        for expected in &expected {
            match server_reader.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => {
                    assert_eq!(&bytes, expected);
                    server_writer.send(Message::Binary(bytes)).await.unwrap();
                }
                other => panic!("unexpected message {other:?}"),
            }
        }
    });

    for message in &messages {
        client.send(Message::Binary(message.clone())).await.unwrap();
        match client.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => assert_eq!(&bytes, message),
            other => panic!("unexpected echo {other:?}"),
        }
    }
    echo.await.unwrap();
}

#[test]
fn read_buffer_size_defaults_to_the_receive_buffer_constant() {
    assert_eq!(
        Config::default().read_buffer_size,
        sockudo_ws::RECV_BUFFER_SIZE
    );
    assert_eq!(
        Config::builder()
            .read_buffer_size(2048)
            .build()
            .read_buffer_size,
        2048
    );
}
