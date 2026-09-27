//! Axum WebSocket Echo Server with Compression Modes
//!
//! This example demonstrates all available compression modes in sockudo-ws:
//! - Compression::Disabled - no compression
//! - Compression::Dedicated - per-connection compressor (best compression ratio)
//! - Compression::Shared - shared compressor pool (good for many connections)
//! - Compression::Window1KB to Window32KB - various window sizes per RFC 7692
//!
//! Run with: cargo run --example axum_deflate_custom --features "axum-integration,permessage-deflate"

use std::net::SocketAddr;

use axum::{Router, response::IntoResponse, routing::get};
use futures_util::StreamExt;

use sockudo_ws::axum_integration::WebSocketUpgrade;
use sockudo_ws::{Compression, Config, Message};

// DeflateConfig and DeflateWindowBits are available for fine-grained control
// (see comments in ws_handler).
#[allow(unused_imports)]
use sockudo_ws::{DeflateConfig, DeflateWindowBits};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(
            "/",
            get(|| async {
                r#"
sockudo-ws Compression Modes Demo (RFC 7692)

Available compression modes:
  - Compression::Disabled    - No compression
  - Compression::Dedicated   - Per-connection compressor (32KB window, best ratio)
  - Compression::Shared      - Shared compressor pool (32KB window)
  - Compression::Window1KB   - 1KB window (bits=10)
  - Compression::Window2KB   - 2KB window (bits=11)
  - Compression::Window4KB   - 4KB window (bits=12)
  - Compression::Window8KB   - 8KB window (bits=13, balanced)
  - Compression::Window16KB  - 16KB window (bits=14)
  - Compression::Window32KB  - 32KB window (bits=15, max per RFC 7692)

Connect to ws://localhost:3000/ws
"#
            }),
        )
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("sockudo-ws Compression Modes Demo");
    println!("Server listening on http://{}", addr);
    println!("Connect to ws://{}/ws", addr);
    println!();
    println!("Current mode: Window8KB (balanced memory/compression)");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    // OPTION 1: Use a simple compression mode (recommended for most cases)
    // This is the easiest way - just pick a mode that matches your needs:
    //
    // Low memory (many connections):
    //   .compression(Compression::Window4KB)
    //
    // Balanced (default recommendation):
    //   .compression(Compression::Window8KB)
    //
    // Best compression (fewer connections, larger messages):
    //   .compression(Compression::Dedicated)
    //
    // Shared pool (high connection count):
    //   .compression(Compression::Shared)

    let config = Config::builder()
        .max_payload_length(64 * 1024)
        .idle_timeout(120)
        .compression(Compression::Window8KB) // Balanced mode
        .build();

    // OPTION 2: Use explicit DeflateConfig for fine-grained control
    // Uncomment below to use custom deflate settings instead:
    //
    // let deflate_config = DeflateConfig {
    //     server_max_window_bits: DeflateWindowBits::Bits12, // 4KB window
    //     client_max_window_bits: DeflateWindowBits::Bits12,
    //     server_no_context_takeover: true, // Reset after each message
    //     client_no_context_takeover: true,
    //     compression_level: 6,             // 0-9, higher = better but slower
    //     compression_threshold: 32,        // Don't compress messages < 32 bytes
    // };
    //
    // let config = Config::builder()
    //     .max_payload_length(64 * 1024)
    //     .idle_timeout(120)
    //     .deflate_config(deflate_config)
    //     .build();
    //
    // OPTION 3: Use preset DeflateConfig methods
    // - DeflateConfig::default() - balanced settings
    // - DeflateConfig::low_memory() - minimal memory usage
    // - DeflateConfig::best_compression() - maximum compression

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
