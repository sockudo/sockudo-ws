//! WebSocket benchmark utility
//!
//! Benchmarks message throughput and latency.

use std::time::Instant;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use sockudo_ws::Config;
use sockudo_ws::handshake::{build_request, generate_key, parse_response, validate_accept_key};
use sockudo_ws::protocol::{Message, Protocol, Role};

use bytes::BytesMut;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let host = args.get(1).map(|s| s.as_str()).unwrap_or("127.0.0.1:9001");
    let message_size: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let message_count: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let connections: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);

    println!("Sockudo-WS Benchmark");
    println!("====================");
    println!("Target: {}", host);
    println!("Message size: {} bytes", message_size);
    println!("Messages per connection: {}", message_count);
    println!("Connections: {}", connections);
    println!();

    let mut handles = Vec::new();

    let start = Instant::now();

    for i in 0..connections {
        let host = host.to_string();
        let handle = tokio::spawn(async move {
            match run_benchmark(&host, message_size, message_count).await {
                Ok(stats) => Some(stats),
                Err(e) => {
                    eprintln!("Connection {} error: {}", i, e);
                    None
                }
            }
        });
        handles.push(handle);
    }

    let mut total_messages = 0;
    let mut total_bytes = 0;

    for handle in handles {
        if let Some(stats) = handle.await.unwrap() {
            total_messages += stats.messages;
            total_bytes += stats.bytes;
        }
    }

    let elapsed = start.elapsed();

    println!("Results");
    println!("=======");
    println!("Total time: {:.2?}", elapsed);
    println!("Total messages: {}", total_messages);
    println!("Total bytes: {} MB", total_bytes / 1_000_000);
    println!(
        "Throughput: {:.2} msg/sec",
        total_messages as f64 / elapsed.as_secs_f64()
    );
    println!(
        "Bandwidth: {:.2} MB/sec",
        (total_bytes as f64 / 1_000_000.0) / elapsed.as_secs_f64()
    );

    Ok(())
}

struct BenchStats {
    messages: usize,
    bytes: usize,
}

async fn run_benchmark(
    host: &str,
    message_size: usize,
    message_count: usize,
) -> std::io::Result<BenchStats> {
    let mut stream = TcpStream::connect(host).await?;
    stream.set_nodelay(true)?;

    // Perform WebSocket handshake
    let key = generate_key();
    let request = build_request(host, "/", &key, None, None);
    stream.write_all(&request).await?;
    stream.flush().await?;

    // Read response
    let mut buf = BytesMut::with_capacity(64 * 1024);
    loop {
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Connection closed during handshake",
            ));
        }

        match parse_response(&buf) {
            Ok(Some((res, consumed))) => {
                if let Some(accept) = res.accept
                    && !validate_accept_key(&key, accept)
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Invalid accept key",
                    ));
                }
                // Keep leftover data
                let _ = buf.split_to(consumed);
                break;
            }
            Ok(None) => continue,
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Handshake error: {}", e),
                ));
            }
        }
    }

    // Create test message
    let test_data: Vec<u8> = (0..message_size).map(|i| (i % 256) as u8).collect();
    let test_message = Message::Binary(Bytes::from(test_data));

    let config = Config::default();
    let mut protocol = Protocol::new(Role::Client, config.max_frame_size, config.max_message_size);
    let mut write_buf = BytesMut::with_capacity(64 * 1024);

    let mut messages_received = 0;
    let mut bytes_transferred = 0;

    // Pre-encode messages for sending
    let mut encoded_msg = BytesMut::with_capacity(message_size + 14);
    protocol.encode_message(&test_message, &mut encoded_msg)?;
    let encoded_msg = encoded_msg.freeze();

    // Split into reader and writer
    let (mut reader, mut writer) = stream.into_split();

    // Spawn writer task
    let writer_handle = tokio::spawn(async move {
        let mut sent = 0;
        while sent < message_count {
            // Send batch of messages
            let batch_size = (message_count - sent).min(1000);
            for _ in 0..batch_size {
                write_buf.extend_from_slice(&encoded_msg);
            }
            writer.write_all(&write_buf).await?;
            write_buf.clear();
            sent += batch_size;
        }
        writer.flush().await?;
        Ok::<_, std::io::Error>(sent)
    });

    // Reader loop
    while messages_received < message_count {
        let n = reader.read_buf(&mut buf).await?;
        if n == 0 {
            break;
        }

        let recv_messages = protocol
            .process(&mut buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}", e)))?;

        for msg in recv_messages {
            match msg {
                Message::Binary(data) => {
                    messages_received += 1;
                    bytes_transferred += data.len();
                }
                Message::Text(text) => {
                    messages_received += 1;
                    bytes_transferred += text.len();
                }
                Message::Close(_) => {
                    break;
                }
                _ => {}
            }
        }
    }

    // Wait for writer
    let _ = writer_handle.await.unwrap()?;

    Ok(BenchStats {
        messages: messages_received,
        bytes: bytes_transferred,
    })
}
