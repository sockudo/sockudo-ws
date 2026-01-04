# sockudo-ws

Ultra-low latency WebSocket library for Rust, designed for high-frequency trading (HFT) applications and real-time systems. Fully compatible with Tokio and Axum.

Will be used in [Sockudo](https://github.com/RustNSparks/sockudo), a high-performance Pusher-compatible WebSocket server.

## Performance

### Rust WebSocket Libraries Benchmark

Benchmarked using [web-socket-benchmark](https://github.com/nurmohammed840/web-socket-benchmark) (100,000 iterations of "Hello, World!" message):

| Library | Send | Echo | Recv | **Total** |
|---------|------|------|------|-----------|
| **sockudo-ws** | **1.2ms** | **5.0ms** | **3.1ms** | **10.2ms** |
| fastwebsockets | 3.3ms | 5.7ms | 3.0ms | 12.0ms |
| web-socket | 2.1ms | 6.8ms | 3.3ms | 12.2ms |
| soketto | 5.8ms | 17.6ms | 9.7ms | 33.1ms |
| tokio-tungstenite | 6.4ms | 18.2ms | 10.2ms | 34.8ms |

**sockudo-ws is ~17% faster than the next fastest Rust WebSocket library!**

<details>
<summary><b>How to reproduce</b></summary>

```bash
# Clone the benchmark repository
git clone https://github.com/nurmohammed840/web-socket-benchmark
cd web-socket-benchmark

# Run benchmarks
cargo bench
```

The benchmark measures:
- **Send**: Time to send 100,000 "Hello, World!" messages from client to server
- **Echo**: Time to send and receive 100,000 messages (round-trip)
- **Recv**: Time to receive 100,000 messages from server to client

Environment: AMD Ryzen 9 7950X, 32GB RAM, Linux 6.18, Rust 1.82

</details>

### vs uWebSockets (C++)

Benchmarked against [uWebSockets](https://github.com/uNetworking/uWebSockets), the industry standard for high-performance WebSockets:

| Test Case | sockudo-ws | uWebSockets | Ratio |
|-----------|------------|-------------|-------|
| 512 bytes, 100 connections | 232,712 msg/s | 227,973 msg/s | **1.02x** |
| 1024 bytes, 100 connections | 232,072 msg/s | 224,498 msg/s | **1.03x** |
| 512 bytes, 500 connections | 231,135 msg/s | 222,493 msg/s | **1.03x** |
| 1024 bytes, 500 connections | 222,578 msg/s | 216,833 msg/s | **1.02x** |

<details>
<summary><b>How to reproduce</b></summary>

**sockudo-ws benchmark:**

```bash
cd sockudo-ws
cargo build --release --example echo_server
./target/release/examples/echo_server &

# Using websocket-bench (https://github.com/anycable/websocket-bench)
websocket-bench broadcast ws://127.0.0.1:8080 \
  --concurrent 100 \
  --sample-size 100000 \
  --payload-size 512 \
  --step-size 100
```

**uWebSockets benchmark:**

```bash
# Build uWebSockets echo server
git clone https://github.com/uNetworking/uWebSockets
cd uWebSockets
make
./EchoServer &

# Run same benchmark
websocket-bench broadcast ws://127.0.0.1:9001 \
  --concurrent 100 \
  --sample-size 100000 \
  --payload-size 512 \
  --step-size 100
```

Environment: AMD Ryzen 9 7950X, 32GB RAM, Linux 6.18, Rust 1.82, uWebSockets v20.64

</details>

sockudo-ws matches or exceeds uWebSockets performance while providing a safe, ergonomic Rust API.

## Features

- **SIMD Acceleration**: AVX2/AVX-512/SSE2/NEON/AltiVec/LSX for frame masking and UTF-8 validation
- **Zero-Copy Parsing**: Direct buffer access without intermediate allocations
- **Write Batching (Corking)**: Minimizes syscalls via vectored I/O
- **permessage-deflate**: Full compression support with shared/dedicated compressors
- **Lock-Free Split Streams**: True concurrent read/write using OS-level stream splitting (zero mutex contention)
- **Pub/Sub System**: High-performance topic-based messaging with sender exclusion
- **HTTP/2 WebSocket**: RFC 8441 Extended CONNECT protocol support
- **HTTP/3 WebSocket**: RFC 9220 WebSocket over QUIC support
- **io_uring**: Linux high-performance async I/O (combinable with HTTP/2 and HTTP/3)
- **Autobahn Compliant**: Passes all 517 Autobahn test suite cases
- **Fuzz Tested**: Comprehensive fuzzing with libFuzzer

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

# With TLS (rustls)
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["rustls-webpki-roots"] }

# With TLS (native-tls)
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["native-tls"] }

# All transports
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["all-transports"] }

