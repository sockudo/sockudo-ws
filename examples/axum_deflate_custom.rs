//! Axum WebSocket Echo Server with Custom Per-Message Deflate Configuration
//!
//! This example demonstrates how to use sockudo-ws with Axum and configure
//! custom permessage-deflate settings (window bits, compression level, etc.).
//!
//! Run with: cargo run --example axum_deflate_custom --features "axum-integration,permessage-deflate"

use std::net::SocketAddr;

use axum::{Router, response::IntoResponse, routing::get};
use futures_util::StreamExt;

use sockudo_ws::axum_integration::WebSocketUpgrade;
use sockudo_ws::{Config, DeflateConfig, Message};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(
            "/",
            get(|| async { "sockudo-ws Echo Server with Custom Deflate - connect to /ws" }),
        )
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Axum + sockudo-ws server with custom permessage-deflate config");
    println!("Server listening on http://{}", addr);
    println!("Connect to ws://{}/ws", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    // Create custom deflate configuration
    // This uses best compression settings
    let deflate_config = DeflateConfig::best_compression();

    // Alternative configurations:
    // - DeflateConfig::default() - balanced settings
    // - DeflateConfig::low_memory() - minimal memory usage
    // - Custom: DeflateConfig {
    //     server_max_window_bits: 12,
    //     client_max_window_bits: 12,
    //     server_no_context_takeover: true,
    //     client_no_context_takeover: true,
    //     compression_level: 6,
    //     compression_threshold: 32,
    //   }

    let config = Config::builder()
        .max_payload_length(64 * 1024)
        .idle_timeout(120)
        .deflate_config(deflate_config) // Use custom deflate config
        .build();

    ws.config(config).on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: sockudo_ws::axum_integration::WebSocket) {
    println!("✓ New WebSocket connection with best_compression deflate");

    let mut msg_count = 0;

    // Echo loop
    while let Some(msg) = socket.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                msg_count += 1;
                let size = text.len();
                let text_str = String::from_utf8_lossy(&text);
                println!(
                    "Message #{}: Received {} bytes (text) - '{}'",
                    msg_count,
                    size,
                    if text_str.len() > 50 {
                        format!("{}...", &text_str[..50])
                    } else {
                        text_str.to_string()
                    }
                );

                // Echo back
                if socket.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Binary(data)) => {
                msg_count += 1;
                println!(
                    "Message #{}: Received {} bytes (binary)",
                    msg_count,
                    data.len()
                );

                // Echo back
                if socket.send(Message::Binary(data)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Close(reason)) => {
                println!("Connection closing: {:?}", reason);
                break;
            }
            Ok(Message::Ping(_)) => {
                println!("Received ping");
            }
            Ok(Message::Pong(_)) => {
                println!("Received pong");
            }
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }
    }

    println!("✗ Connection closed. Total messages: {}", msg_count);
}
