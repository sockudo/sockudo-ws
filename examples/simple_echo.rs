//! Simple Echo Server using the high-level WebSocketStream API
//!
//! This example shows the clean `ws.send()` style API.
//! Run with: cargo run --example simple_echo

use std::net::SocketAddr;

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};

use sockudo_ws::error::Result;
use sockudo_ws::handshake::{build_response, generate_accept_key, parse_request};
use sockudo_ws::protocol::Message;
use sockudo_ws::{Config, WebSocketStream};

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:9001".parse().unwrap();
    let listener = TcpListener::bind(addr).await?;

    println!("Simple echo server listening on ws://{}", addr);

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

    // Create high-level WebSocket stream
    let mut ws = WebSocketStream::server(stream, Config::default());

    // Simple echo loop using Stream + Sink API
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(text) => {
                ws.send(Message::Text(text)).await?;
            }
            Message::Binary(data) => {
                ws.send(Message::Binary(data)).await?;
            }
            Message::Ping(_) => {
                // Pong is auto-handled by WebSocketStream
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

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
