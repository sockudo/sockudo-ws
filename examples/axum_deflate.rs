//! Axum WebSocket Echo Server with Per-Message Deflate
//!
//! This example demonstrates how to use sockudo-ws with Axum and enable
//! permessage-deflate compression for WebSocket messages.
//!
//! Run with: cargo run --example axum_deflate --features "axum-integration,permessage-deflate"

use std::net::SocketAddr;

use axum::{Router, response::IntoResponse, routing::get};
use futures_util::StreamExt;

use sockudo_ws::axum_integration::WebSocketUpgrade;
use sockudo_ws::{Config, Message};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(
            "/",
            get(|| async { "sockudo-ws Echo Server with Deflate - connect to /ws" }),
        )
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!(
        "Axum + sockudo-ws server with permessage-deflate listening on http://{}",
        addr
    );
    println!(
        "Connect to ws://{}/ws with a WebSocket client that supports deflate",
        addr
    );

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    // Configure with permessage-deflate enabled
    let config = Config::builder()
        .max_payload_length(16 * 1024)
        .idle_timeout(60)
        .enable_deflate() // Enable with default deflate config
        .build();

    ws.config(config).on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: sockudo_ws::axum_integration::WebSocket) {
    println!("New WebSocket connection established with deflate support");

    // Echo loop
    while let Some(msg) = socket.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let text_str = String::from_utf8_lossy(&text);
                println!("Received text: {}", text_str);
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Binary(data)) => {
                println!("Received binary: {} bytes", data.len());
                if socket.send(Message::Binary(data)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Close(reason)) => {
                println!("Connection closing: {:?}", reason);
                break;
            }
            Ok(_) => {} // Ping/Pong handled automatically
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }
    }

    println!("WebSocket connection closed");
}
