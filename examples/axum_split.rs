//! Axum WebSocket Split Example
//!
//! This example demonstrates the split() functionality in the axum integration,
//! which allows separating a WebSocket into independent reader and writer halves
//! for concurrent operations.
//!
//! Run with: cargo run --example axum_split --features axum-integration

use std::net::SocketAddr;

use axum::{Router, response::IntoResponse, routing::get};
use sockudo_ws::Message;
use sockudo_ws::axum_integration::{WebSocket, WebSocketUpgrade};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(
            "/",
            get(|| async { "WebSocket Split Test - connect to /ws" }),
        )
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Axum WebSocket Split server listening on http://{}", addr);
    println!("Connect to ws://{}/ws", addr);
    println!("\nThis server demonstrates split() by:");
    println!("1. Receiving messages on a separate reader task");
    println!("2. Sending periodic heartbeats from the writer");
    println!("3. Echoing received messages back via the writer");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    // Split the WebSocket into reader and writer
    // Note: split() returns Option because compressed WebSockets don't support splitting yet
    let Some((mut reader, mut writer)) = socket.split() else {
        eprintln!("Cannot split compressed WebSocket connection");
        return;
    };

    // Create a channel to send messages from reader to writer
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    // Spawn reader task
    let reader_handle = tokio::spawn(async move {
        println!("Reader task started");
        while let Some(msg) = reader.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str = String::from_utf8_lossy(&text);
                    println!("Received text: {}", text_str);
                    // Send to writer task for echo
                    if tx.send(Message::Text(text)).is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    println!("Received binary: {} bytes", data.len());
                    if tx.send(Message::Binary(data)).is_err() {
                        break;
                    }
                }
                Ok(Message::Close(frame)) => {
                    println!("Received close frame: {:?}", frame);
                    break;
                }
                Ok(Message::Ping(data)) => {
                    println!("Received ping: {} bytes", data.len());
                }
                Ok(Message::Pong(data)) => {
                    println!("Received pong: {} bytes", data.len());
                }
                Err(e) => {
                    eprintln!("Reader error: {}", e);
                    break;
                }
            }
        }
        println!("Reader task finished");
    });

    // Spawn writer task
    let writer_handle = tokio::spawn(async move {
        println!("Writer task started");
        let mut heartbeat_interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
        let mut heartbeat_count = 0u32;

        loop {
            tokio::select! {
                // Send heartbeat periodically
                _ = heartbeat_interval.tick() => {
                    heartbeat_count += 1;
                    let msg = format!("Heartbeat #{}", heartbeat_count);
                    println!("Sending heartbeat: {}", msg);
                    if writer.send(Message::Text(msg.into())).await.is_err() {
                        eprintln!("Failed to send heartbeat");
                        break;
                    }
                }
                // Echo messages from reader
                Some(msg) = rx.recv() => {
                    println!("Echoing message back");
                    if writer.send(msg).await.is_err() {
                        eprintln!("Failed to echo message");
                        break;
                    }
                }
            }
        }
        println!("Writer task finished");
    });

    // Wait for both tasks to complete
    let _ = tokio::join!(reader_handle, writer_handle);
    println!("WebSocket connection closed");
}