# Everything
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["full"] }

# With mimalloc allocator (recommended for production)
sockudo-ws = { git = "https://github.com/RustNSparks/sockudo-ws", features = ["mimalloc"] }
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
                ws.send(Message::text(text)).await.unwrap();
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

### Lock-Free Split Streams (Concurrent Read/Write)

sockudo-ws uses **tokio::io::split()** for true concurrent I/O with **zero mutex contention**:

```rust
use sockudo_ws::{Config, Message, WebSocketStream};
use tokio::sync::mpsc;

async fn handle(stream: TcpStream) {
    let ws = WebSocketStream::server(stream, Config::default());
    
    // Split into independent read/write halves
    // Reader and writer can operate 100% concurrently!
    let (mut reader, mut writer) = ws.split();

    // Writer task - NEVER blocks reader
    let (tx, mut rx) = mpsc::channel::<Message>(32);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            writer.send(msg).await.unwrap();
        }
    });

    // Reader loop - NEVER blocks writer
    while let Some(msg) = reader.next().await {
        match msg.unwrap() {
            Message::Text(text) => {
                tx.send(Message::text(text)).await.unwrap();
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}
```

**Why This is Fast:**
- ✅ **Zero mutex contention** - reader and writer operate independently
- ✅ **OS-level splitting** - leverages tokio's optimized `ReadHalf` and `WriteHalf`
- ✅ **True concurrency** - can read and write simultaneously without blocking
- ✅ **Control frame coordination** - Ping/Pong/Close handled via lightweight mpsc channel

### Pub/Sub System

sockudo-ws includes a high-performance pub/sub system for topic-based messaging, inspired by uWebSockets/Bun:

```rust
use sockudo_ws::pubsub::PubSub;
use sockudo_ws::Message;
use tokio::sync::mpsc;

// Create pub/sub system
let pubsub = PubSub::new();

// Create a subscriber with a message channel
let (tx, mut rx) = mpsc::unbounded_channel();
let subscriber_id = pubsub.create_subscriber(tx);

// Subscribe to topics
pubsub.subscribe(subscriber_id, "chat/general");
pubsub.subscribe(subscriber_id, "notifications");

// Publish to all subscribers
let msg = Message::text("Hello everyone!");
pubsub.publish("chat/general", msg);

// Publish excluding a specific subscriber (useful for echo prevention)
let msg = Message::text("Broadcast from user");
pubsub.publish_excluding(subscriber_id, "chat/general", msg);

// Unsubscribe from a topic
pubsub.unsubscribe(subscriber_id, "chat/general");

// Remove subscriber when connection closes
pubsub.remove_subscriber(subscriber_id);
```

#### Pusher-Style Socket IDs

```rust
use sockudo_ws::pubsub::PubSub;

let pubsub = PubSub::new();

// Generate Pusher-style socket ID (format: "1234567890.9876543210")
let socket_id = PubSub::generate_socket_id();

// Create subscriber with custom socket ID
let (tx, rx) = mpsc::unbounded_channel();
let subscriber_id = pubsub.create_subscriber_with_id(&socket_id, tx);

// Subscribe/publish using socket ID
pubsub.subscribe_by_socket_id(&socket_id, "private-channel");
pubsub.publish_excluding_socket_id(&socket_id, "chat", Message::text("Hello"));

// Lookup subscriber by socket ID
if let Some(id) = pubsub.get_subscriber_by_socket_id(&socket_id) {
    println!("Found subscriber: {:?}", id);
}
```

#### Pub/Sub Features

- **64 Sharded Topics**: Reduced lock contention for high concurrency
- **Lock-Free Subscriber IDs**: Atomic allocation for fast subscriber creation
- **Zero-Copy Messages**: Uses `Bytes` for efficient message sharing
- **Cache-Line Alignment**: Prevents false sharing in concurrent access
- **Pusher-Style String IDs**: Optional string-based subscriber identifiers
- **Sender Exclusion**: `publish_excluding()` prevents echo to the sender
- **Automatic Cleanup**: Empty topics are removed automatically

#### Pub/Sub Statistics

