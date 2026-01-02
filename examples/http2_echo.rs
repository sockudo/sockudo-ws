//! HTTP/2 WebSocket Echo Server Example
//!
//! This example demonstrates running a WebSocket server over HTTP/2
//! using the Extended CONNECT protocol (RFC 8441).
//!
//! # Running
//!
//! ```bash
//! cargo run --example http2_echo --features http2
//! ```
//!
//! Note: HTTP/2 typically requires TLS in production. This example
//! shows the basic structure - add TLS for production use.

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Http2, Message, WebSocketServer};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("HTTP/2 WebSocket Echo Server");
    println!("============================");
    println!();
    println!("This example shows HTTP/2 WebSocket (RFC 8441).");
    println!("In production, wrap the stream with TLS first.");
    println!();

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Listening on: {}", listener.local_addr()?);

    let config = Config::builder()
        .max_payload_length(64 * 1024)
        .http2_max_streams(100)
        .build();

    let server = WebSocketServer::<Http2>::new(config);

    loop {
        let (stream, addr) = listener.accept().await?;
        println!("New TCP connection from: {}", addr);

        // In production: wrap `stream` with TLS here
        // let tls_stream = tls_acceptor.accept(stream).await?;

        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = server
                .serve(stream, |mut ws, req| async move {
                    println!(
                        "HTTP/2 WebSocket connection to: {} (protocol: {:?})",
                        req.path, req.protocol
                    );

                    // Echo loop - same API as HTTP/1.1!
                    while let Some(msg) = ws.next().await {
                        match msg {
                            Ok(Message::Text(text)) => {
                                println!(
                                    "Received text: {}",
                                    std::str::from_utf8(&text).unwrap_or("<invalid utf8>")
                                );
                                if ws.send(Message::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Message::Binary(data)) => {
                                println!("Received binary: {} bytes", data.len());
                                if ws.send(Message::Binary(data)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Message::Ping(_)) => {
                                println!("Received ping");
                                // Pong is sent automatically
                            }
                            Ok(Message::Pong(_)) => {
                                println!("Received pong");
                            }
                            Ok(Message::Close(reason)) => {
                                println!("Received close: {:?}", reason);
                                break;
                            }
                            Err(e) => {
                                eprintln!("Error: {}", e);
                                break;
                            }
                        }
                    }

                    println!("WebSocket connection closed");
                })
                .await
            {
                eprintln!("HTTP/2 server error: {}", e);
            }
        });
    }
}
