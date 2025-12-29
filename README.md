# sockudo-ws

Ultra-low latency WebSocket library for Rust, designed for high-frequency trading (HFT) applications and real-time systems. Fully compatible with Tokio and Axum.

Used in [Sockudo](https://github.com/RustNSparks/sockudo), a high-performance Pusher-compatible WebSocket server.

**Coming soon:** N-API bindings for Node.js

## Performance

Benchmarked against [uWebSockets](https://github.com/uNetworking/uWebSockets), the industry standard for high-performance WebSockets:

| Test Case | sockudo-ws | uWebSockets | Ratio |
|-----------|------------|-------------|-------|
| 512 bytes, 100 connections | 232,712 msg/s | 227,973 msg/s | **1.02x** |
| 1024 bytes, 100 connections | 232,072 msg/s | 224,498 msg/s | **1.03x** |
| 512 bytes, 500 connections | 231,135 msg/s | 222,493 msg/s | **1.03x** |
| 1024 bytes, 500 connections | 222,578 msg/s | 216,833 msg/s | **1.02x** |

sockudo-ws matches or exceeds uWebSockets performance while providing a safe, ergonomic Rust API.

## Features

- **SIMD Acceleration**: AVX2/AVX-512/NEON for frame masking and UTF-8 validation
- **Zero-Copy Parsing**: Direct buffer access without intermediate allocations
- **Write Batching (Corking)**: Minimizes syscalls via vectored I/O
- **permessage-deflate**: Full compression support with shared/dedicated compressors
- **Split Streams**: Concurrent read/write from separate tasks
- **Autobahn Compliant**: Passes all 517 Autobahn test suite cases

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws" }

# Optional: Enable compression
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["permessage-deflate"] }
```

## Quick Start

### Simple Echo Server

```rust
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::net::TcpStream;

async fn handle(stream: TcpStream) {
    // After WebSocket handshake...
    let mut ws = WebSocketStream::server(stream, Config::default());

    while let Some(msg) = ws.next().await {
        match msg.unwrap() {
            Message::Text(text) => {
                ws.send(Message::Text(text)).await.unwrap();
            }
            Message::Binary(data) => {
                ws.send(Message::Binary(data)).await.unwrap();
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}
```

### Split Streams (Concurrent Read/Write)

```rust
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::sync::mpsc;

async fn handle(stream: TcpStream) {
    let ws = WebSocketStream::server(stream, Config::default());
    let (mut reader, mut writer) = ws.split();

    // Writer task
    let (tx, mut rx) = mpsc::channel::<Message>(32);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            writer.send(msg).await.unwrap();
        }
    });

    // Reader loop
    while let Some(msg) = reader.next().await {
        match msg.unwrap() {
            Message::Text(text) => {
                tx.send(Message::Text(text)).await.unwrap();
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}
```

### Axum Integration

```rust
use axum::{Router, body::Body, extract::Request, http::{Response, StatusCode, header}, routing::get};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use sockudo_ws::{Config, Message, WebSocketStream, handshake::generate_accept_key};

async fn ws_handler(req: Request) -> Response<Body> {
    let key = req.headers().get("sec-websocket-key").unwrap().to_str().unwrap();
    let accept_key = generate_accept_key(key);

    tokio::spawn(async move {
        if let Ok(upgraded) = hyper::upgrade::on(req).await {
            let mut ws = WebSocketStream::server(TokioIo::new(upgraded), Config::default());
            
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Text(text) => { ws.send(Message::Text(text)).await.ok(); }
                    Message::Binary(data) => { ws.send(Message::Binary(data)).await.ok(); }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(Body::empty())
        .unwrap()
}
```

### Configuration (uWebSockets-style)

```rust
use sockudo_ws::{Config, Compression};

let config = Config::builder()
    .compression(Compression::Shared)      // SHARED_COMPRESSOR
    .max_payload_length(16 * 1024)         // 16KB max message
    .idle_timeout(10)                      // 10 second timeout
    .max_backpressure(1024 * 1024)         // 1MB backpressure limit
    .build();

// Or use uWebSockets-style defaults
let config = Config::uws_defaults();
```

## Configuration Options

| Option | Default | Description |
|--------|---------|-------------|
| `compression` | `Disabled` | Compression mode |
| `max_message_size` | 64MB | Maximum message size |
| `max_frame_size` | 16MB | Maximum single frame size |
| `idle_timeout` | 120s | Close connection after inactivity (0 = disabled) |
| `max_backpressure` | 1MB | Max write buffer before dropping connection |
| `auto_ping` | true | Automatic ping/pong keepalive |
| `ping_interval` | 30s | Seconds between pings |
| `write_buffer_size` | 16KB | Cork buffer size |

### Compression Modes

| Mode | Description |
|------|-------------|
| `Compression::Disabled` | No compression |
| `Compression::Dedicated` | Per-connection compressor (best ratio, more memory) |
| `Compression::Shared` | Shared compressor (good for many connections) |
| `Compression::Shared4KB` | Shared with 4KB sliding window |
| `Compression::Shared8KB` | Shared with 8KB sliding window |
| `Compression::Shared16KB` | Shared with 16KB sliding window |

## API Reference

### WebSocketStream

The main WebSocket type implementing `Stream` + `Sink`:

```rust
// Create server-side stream
let ws = WebSocketStream::server(tcp_stream, config);

// Create client-side stream
let ws = WebSocketStream::client(tcp_stream, config);

// Send messages
ws.send(Message::Text("hello".into())).await?;
ws.send(Message::Binary(bytes)).await?;

// Receive messages
while let Some(msg) = ws.next().await {
    // handle msg
}

// Close connection
ws.close(1000, "goodbye").await?;
```

### Split Streams

For concurrent read/write operations:

```rust
let (reader, writer) = ws.split();

// SplitReader
reader.next().await  // Receive message

// SplitWriter
writer.send(msg).await?;
writer.send_text("hello".into()).await?;
writer.send_binary(bytes).await?;
writer.close(1000, "bye").await?;
writer.is_closed().await;

// Reunite
let ws = sockudo_ws::reunite(reader, writer)?;
```

### Message Types

```rust
pub enum Message {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<CloseReason>),
}
```

## Running Tests

### Autobahn Test Suite

```bash
cd autobahn

# Build and run server + tests
make test

# Or manually:
make run  # Start server
# Then run Autobahn client in another terminal
```

## Architecture

```
sockudo-ws/
├── src/
│   ├── lib.rs          # Public API, Config
│   ├── stream.rs       # WebSocketStream, Split types
│   ├── protocol.rs     # WebSocket protocol state machine
│   ├── frame.rs        # Frame encoding/decoding
│   ├── handshake.rs    # HTTP upgrade handshake
│   ├── mask.rs         # SIMD frame masking
│   ├── utf8.rs         # SIMD UTF-8 validation
│   ├── cork.rs         # Write batching buffer
│   ├── deflate.rs      # permessage-deflate compression
│   └── simd.rs         # SIMD feature detection
├── examples/
│   ├── simple_echo.rs  # Basic echo server
│   ├── split_echo.rs   # Concurrent read/write example
│   └── axum_echo.rs    # Axum integration example
├── autobahn/
│   ├── server.rs       # Autobahn test server
│   └── Makefile        # Build and test automation
└── benches/
    └── throughput.rs   # Criterion benchmarks
```

## Performance Optimizations

1. **SIMD Masking**: Uses AVX2/AVX-512/NEON to XOR mask frames at 32-64 bytes per cycle
2. **SIMD UTF-8**: Validates UTF-8 text at memory bandwidth speeds
3. **Zero-Copy**: Parses frames directly from receive buffer without copying
4. **Cork Buffer**: Batches small writes into 16KB chunks for fewer syscalls
5. **Vectored I/O**: Uses `writev()` to send multiple buffers in single syscall

## License

MIT

## Credits

- Inspired by [uWebSockets](https://github.com/uNetworking/uWebSockets)