```rust
// Get statistics
let topic_count = pubsub.topic_count();
let subscriber_count = pubsub.subscriber_count();
let messages_published = pubsub.messages_published();
let subscribers_in_topic = pubsub.topic_subscriber_count("chat/general");
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
                    Message::Text(text) => { ws.send(Message::text(text)).await.ok(); }
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
use sockudo_ws::{WebSocketServer, Http2, Config, Message};
use futures_util::{SinkExt, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080").await?;
    
    let config = Config::builder()
        .http2_max_streams(100)
        .build();
    
    let server = WebSocketServer::<Http2>::new(config);

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
use sockudo_ws::{WebSocketClient, Http2, Config, Message};

let client = WebSocketClient::<Http2>::new(Config::default());
let mut ws = client.connect(tls_stream, "wss://example.com/ws", None).await?;

ws.send(Message::text("Hello!")).await?;
```

### HTTP/2 Multiplexed Connections

Open multiple WebSocket streams over a single HTTP/2 connection:

```rust
use sockudo_ws::{WebSocketClient, Http2, Config};

let client = WebSocketClient::<Http2>::new(Config::default());
let mut conn = client.connect_multiplexed(tls_stream).await?;

// Open multiple WebSocket streams on the same connection
let mut ws1 = conn.open_websocket("wss://example.com/chat", None).await?;
let mut ws2 = conn.open_websocket("wss://example.com/notifications", None).await?;
```

## HTTP/3 WebSocket (RFC 9220)

HTTP/3 WebSocket runs over QUIC, providing benefits like 0-RTT, no head-of-line blocking, and better mobile performance.

```rust
use sockudo_ws::{WebSocketServer, Http3, Config, Message};
use futures_util::{SinkExt, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load TLS certificates (required for QUIC)
    let tls_config = load_server_tls_config()?;
    
    let ws_config = Config::builder()
        .http3_idle_timeout(30_000)
        .build();

    let server = WebSocketServer::<Http3>::bind(
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
use sockudo_ws::{WebSocketServer, Http2, Config};

#[tokio_uring::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:8080".parse()?)?;
    let server = WebSocketServer::<Http2>::new(Config::default());

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

### Core Features

| Feature | Default | Description |
|---------|---------|-------------|
| `simd` | ✅ | SIMD acceleration for masking and UTF-8 |
| `tokio-runtime` | ✅ | Tokio async runtime support |
| `permessage-deflate` | ✅ | Compression support (RFC 7692) |
| `fastrand` | ✅ | Fast PRNG for client mask generation |

### SIMD Features

| Feature | Description |
|---------|-------------|
| `avx2` | Enable AVX2 (256-bit SIMD) |
| `avx512` | Enable AVX-512 (512-bit SIMD) |
| `neon` | Enable ARM NEON |
| `nightly` | Enable additional SIMD on arm, loongarch64, powerpc, s390x |

### TLS Features

| Feature | Description |
|---------|-------------|
| `native-tls` | TLS via tokio-native-tls |
| `rustls-webpki-roots` | TLS via tokio-rustls with webpki-roots |
| `rustls-native-roots` | TLS via tokio-rustls with native root certificates |
| `rustls-platform-verifier` | TLS via tokio-rustls with platform verifier |

### SHA-1 Implementations

At least one SHA-1 implementation is required for the WebSocket handshake:

| Feature | Description |
|---------|-------------|
| `ring` | SHA-1 via ring (recommended with rustls) |
| `aws_lc_rs` | SHA-1 via AWS LC |
| `openssl` | SHA-1 via OpenSSL (recommended with native-tls) |
| `sha1_smol` | Pure Rust SHA-1 fallback |

### Random Number Generators

For client mask generation:

| Feature | Description |
|---------|-------------|
| `fastrand` | Fast PRNG (default) |
| `getrandom` | Cryptographically secure RNG |
| `rand_rng` | Use rand crate |

### Transport Features

| Feature | Description |
|---------|-------------|
| `http2` | HTTP/2 WebSocket (RFC 8441) |
| `http3` | HTTP/3 WebSocket (RFC 9220) |
| `io-uring` | Linux io_uring support |
| `all-transports` | All transport features |

### Allocator Features

| Feature | Description |
|---------|-------------|
| `mimalloc` | Use mimalloc as global allocator (10-30% throughput improvement) |

### Integration Features

| Feature | Description |
|---------|-------------|
| `axum-integration` | Axum web framework support |
| `full` | All features enabled |

## SIMD Architecture Support

sockudo-ws uses SIMD acceleration for frame masking and UTF-8 validation:

| Architecture | Instructions | Masking | UTF-8 | Stable | Nightly |
|--------------|--------------|---------|-------|--------|---------|
| x86_64 | SSE2 | ✅ | ✅ | ✅ | ✅ |
| x86_64 | SSE4.2 | ✅ | ✅ | ✅ | ✅ |
| x86_64 | AVX2 | ✅ | ✅ | ✅ | ✅ |
| x86_64 | AVX-512 | ✅ | ✅ | ✅ | ✅ |
| aarch64 | NEON | ✅ | ✅ | ✅ | ✅ |
| arm | NEON | ✅ | ✅ | ❌ | ✅ |
| loongarch64 | LSX | ✅ | ✅* | ❌ | ✅ |
| loongarch64 | LASX | ✅ | ✅* | ❌ | ✅ |
| powerpc | AltiVec | ✅ | ✅* | ❌ | ✅ |
| powerpc64 | AltiVec | ✅ | ✅* | ❌ | ✅ |
| s390x | z13 vectors | ✅ | ✅* | ❌ | ✅ |

*Custom SIMD UTF-8 validation with ASCII fast-path (requires `nightly` feature).

UTF-8 validation uses:
- [simdutf8](https://github.com/rusticstuff/simdutf8) for x86_64 (SSE4.2, AVX2, AVX-512), aarch64 (NEON), arm (NEON), wasm32
- Custom SIMD implementations for LoongArch64, PowerPC, and s390x (with `nightly` feature)

## API Reference

### WebSocketStream

The main WebSocket type implementing `Stream` + `Sink`:

```rust
// Create server-side stream
let ws = WebSocketStream::server(tcp_stream, config);

