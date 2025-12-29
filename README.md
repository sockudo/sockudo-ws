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

*Benchmarked on AMD Ryzen 9 7950X, 32GB RAM, Linux 6.18*

sockudo-ws matches or exceeds uWebSockets performance while providing a safe, ergonomic Rust API.

## Features

- **SIMD Acceleration**: AVX2/AVX-512/NEON for frame masking and UTF-8 validation
- **Zero-Copy Parsing**: Direct buffer access without intermediate allocations
- **Write Batching (Corking)**: Minimizes syscalls via vectored I/O
- **permessage-deflate**: Full compression support with shared/dedicated compressors
- **Split Streams**: Concurrent read/write from separate tasks
- **HTTP/2 WebSocket**: RFC 8441 Extended CONNECT protocol support
- **HTTP/3 WebSocket**: RFC 9220 WebSocket over QUIC support
- **io_uring**: Linux high-performance async I/O (combinable with HTTP/2 and HTTP/3)
- **Autobahn Compliant**: Passes all 517 Autobahn test suite cases

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws" }

# With compression
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["permessage-deflate"] }

# With HTTP/2 support
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["http2"] }

# With HTTP/3 support
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["http3"] }

# With io_uring (Linux only)
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["io-uring"] }

# All transports
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["all-transports"] }

# Everything
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["full"] }
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

## HTTP/2 WebSocket (RFC 8441)

HTTP/2 WebSocket uses the Extended CONNECT protocol for multiplexed WebSocket streams over a single TCP connection.

```rust
use sockudo_ws::http2::H2WebSocketServer;
use sockudo_ws::{Config, Message};
use futures_util::{SinkExt, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    
    let config = Config::builder()
        .http2_max_streams(100)
        .build();
    
    let server = H2WebSocketServer::new(config);

    loop {
        let (stream, _) = listener.accept().await?;
        
        // In production: wrap with TLS first
        // let tls_stream = tls_acceptor.accept(stream).await?;
        
        let server = server.clone();
        tokio::spawn(async move {
            server.serve(stream, |mut ws, req| async move {
                println!("HTTP/2 WebSocket at: {}", req.path);
                
                // Same API as HTTP/1.1!
                while let Some(msg) = ws.next().await {
                    if let Ok(msg) = msg {
                        ws.send(msg).await.ok();
                    }
                }
            }).await.ok();
        });
    }
}
```

### HTTP/2 Client

```rust
use sockudo_ws::http2::H2WebSocketClient;

let client = H2WebSocketClient::new(Config::default());
let mut ws = client.connect(tls_stream, "wss://example.com/ws", None).await?;

ws.send(Message::Text("Hello!".into())).await?;
```

### HTTP/2 Multiplexed Connections

Open multiple WebSocket streams over a single HTTP/2 connection:

```rust
let client = H2WebSocketClient::new(Config::default());
let mut conn = client.connect_multiplexed(tls_stream).await?;

// Open multiple WebSocket streams on the same connection
let mut ws1 = conn.open_websocket("wss://example.com/chat", None).await?;
let mut ws2 = conn.open_websocket("wss://example.com/notifications", None).await?;
```

## HTTP/3 WebSocket (RFC 9220)

HTTP/3 WebSocket runs over QUIC, providing benefits like 0-RTT, no head-of-line blocking, and better mobile performance.

```rust
use sockudo_ws::http3::H3WebSocketServer;
use sockudo_ws::{Config, Message};
use futures_util::{SinkExt, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load TLS certificates (required for QUIC)
    let tls_config = load_server_tls_config()?;
    
    let ws_config = Config::builder()
        .http3_idle_timeout(30_000)
        .build();

    let server = H3WebSocketServer::bind(
        "0.0.0.0:4433".parse()?,
        tls_config,
        ws_config,
    ).await?;

    println!("HTTP/3 server listening on {}", server.local_addr()?);

    server.serve(|mut ws, req| async move {
        println!("HTTP/3 WebSocket at: {}", req.path);
        
        // Same API as HTTP/1.1 and HTTP/2!
        while let Some(msg) = ws.next().await {
            if let Ok(msg) = msg {
                ws.send(msg).await.ok();
            }
        }
    }).await?;

    Ok(())
}
```

### HTTP/3 Benefits

| Feature | Benefit |
|---------|---------|
| No head-of-line blocking | One slow stream doesn't block others |
| 0-RTT connection resumption | Faster reconnections |
| Better mobile performance | Handles network changes gracefully |
| Multiple streams per connection | Efficient multiplexing |

## io_uring Support (Linux)

io_uring provides kernel-level async I/O with zero-copy operations. It's a **transport layer** that can be combined with any protocol.

### io_uring with HTTP/1.1

```rust
use sockudo_ws::io_uring::UringStream;
use sockudo_ws::{Config, WebSocketStream};

#[tokio_uring::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:8080".parse()?)?;

    loop {
        let (tcp_stream, _) = listener.accept().await?;
        
        // Wrap in UringStream for io_uring I/O
        let uring_stream = UringStream::new(tcp_stream);
        
        tokio_uring::spawn(async move {
            let mut ws = WebSocketStream::server(uring_stream, Config::default());
            
            while let Some(msg) = ws.next().await {
                if let Ok(msg) = msg {
                    ws.send(msg).await.ok();
                }
            }
        });
    }
}
```

### io_uring with HTTP/2

Combine io_uring transport with HTTP/2 protocol for maximum performance:

```rust
use sockudo_ws::io_uring::UringStream;
use sockudo_ws::http2::H2WebSocketServer;
use sockudo_ws::Config;

#[tokio_uring::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:8080".parse()?)?;
    let server = H2WebSocketServer::new(Config::default());

    loop {
        let (tcp_stream, _) = listener.accept().await?;
        
        // 1. Wrap TCP in UringStream for io_uring I/O
        let uring_stream = UringStream::new(tcp_stream);
        
        // 2. Add TLS (required for HTTP/2)
        // let tls_stream = tls_acceptor.accept(uring_stream).await?;
        
        // 3. Run HTTP/2 WebSocket over io_uring transport
        let server = server.clone();
        tokio_uring::spawn(async move {
            server.serve(uring_stream, |mut ws, req| async move {
                // WebSocket over HTTP/2 over io_uring!
                while let Some(msg) = ws.next().await {
                    if let Ok(msg) = msg {
                        ws.send(msg).await.ok();
                    }
                }
            }).await.ok();
        });
    }
}
```

### The io_uring + HTTP/2 Stack

```
┌─────────────────────────────┐
│     WebSocket Messages      │  ← Your application code
├─────────────────────────────┤
│   WebSocketStream<H2Stream> │  ← sockudo-ws
├─────────────────────────────┤
│      HTTP/2 (h2 crate)      │  ← Extended CONNECT framing
├─────────────────────────────┤
│     TLS (rustls/openssl)    │  ← Required for HTTP/2
├─────────────────────────────┤
│        UringStream          │  ← io_uring async I/O
├─────────────────────────────┤
│      TCP (kernel)           │  ← io_uring submission queue
└─────────────────────────────┘
```

## Unified API

All transports use the same `WebSocketStream<S>` API:

```rust
// HTTP/1.1 (default)
let ws = WebSocketStream::server(tcp_stream, config);

// HTTP/2
let ws = WebSocketStream::server(h2_stream, config);

// HTTP/3
let ws = WebSocketStream::server(h3_stream, config);

// io_uring
let ws = WebSocketStream::server(uring_stream, config);

// Same message loop for all!
while let Some(msg) = ws.next().await {
    ws.send(msg?).await?;
}
```

## Configuration

### Basic Configuration

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

### HTTP/2 Configuration

```rust
let config = Config::builder()
    .http2_window_size(1024 * 1024)        // 1MB stream window
    .http2_connection_window_size(2 * 1024 * 1024)  // 2MB connection window
    .http2_max_streams(100)                // Max concurrent streams
    .build();
```

### HTTP/3 Configuration

```rust
let config = Config::builder()
    .http3_idle_timeout(30_000)            // 30 second idle timeout
    .build();
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

## Feature Flags

| Feature | Description |
|---------|-------------|
| `simd` | SIMD acceleration (default) |
| `tokio-runtime` | Tokio async runtime (default) |
| `permessage-deflate` | Compression support (default) |
| `axum-integration` | Axum web framework support |
| `http2` | HTTP/2 WebSocket (RFC 8441) |
| `http3` | HTTP/3 WebSocket (RFC 9220) |
| `io-uring` | Linux io_uring support |
| `all-transports` | All transport features |
| `full` | Everything |

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

### Unit Tests

```bash
cargo test
```

### With Features

```bash
cargo test --features http2
cargo test --features http3
cargo test --features full
```

### Autobahn Test Suite

```bash
cd autobahn

# Build and run server + tests
make test

# Or manually:
make run  # Start server
# Then run Autobahn client in another terminal
```

## Examples

Run the examples:

```bash
# Basic echo server
cargo run --example simple_echo

# Split streams (concurrent read/write)
cargo run --example split_echo

# Axum integration
cargo run --example axum_echo

# HTTP/2 WebSocket server
cargo run --example http2_echo --features http2

# HTTP/3 WebSocket server
cargo run --example http3_echo --features http3
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
│   ├── simd.rs         # SIMD feature detection
│   ├── http2/          # HTTP/2 WebSocket (RFC 8441)
│   │   ├── mod.rs
│   │   ├── stream.rs   # H2Stream wrapper
│   │   ├── handshake.rs
│   │   ├── server.rs   # H2WebSocketServer
│   │   └── client.rs   # H2WebSocketClient
│   ├── http3/          # HTTP/3 WebSocket (RFC 9220)
│   │   ├── mod.rs
│   │   ├── stream.rs   # H3Stream wrapper
│   │   ├── handshake.rs
│   │   ├── server.rs   # H3WebSocketServer
│   │   └── client.rs   # H3WebSocketClient
│   └── io_uring/       # Linux io_uring transport
│       ├── mod.rs
│       ├── stream.rs   # UringStream wrapper
│       └── buffer.rs   # Registered buffer pool
├── examples/
│   ├── simple_echo.rs  # Basic echo server
│   ├── split_echo.rs   # Concurrent read/write
│   ├── axum_echo.rs    # Axum integration
│   ├── http2_echo.rs   # HTTP/2 WebSocket server
│   └── http3_echo.rs   # HTTP/3 WebSocket server
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
6. **io_uring**: Kernel-level async I/O with submission queue batching

## License

MIT

## Credits

- Inspired by [uWebSockets](https://github.com/uNetworking/uWebSockets)
- HTTP/2 support via [h2](https://github.com/hyperium/h2)
- HTTP/3 support via [quinn](https://github.com/quinn-rs/quinn) and [h3](https://github.com/hyperium/h3)
- io_uring support via [tokio-uring](https://github.com/tokio-rs/tokio-uring)
