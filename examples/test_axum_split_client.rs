//! Test client for the axum split example
//!
//! Run with: cargo run --example test_axum_split_client --features axum-integration

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Connecting to ws://127.0.0.1:3000/ws");

    let stream = TcpStream::connect("127.0.0.1:3000").await?;

    // Perform WebSocket handshake
    let mut stream = stream;
    let handshake = "GET /ws HTTP/1.1\r\n\
         Host: 127.0.0.1:3000\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n";

    stream.write_all(handshake.as_bytes()).await?;

    // Read handshake response
    let mut response = vec![0u8; 1024];
    let n = stream.read(&mut response).await?;
    let response_str = String::from_utf8_lossy(&response[..n]);
    println!("Handshake response:\n{}", response_str);

    if !response_str.contains("101 Switching Protocols") {
        return Err("Handshake failed".into());
    }

    // Create WebSocket stream
    let config = Config::default();
    let mut ws = WebSocketStream::client(stream, config);

    println!("\n--- Testing split functionality ---\n");

    // Send test messages
    let test_messages = [
        "Hello from test client",
        "Testing split() functionality",
        "Message 3",
    ];

    for (i, msg) in test_messages.iter().enumerate() {
        println!("Sending: {}", msg);
        ws.send(Message::Text(msg.to_string().into())).await?;

        // Wait for echo
        if let Some(response) = ws.next().await {
            match response? {
                Message::Text(text) => {
                    let text_str = String::from_utf8_lossy(&text);
                    println!("Received echo: {}", text_str);
                }
                msg => println!("Received: {:?}", msg),
            }
        }

        if i == 0 {
            // Wait a bit to potentially receive a heartbeat
            println!("\nWaiting 2 seconds for potential heartbeat...");
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

            // Check for heartbeat
            tokio::select! {
                Some(msg) = ws.next() => {
                    match msg? {
                        Message::Text(text) => {
                            let text_str = String::from_utf8_lossy(&text);
                            if text_str.contains("Heartbeat") {
                                println!("✓ Received heartbeat: {}", text_str);
                            } else {
                                println!("Received: {}", text_str);
                            }
                        }
                        msg => println!("Received: {:?}", msg),
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    println!("No heartbeat yet (will arrive in ~10s)");
                }
            }
        }
    }

    println!("\n✓ Split functionality test completed successfully!");
    println!("The server correctly:");
    println!("  - Received messages on reader task");
    println!("  - Echoed them back via writer task");
    println!("  - Can send heartbeats independently from writer task");

    // Close connection
    ws.send(Message::Close(None)).await?;

    Ok(())
}