// Create client-side stream
let ws = WebSocketStream::client(tcp_stream, config);

// Send messages
ws.send(Message::text("hello")).await?;
ws.send(Message::binary(bytes)).await?;

// Receive messages
while let Some(msg) = ws.next().await {
    // handle msg
}

// Close connection
ws.close(1000, "goodbye").await?;

// Backpressure handling
if ws.is_backpressured() {
    // Write buffer is full, consider slowing down
}
```

### Lock-Free Split Streams

For concurrent read/write operations with zero mutex contention:

```rust
let (reader, writer) = ws.split();

// SplitReader - operates independently, never blocks writer
reader.next().await  // Receive message
reader.is_closed()   // Check if closed (non-blocking)

// SplitWriter - operates independently, never blocks reader
writer.send(msg).await?;
writer.send_text("hello").await?;
writer.send_binary(bytes).await?;
writer.close(1000, "bye").await?;
writer.is_closed()   // Check if closed (non-blocking)
writer.flush().await?;  // Flush pending control responses
```

**Implementation Details:**
- Uses `tokio::io::split()` for OS-level stream splitting
- Reader owns `ReadHalf<S>` and protocol decoder
- Writer owns `WriteHalf<S>` and protocol encoder
- Control frames (Ping/Pong/Close) coordinated via mpsc channel
- No shared mutex - true concurrent I/O

### Message Types

```rust
pub enum Message {
    Text(Bytes),      // Zero-copy, UTF-8 validated
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<CloseReason>),
}

// Create messages
let text_msg = Message::text("hello");           // From &str
let text_msg = Message::text(String::from("hello")); // From String
let binary_msg = Message::binary(vec![1, 2, 3]); // From Vec<u8>

