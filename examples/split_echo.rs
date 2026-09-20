//! Split Stream Echo Server
//!
//! Demonstrates concurrent read/write using split streams.
//! Run with: cargo run --example split_echo

use std::net::SocketAddr;

use bytes::BytesMut;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use sockudo_ws::error::Result;
use sockudo_ws::handshake::{build_response, generate_accept_key, parse_request};
use sockudo_ws::protocol::Message;
use sockudo_ws::{Config, WebSocketStream};

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:9001".parse().unwrap();
    let listener = TcpListener::bind(addr).await?;

    println!("Split echo server listening on ws://{}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true).ok();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream).await {
                eprintln!("Connection error: {}", e);
            }
        });
    }
}

async fn handle_connection(stream: TcpStream) -> Result<()> {
    // Perform WebSocket handshake
    let stream = do_handshake(stream).await?;

    // Create WebSocket stream and split it
    let ws = WebSocketStream::server(stream, Config::default());
    let (mut reader, mut writer) = ws.split();

    // Channel to forward messages from reader to writer
    let (tx, mut rx) = mpsc::channel::<Message>(32);

    // Spawn writer task
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = writer.send(msg).await {
                eprintln!("Write error: {}", e);
                break;
            }
        }
    });

    // Reader loop (in current task)
    while let Some(msg) = reader.next().await {
        match msg? {
            Message::Text(text) => {
                // Forward to writer task
                if tx.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Message::Binary(data) => {
                if tx.send(Message::Binary(data)).await.is_err() {
                    break;
                }
            }
            Message::Ping(_) | Message::Pong(_) => {
                // Handled automatically
            }
            Message::Close(_) => break,
        }
    }

    // Cleanup
    drop(tx);
    let _ = writer_task.await;

    Ok(())
}

async fn do_handshake(mut stream: TcpStream) -> Result<TcpStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = BytesMut::with_capacity(4096);

    loop {
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(sockudo_ws::Error::ConnectionClosed);
        }

        if let Some((req, _)) = parse_request(&buf)? {
            let accept_key = generate_accept_key(req.key);
            let response = build_response(&accept_key, None, None);
            stream.write_all(&response).await?;
            stream.flush().await?;
            break;
        }
    }

    Ok(stream)
}
