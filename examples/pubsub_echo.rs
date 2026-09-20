//! Pub/Sub Echo Server Example
//!
//! This example demonstrates the high-performance pub/sub system.
//! Clients can subscribe to topics and broadcast messages to all subscribers.
//!
//! Run with: cargo run --example pubsub_echo --features tokio-runtime
//!
//! Test with multiple websocket clients:
//!   websocat ws://127.0.0.1:9001
//!   Send: /join chat
//!   Send: Hello everyone!

use std::sync::Arc;

use bytes::BytesMut;

use tokio::net::{TcpListener, TcpStream};

use sockudo_ws::error::Result;
use sockudo_ws::handshake::{build_response, generate_accept_key, parse_request};
use sockudo_ws::pubsub::PubSub;
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    println!("Pub/Sub Echo Server listening on 127.0.0.1:9001");
    println!("Commands:");
    println!("  /join <topic>   - Subscribe to a topic");
    println!("  /leave <topic>  - Unsubscribe from a topic");
    println!("  /topics         - List subscribed topics");
    println!("  /stats          - Show pub/sub statistics");
    println!("  Any other text  - Broadcast to all subscribed topics");
    println!();

    let listener = TcpListener::bind("127.0.0.1:9001").await?;
    let pubsub = Arc::new(PubSub::new());

    loop {
        let (stream, addr) = listener.accept().await?;
        stream.set_nodelay(true).ok();
        let pubsub = pubsub.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, pubsub).await {
                eprintln!("Connection error from {}: {}", addr, e);
            }
        });
    }
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

async fn handle_connection(stream: TcpStream, pubsub: Arc<PubSub>) -> Result<()> {
    // Perform WebSocket handshake
    let stream = do_handshake(stream).await?;
    let (mut reader, mut writer) = WebSocketStream::server(stream, Config::default()).split();

    // Create a subscriber for this connection
    let (tx, mut rx) = mpsc::unbounded_channel();
    let subscriber_id = pubsub.create_subscriber(tx);

    // Send welcome message
    writer
        .send(Message::text(
            "Welcome! Use /join <topic> to subscribe to a topic.",
        ))
        .await?;

    // Track topics for this connection
    let mut local_topics: Vec<String> = Vec::new();

    loop {
        tokio::select! {
            // Handle incoming WebSocket messages
            msg = reader.next() => {
                match msg {
                    Some(Ok(Message::Text(data))) => {
                        let text = String::from_utf8_lossy(&data);
                        let text = text.trim();

                        if text.starts_with("/join ") {
                            let topic = text.strip_prefix("/join ").unwrap().trim();
                            if !topic.is_empty() {
                                pubsub.subscribe(subscriber_id, topic);
                                local_topics.push(topic.to_string());
                                writer.send(Message::text(format!(
                                    "Subscribed to '{}'. {} subscribers on this topic.",
                                    topic,
                                    pubsub.topic_subscriber_count(topic)
                                ))).await?;
                            }
                        } else if text.starts_with("/leave ") {
                            let topic = text.strip_prefix("/leave ").unwrap().trim();
                            if pubsub.unsubscribe(subscriber_id, topic) {
                                local_topics.retain(|t| t != topic);
                                writer.send(Message::text(format!(
                                    "Unsubscribed from '{}'",
                                    topic
                                ))).await?;
                            } else {
                                writer.send(Message::text(format!(
                                    "Not subscribed to '{}'",
                                    topic
                                ))).await?;
                            }
                        } else if text == "/topics" {
                            let topics = pubsub.subscriber_topics(subscriber_id);
                            if topics.is_empty() {
                                writer.send(Message::text("Not subscribed to any topics")).await?;
                            } else {
                                writer.send(Message::text(format!(
                                    "Subscribed topics: {}",
                                    topics.join(", ")
                                ))).await?;
                            }
                        } else if text == "/stats" {
                            writer.send(Message::text(format!(
                                "Stats: {} topics, {} subscribers, {} messages published",
                                pubsub.topic_count(),
                                pubsub.subscriber_count(),
                                pubsub.messages_published()
                            ))).await?;
                        } else if !text.is_empty() {
                            // Broadcast to all subscribed topics (excluding sender)
                            let msg = Message::text(text.to_string());
                            let mut total_sent = 0;
                            for topic in &local_topics {
                                let result = pubsub.publish_excluding(subscriber_id, topic, msg.clone());
                                total_sent += result.count();
                            }
                            writer.send(Message::text(format!(
                                "Sent to {} recipients across {} topics",
                                total_sent,
                                local_topics.len()
                            ))).await?;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        // Broadcast binary to all subscribed topics
                        let msg = Message::Binary(data);
                        for topic in &local_topics {
                            pubsub.publish_excluding(subscriber_id, topic, msg.clone());
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
                    }
                    Some(Ok(_)) => {} // Ping/Pong handled automatically
                    Some(Err(e)) => {
                        eprintln!("WebSocket error: {}", e);
                        break;
                    }
                }
            }

            // Handle messages from pub/sub (from other connections)
            Some(msg) = rx.recv() => {
                if let Err(e) = writer.send(msg).await {
                    eprintln!("Failed to send pub/sub message: {}", e);
                    break;
                }
            }
        }
    }

    // Cleanup: remove subscriber (automatically unsubscribes from all topics)
    pubsub.remove_subscriber(subscriber_id);

    Ok(())
}