// Access text content
if let Message::Text(bytes) = msg {
    let text: &str = msg.as_text().unwrap(); // Returns Option<&str>
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

## Fuzzing

sockudo-ws includes fuzz targets for security testing:

```bash
# Install cargo-fuzz
cargo install cargo-fuzz

# Run fuzzing (requires nightly)
cd fuzz
cargo +nightly fuzz run parse_frame
cargo +nightly fuzz run unmask
cargo +nightly fuzz run utf8_validation
cargo +nightly fuzz run protocol
```

### Fuzz Targets

| Target | Description |
|--------|-------------|
| `parse_frame` | WebSocket frame parsing with arbitrary bytes |
| `unmask` | SIMD masking/unmasking operations |
| `utf8_validation` | UTF-8 validation consistency with std |
| `protocol` | Frame encoding/decoding round-trip |

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
│   ├── lib.rs            # Public API, Config
│   ├── stream/           # WebSocket stream types
│   │   ├── mod.rs
│   │   ├── websocket.rs  # WebSocketStream, Split types
│   │   └── transport_stream.rs
│   ├── protocol.rs       # WebSocket protocol state machine
│   ├── frame.rs          # Frame encoding/decoding
│   ├── handshake.rs      # HTTP upgrade handshake
│   ├── simd.rs           # SIMD masking (AVX/SSE/NEON/AltiVec/LSX)
│   ├── utf8.rs           # SIMD UTF-8 validation
│   ├── cork.rs           # Write batching buffer
│   ├── deflate.rs        # permessage-deflate compression
│   ├── error.rs          # Error types with categorization
│   ├── transport.rs      # Transport trait (Http1, Http2, Http3)
│   ├── server.rs         # WebSocketServer<T: Transport>
│   ├── client.rs         # WebSocketClient<T: Transport>
│   ├── multiplex.rs      # MultiplexedConnection
│   ├── extended_connect.rs # Shared Extended CONNECT logic
│   ├── http2/            # HTTP/2 WebSocket (RFC 8441)
│   │   ├── mod.rs
│   │   └── stream.rs     # Http2Stream wrapper
│   ├── http3/            # HTTP/3 WebSocket (RFC 9220)
│   │   ├── mod.rs
│   │   └── stream.rs     # Http3Stream wrapper
│   └── io_uring/         # Linux io_uring transport
│       ├── mod.rs
│       ├── stream.rs     # UringStream wrapper
│       └── buffer.rs     # Registered buffer pool
├── fuzz/                 # Fuzzing targets
│   └── fuzz_targets/
│       ├── parse_frame.rs
│       ├── unmask.rs
│       ├── utf8_validation.rs
│       └── protocol.rs
├── examples/
│   ├── simple_echo.rs    # Basic echo server
│   ├── split_echo.rs     # Concurrent read/write
│   ├── axum_echo.rs      # Axum integration
│   ├── http2_echo.rs     # HTTP/2 WebSocket server
│   └── http3_echo.rs     # HTTP/3 WebSocket server
├── autobahn/
│   ├── server.rs         # Autobahn test server
│   └── Makefile          # Build and test automation
└── benches/
    └── throughput.rs     # Criterion benchmarks
```

## Performance Optimizations

1. **SIMD Masking**: Uses AVX2/AVX-512/SSE2/NEON/AltiVec/LSX to XOR mask frames at 16-64 bytes per cycle
2. **SIMD UTF-8**: Validates UTF-8 text at memory bandwidth speeds via simdutf8
3. **Zero-Copy**: Parses frames directly from receive buffer without copying
4. **Cork Buffer**: Batches small writes into 16KB chunks for fewer syscalls
5. **Vectored I/O**: Uses `writev()` to send multiple buffers in single syscall
6. **io_uring**: Kernel-level async I/O with submission queue batching
7. **Alignment-Aware SIMD**: Handles unaligned prefix/suffix for optimal memory access
8. **Optional mimalloc**: High-performance allocator for reduced allocation latency

## License

MIT

## Credits

sockudo-ws incorporates ideas and techniques from several excellent WebSocket libraries:

- **[uWebSockets](https://github.com/uNetworking/uWebSockets)** - The industry standard for high-performance WebSockets. Inspired the cork/batch writing strategy and overall performance-first design philosophy.

- **[tokio-websockets](https://github.com/Gelbpunkt/tokio-websockets)** - A well-designed Tokio-native WebSocket library. Borrowed several optimizations including:
  - Masked frame fast path for small client frames
  - Alignment-aware SIMD implementations
  - Multi-architecture SIMD support (LoongArch64 LSX/LASX, PowerPC AltiVec, s390x z13, ARM NEON)
  - Feature flag organization (TLS variants, SHA-1 options, RNG options)
  - Fuzzing infrastructure

- **[fastwebsockets](https://github.com/denoland/fastwebsockets)** - Deno's high-performance WebSocket library. Referenced for fuzzing patterns and frame parsing optimizations.

- **[h2](https://github.com/hyperium/h2)** - HTTP/2 implementation used for RFC 8441 support

- **[quinn](https://github.com/quinn-rs/quinn)** and **[h3](https://github.com/hyperium/h3)** - QUIC and HTTP/3 implementations used for RFC 9220 support

- **[tokio-uring](https://github.com/tokio-rs/tokio-uring)** - io_uring integration for Linux

- **[simdutf8](https://github.com/rusticstuff/simdutf8)** - Battle-tested SIMD UTF-8 validation (used by simd-json, polars, arrow)
