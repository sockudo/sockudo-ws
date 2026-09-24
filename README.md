# sockudo-ws

Ultra-low latency WebSocket library for Rust, designed for high-frequency trading (HFT) applications and real-time systems. Fully compatible with Tokio, Compio, and Axum.

Will be used in [Sockudo](https://github.com/sockudo/sockudo), a high-performance Pusher-compatible WebSocket server.

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

# Add sockudo-ws to the benchmark suite, then run:
RUSTFLAGS="-C target-cpu=native" cargo bench
```

The benchmark measures:
- **Send**: Time to send 100,000 "Hello, World!" messages from client to server
- **Echo**: Time to send and receive 100,000 messages (round-trip)
- **Recv**: Time to receive 100,000 messages from server to client

Environment: AMD Ryzen 9 7950X, 32GB RAM, Linux 6.18, Rust 1.82

</details>

### vs uWebSockets (C++)

Benchmarked against [uWebSockets](https://github.com/uNetworking/uWebSockets), the industry standard for high-performance WebSockets, using the [fastwebsockets benchmark suite](https://github.com/denoland/fastwebsockets/tree/main/benches):

| Test Case | sockudo-ws | uWebSockets | Ratio |
|-----------|------------|-------------|-------|
| 512 bytes, 100 connections | 232,712 msg/s | 227,973 msg/s | **1.02x** |
| 1024 bytes, 100 connections | 232,072 msg/s | 224,498 msg/s | **1.03x** |
| 512 bytes, 500 connections | 231,135 msg/s | 222,493 msg/s | **1.03x** |
| 1024 bytes, 500 connections | 222,578 msg/s | 216,833 msg/s | **1.02x** |

<details>
<summary><b>How to reproduce</b></summary>

```bash
# Clone the fastwebsockets repository
git clone https://github.com/denoland/fastwebsockets
cd fastwebsockets/benches

# Follow the instructions in the benchmark directory to run
# the comparison between sockudo-ws and uWebSockets
```

Environment: AMD Ryzen 9 7950X, 32GB RAM, Linux 6.18, Rust 1.82, uWebSockets v20.64

</details>

sockudo-ws matches or exceeds uWebSockets performance while providing a safe, ergonomic Rust API.

## Features

- **SIMD Frame Masking**: Architecture-specific AVX2/AVX-512/SSE2/NEON/AltiVec/LSX/LASX/z13 implementations
- **UTF-8 Validation**: `simdutf8` acceleration on supported x86, AArch64, and wasm32 targets, with a portable validator elsewhere
- **Zero-Copy Parsing**: Direct buffer access without intermediate allocations
- **Write Batching (Corking)**: Minimizes syscalls via vectored I/O
- **permessage-deflate**: Full compression support with shared/dedicated compressors
- **Lock-Free Split Streams**: True concurrent read/write using OS-level stream splitting (zero mutex contention)
- **Runtime-Separated APIs**: Tokio support via `tokio-runtime`, native Compio support via `compio-runtime`
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
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws" }

# With compression
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["permessage-deflate"] }

# Default Tokio runtime with HTTP/2 support
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["http2"] }

# Default Tokio runtime with HTTP/3 support
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["http3"] }

# Tokio runtime without default features
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", default-features = false, features = ["tokio-runtime", "http2", "fastrand"] }
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", default-features = false, features = ["tokio-runtime", "http3", "fastrand"] }

# With io_uring (Linux only)
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["io-uring"] }

# With native Compio runtime support
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", default-features = false, features = ["compio-runtime", "fastrand"] }

# With Compio and HTTP/2 or HTTP/3 transports
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", default-features = false, features = ["compio-runtime", "http2", "fastrand"] }
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", default-features = false, features = ["compio-runtime", "http3", "fastrand"] }

# With TLS (rustls)
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["rustls-webpki-roots"] }
rustls = { version = "0.23", default-features = false, features = [
    "aws-lc-rs",
    "logging",
    "std",
    "tls12",
] }

# With TLS (native-tls)
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["native-tls"] }

# All transports
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["all-transports"] }

# Everything
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["full"] }

# With mimalloc allocator (recommended for production)
sockudo-ws = { git = "https://github.com/sockudo/sockudo-ws", features = ["mimalloc"] }
```

### Runtime and Transport Features

Runtime selection is independent from protocol selection. Enable exactly the runtime API you want, then add `http2` and/or `http3` for extended CONNECT transports.

| Runtime feature | HTTP/1.1 WebSocket | HTTP/2 WebSocket | HTTP/3 WebSocket |
|-----------------|--------------------|------------------|------------------|
| `tokio-runtime` | `WebSocketStream` / `WebSocketClient<Http1>` | `WebSocketClient<Http2>`, `WebSocketServer<Http2>`, `MultiplexedConnection<Http2>` | `WebSocketClient<Http3>`, `WebSocketServer<Http3>`, `MultiplexedConnection<Http3>` |
| `compio-runtime` | `compio::accept_async`, `compio::connect_async` | `compio::serve_http2`, `compio::connect_http2`, `compio::connect_http2_multiplexed` | `compio::CompioHttp3Server`, `compio::connect_http3`, `compio::connect_http3_multiplexed` |

The `http2` and `http3` features do not select a runtime. For example, `default-features = false, features = ["compio-runtime", "http2"]` builds Compio HTTP/2 support without enabling the Tokio runtime API.

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

### HTTP/1.1 Custom Headers

HTTP/1.1 clients can attach application headers to the WebSocket upgrade request. Sockudo validates
header syntax and rejects attempts to override handshake-managed headers such as `Host`, `Upgrade`,
and `Sec-WebSocket-Key`.

```rust
use sockudo_ws::{Config, Http1};
use sockudo_ws::client::WebSocketClient;
use tokio::net::TcpStream;

async fn connect() -> sockudo_ws::Result<()> {
    let stream = TcpStream::connect("example.com:80").await?;
    let headers = vec![
        ("Authorization".to_string(), "Bearer token".to_string()),
        ("User-Agent".to_string(), "my-client".to_string()),
    ];
    let client = WebSocketClient::<Http1>::new(Config::default());
    let (_websocket, _handshake) = client
        .connect_with_headers(
            stream,
            "example.com",
            "/ws",
            None,
            Some(&headers),
        )
        .await?;
    Ok(())
}
```

Compio applications can use `sockudo_ws::compio::connect_async_with_headers` with the same header
representation and validation rules.

### Native Compio Runtime

Compio uses completion-based I/O, so sockudo-ws exposes a native async-method API behind `compio-runtime`:

```rust
use sockudo_ws::compio::{accept_async, connect_async, net::{TcpListener, TcpStream}};
use sockudo_ws::{Config, Message};

#[sockudo_ws::compio::main]
async fn main() -> sockudo_ws::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9001").await?;

    let server = sockudo_ws::compio::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut ws, _) = accept_async(stream, Config::default()).await.unwrap();

        while let Some(msg) = ws.next().await {
            let msg = msg.unwrap();
            if matches!(msg, Message::Close(_)) {
                break;
            }
            ws.send(msg).await.unwrap();
        }
    });

    let stream = TcpStream::connect("127.0.0.1:9001").await?;
    let (mut ws, _) = connect_async(
        stream,
        "127.0.0.1:9001",
        "/",
        None,
        Config::default(),
    )
    .await?;
    ws.send_text("hello").await?;
    let _ = ws.next().await;

    server.await.unwrap();
    Ok(())
}
```

### Compio HTTP/2 and HTTP/3

Compio uses the same `http2` and `http3` transport features as Tokio. Only the runtime feature changes.

```rust
use sockudo_ws::compio::{
    connect_http2, serve_http2,
    net::{TcpListener, TcpStream},
};
use sockudo_ws::Config;

#[sockudo_ws::compio::main]
async fn main() -> sockudo_ws::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:9002").await?;
    let addr = listener.local_addr()?;

    let server = sockudo_ws::compio::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_http2(stream, Config::default(), |mut ws, _req| async move {
            if let Some(Ok(msg)) = ws.next().await {
                ws.send(msg).await.unwrap();
            }
        })
        .await
        .unwrap();
    });

    let stream = TcpStream::connect(addr).await?;
    let mut ws = connect_http2(
        stream,
        &format!("https://localhost:{}/chat", addr.port()),
        None,
        Config::default(),
    )
    .await?;

    ws.send_text("hello over h2").await?;
    let _ = ws.next().await;

    server.await.unwrap();
    Ok(())
}
```

```rust
use sockudo_ws::compio::{CompioHttp3Server, connect_http3_multiplexed};
use sockudo_ws::Config;

async fn http3_example(
    server_addr: std::net::SocketAddr,
    server_tls: rustls::ServerConfig,
    client_tls: rustls::ClientConfig,
) -> sockudo_ws::Result<()> {
    let server = CompioHttp3Server::bind(server_addr, server_tls, Config::default()).await?;
    let addr = server.local_addr()?;

    let server_task = sockudo_ws::compio::runtime::spawn(async move {
        server
            .serve(|mut ws, _req| async move {
                if let Some(Ok(msg)) = ws.next().await {
                    ws.send(msg).await.unwrap();
                }
            })
            .await
            .unwrap();
    });

    let mut mux = connect_http3_multiplexed(addr, "localhost", client_tls, Config::default()).await?;
    let mut chat = mux.open_websocket("/chat", None).await?;
    let mut notifications = mux.open_websocket("/notifications", None).await?;

    chat.send_text("hello h3").await?;
    notifications.send_text("ping").await?;

    mux.close();
    server_task.await.unwrap();
    Ok(())
}
```

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

HTTP/3 WebSocket runs over QUIC, with independent streams and connection migration. Built-in endpoints apply the configured QUIC transport settings and reject `enable_0rtt = true`; TLS resumption does not enable early data.

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
| TLS connection resumption | Reuse session state without 0-RTT early data |
| Better mobile performance | Handles network changes gracefully |
| Multiple streams per connection | Efficient multiplexing |

## io_uring Support (Linux)

io_uring provides completion-based kernel I/O. `UringStream` is a buffered TCP transport bridge: its poll-based API copies between borrowed buffers and owned completion buffers. HTTP/3 uses a separate UDP transport. Run inside `tokio_uring::start` on Linux 5.10 or later.

### io_uring with HTTP/1.1

```rust
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::io_uring::UringStream;
use sockudo_ws::{Config, WebSocketStream};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio_uring::start(async {
        let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:8080".parse()?)?;
        loop {
            let (tcp, _) = listener.accept().await?;
            // Wrap TCP in UringStream for io_uring I/O.
            let uring = UringStream::new(tcp);
            tokio_uring::spawn(async move {
                // Direct WebSocket framing; an HTTP upgrade must be handled separately.
                let mut ws = WebSocketStream::server(uring, Config::default());
                while let Some(Ok(message)) = ws.next().await {
                    if ws.send(message).await.is_err() {
                        break;
                    }
                }
            });
        }
    })
}
```

### io_uring with HTTP/2

Enable `io-uring`, `http2`, and a `rustls-*` feature for this TLS example. Supply a TLS acceptor configured with a certificate and ALPN `h2`:

```rust
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::io_uring::UringStream;
use sockudo_ws::{Config, Http2, WebSocketServer};

// Configure the TLS acceptor with a certificate and ALPN protocol h2.
fn serve(tls_acceptor: tokio_rustls::TlsAcceptor) -> Result<(), Box<dyn std::error::Error>> {
    tokio_uring::start(async move {
        let listener = tokio_uring::net::TcpListener::bind("127.0.0.1:8443".parse()?)?;
        let server = WebSocketServer::<Http2>::new(Config::default());
        loop {
            let (tcp, _) = listener.accept().await?;
            // Wrap TCP in UringStream for io_uring I/O.
            let uring = UringStream::new(tcp);
            // Add TLS for this HTTP/2 endpoint: TCP -> TLS -> HTTP/2 -> WebSocket.
            let tls = tls_acceptor.accept(uring).await?;
            let server = server.clone();
            tokio_uring::spawn(async move {
                server.serve(tls, |mut ws, _req| async move {
                    // Handle WebSocket over HTTP/2 over TLS over io_uring.
                    while let Some(Ok(message)) = ws.next().await {
                        if ws.send(message).await.is_err() {
                            break;
                        }
                    }
                }).await
            });
        }
    })
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
│     TLS (rustls/openssl)    │  ← TLS for this endpoint
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
let ws = WebSocketStream::server(h2_stream, config).with_immediate_write_shutdown();

// HTTP/3
let ws = WebSocketStream::server(h3_stream, config).with_immediate_write_shutdown();

// io_uring
let ws = WebSocketStream::server(uring_stream, config);

// Same message loop for all!
while let Some(msg) = ws.next().await {
    ws.send(msg?).await?;
}
```

The built-in HTTP/2 and HTTP/3 client/server entry points enable immediate send-side shutdown automatically. When constructing a unified stream directly over a multiplexed transport, call `with_immediate_write_shutdown()` so `close()` sends END_STREAM after the WebSocket Close frame. TCP/TLS streams keep their write half open until the peer's Close.

Unified closing uses one absolute `close_timeout` budget, starting when a local Close is queued or a peer Close is received. Continue polling `next()` after local `close()` to receive the peer's response; a silent peer produces `ConnectionClosed` once, then the stream ends. Crossing Pings do not reset the budget. Cleanup errors or expiration do not replace an accepted Close or an existing idle/Pong timeout. Deadline expiry alone leaves parsed messages deliverable in wire order. A control write failure or timeout instead terminates immediately and may discard undelivered Ping/data messages; an accepted Close remains protected. No new Pong is started after expiry or when Close is already accepted. Every budget permits at most one nonwaiting transport read after expiry across subsequent `next()` calls; a cancelled Compio owned read is never restarted. Zero also limits closing writes and shutdown to one poll. This does not guarantee completion or transmission of Close: Compio drivers may require a runtime turn even for an otherwise writable socket. No hidden grace period is added. Compio `next()` is not generally cancellation-safe: cancelling it during Close cleanup can lose that Close; these delivery guarantees assume the receive future is driven to completion.

## Configuration

### Basic Configuration

```rust
use sockudo_ws::{Config, Compression};

let config = Config::builder()
    .compression(Compression::Shared)      // SHARED_COMPRESSOR
    .max_payload_length(16 * 1024)         // 16KB max message
    .ping_interval(30)                     // Ping after 30s inbound inactivity
    .pong_timeout(10)                      // Matching Pong deadline
    .pong_timeout_close(4201, "Pong reply not received in time")
    .idle_timeout(0)                       // Independent hard idle limit disabled
    .close_timeout(5)                      // Bounded Close flush/shutdown
    .max_backpressure(1024 * 1024)         // 1 MiB queued-write threshold
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
| `idle_timeout` | 120s | Hard inbound-idle deadline, independent of Pong detection (0 = disabled) |
| `max_backpressure` | 1 MiB | Soft queued-write threshold: with coalescing enabled, Tokio Sink readiness drains at the smaller of this and the high-water mark (default 64 KiB); not a message-size limit or a disconnect condition (0 = drain any pending output) |
| `write_coalescing` | true | Allow Tokio Sink `feed` batching up to the readiness threshold; false drains any pending output before accepting another frame; `send` and `flush` always flush |
| `auto_ping` | true | Enable proactive native Ping; automatic Pong/Close responses remain enabled when false |
| `ping_interval` | 30s | Inbound inactivity before one native Ping (0 = disabled) |
| `pong_timeout` | 10s | Matching Pong deadline after Ping flush (0 = no deadline and no second Ping until a match) |
| `pong_timeout_close_code` | 1001 | Close code for a missed Pong |
| `pong_timeout_close_reason` | `Pong reply not received in time` | Close reason for a missed Pong |
| `close_timeout` | 5s | Bound for Close handling; Tokio split `close()` includes waiting for the shared sink (0 = one immediate attempt without waiting) |
| `write_buffer_size` | 16KB | Cork buffer size |

### Queued-write backpressure

`feed`, `send_all`, and `forward` may wait when queued output reaches the effective readiness threshold: the smaller of the high-water mark and `max_backpressure` with coalescing enabled, or any pending output with coalescing disabled. A single message may exceed this soft threshold; large messages do not cause a disconnect. A pending drain continues until the transport flush finishes, even if fewer bytes remain than the threshold.

On a unified stream, waiting for writable capacity does not poll the read side, so automatic Pong and inbound heartbeat/idle processing do not advance during that wait. An open connection has no write deadline from `close_timeout`. A peer that never reads can therefore stall a sequential fan-out loop. Monitor `write_buffer_len()` / `is_backpressured()` to choose an application-level slow-consumer policy; these observations do not guarantee that a later send cannot wait. Native `split()` lets reading/control processing progress independently, but sequentially awaiting each split writer still permits head-of-line blocking.

With `write_coalescing=true` (the default), Tokio Sink readiness drains at the smaller of the high-water mark (default 64 KiB) and `max_backpressure` (default 1 MiB). `is_backpressured()` reports whether queued bytes exceed the high-water mark; readiness starts draining when a threshold is reached. With `write_coalescing=false`, readiness drains any pending output before accepting another frame. Zero thresholds never flush an empty buffer merely to become ready.

`SinkExt::send()` and `SinkExt::flush()` always complete the transport flush, even while parsed inbound messages remain unread. Applications previously relying on implicit batching across `send()` calls should use `feed()` and then `flush()` at each batch boundary. Flush before waiting for a reply or pausing reads; the read path also drains output before waiting for more input. A successful flush does not mean the peer has received or processed the message.

For a Tokio unified echo loop, explicitly queue data replies with `feed`. When the parsed input batch is exhausted, `next()` drains queued output before reading more input:

```rust
use futures_util::{SinkExt, StreamExt};
use sockudo_ws::{Message, WebSocketStream};
use tokio::io::{AsyncRead, AsyncWrite};

async fn echo<S>(ws: &mut WebSocketStream<S>) -> sockudo_ws::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        match message? {
            message @ (Message::Text(_) | Message::Binary(_)) => {
                ws.feed(message).await?;
            }
            Message::Close(_) => return Ok(()),
            _ => {} // Ping/Pong are handled by the stream.
        }
    }
    Ok(())
}
```

Do not add an unconditional final `flush()` after Close, EOF or a stream error: the stream may already be closed. If the application stops this loop while the connection is still open, or pauses it to await a database/channel operation, explicitly flush queued replies before that break or await. This example batches replies; keep `send()` when each reply must be flushed before proceeding. Read-batch batching does not guarantee delivery of queued replies if a Close or error terminates the stream.

### Native keepalive semantics

Keepalive is driven by valid inbound activity, not a fixed cadence. After
`ping_interval` with no inbound frame, sockudo-ws writes one RFC 6455 Ping with
an opaque 8-byte nonce. The Pong timer begins only after that Ping is written
and flushed. Only a Pong with the exact payload clears it; unsolicited, stale,
late, or wrong-payload Pongs do not.

Any valid non-Pong inbound frame resets inactivity, including while a Ping is
outstanding, but it does not extend or satisfy that Ping's Pong deadline. If a
hard `idle_timeout` and Pong deadline tie, the more specific Pong timeout wins,
so only one Close and one typed terminal error are produced.

On timeout, the server rejects further application data, attempts one
configured Close within `close_timeout`, and completes local cleanup even when
the peer is unreachable. Code 4201 can be observed on a writable path, but no
implementation can guarantee delivery through an already-dead TCP path.

These semantics are shared by Tokio and Compio, plain and
permessage-deflate, unified and split APIs. Generic TCP/TLS and HTTP/2/HTTP/3
streams inherit the state machine from their runtime WebSocket stream.

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
| `compio-runtime` | ❌ | Native Compio runtime support with a portable polling driver |
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

Rustls features do not select a crypto provider. Applications must enable
exactly one provider, such as `aws-lc-rs` or `ring`, on their direct `rustls`
dependency.

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

Transport features are runtime-neutral. Pair `http2` or `http3` with either `tokio-runtime` or `compio-runtime`.

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

| Architecture | Instructions | Masking | UTF-8 backend | Stable | Nightly |
|--------------|--------------|---------|---------------|--------|---------|
| x86_64 | SSE2 | ✅ | Portable fallback | ✅ | ✅ |
| x86_64 | SSE4.2 | ✅ | SSE4.2 | ✅ | ✅ |
| x86_64 | AVX2 | ✅ | AVX2 | ✅ | ✅ |
| x86_64 | AVX-512 | ✅ | AVX2 | ✅ | ✅ |
| aarch64 | NEON | ✅ | NEON | ✅ | ✅ |
| arm | NEON | ✅ | Portable fallback | ❌ | ✅ |
| loongarch64 | LSX | ✅ | Portable fallback | ❌ | ✅ |
| loongarch64 | LASX | ✅ | Portable fallback | ❌ | ✅ |
| powerpc | AltiVec | ✅ | Portable fallback | ❌ | ✅ |
| powerpc64 | AltiVec | ✅ | Portable fallback | ❌ | ✅ |
| s390x | z13 vectors | ✅ | Portable fallback | ❌ | ✅ |

The UTF-8 backend column describes acceleration, not validation availability.
Portable fallback uses the dependency's standard validator, so all targets validate complete UTF-8 inputs.
The Stable and Nightly columns describe availability of the listed masking implementation.

UTF-8 validation uses:
- [simdutf8](https://github.com/rusticstuff/simdutf8) for x86/x86_64 (SSE4.2 or AVX2), aarch64 (NEON), and SIMD-enabled wasm32
- The dependency's standard UTF-8 validator fallback on other targets, including arm, LoongArch64, PowerPC, and s390x

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

// SplitReader - owns decoding and reports shared terminal state
reader.next().await  // Receive message
reader.is_closed()   // Check if closed (non-blocking)

// SplitWriter - bounded command handle to the connection writer driver
writer.send(msg).await?;
writer.send_text("hello").await?;
writer.send_binary(bytes).await?;
writer.close(1000, "bye").await?;
writer.is_closed()   // Check if closed (non-blocking)
writer.flush().await?;  // Flush accepted application writes
```

**Implementation Details:**
- Uses `tokio::io::split()` for OS-level stream splitting
- Reader owns `ReadHalf<S>` and protocol decoder
- One connection-scoped driver exclusively owns `WriteHalf<S>` and the encoder
- Bounded control/application queues provide backpressure under Ping floods
- Pong and Close responses progress without later application `send()`/`flush()`
- Dropping either half cancels the driver; EOF/Close/timeout propagates to both
- No lock is held across an await

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

### Unit and Integration Tests

```bash
cargo nextest run
cargo test --doc
```

Install [`cargo-nextest`](https://nexte.st/docs/installation/) before using these commands. Nextest runs each test as a separate process and reports parameterized cases independently; doctests remain a separate `cargo test --doc` target because nextest does not execute them.

Nextest can run tests from different binaries concurrently, so integration tests must bind OS-assigned ports instead of fixed ports.

### With Features

```bash
cargo nextest run --features http2
cargo nextest run --features http3
cargo nextest run --features full
cargo nextest run --all-features
cargo nextest run --no-default-features --features tokio-runtime,http2,http3
cargo nextest run --no-default-features --features compio-runtime,http2,http3
```

### End-to-End Transport Tests

These tests bind real loopback TCP/QUIC endpoints and use the public runtime APIs for HTTP/2 and HTTP/3 WebSocket handshakes.

```bash
cargo nextest run --all-features --test e2e_runtime_transports
cargo nextest run --no-default-features --features tokio-runtime,http2,http3 --test e2e_runtime_transports
cargo nextest run --no-default-features --features compio-runtime,http2,http3 --test e2e_runtime_transports
```

### Autobahn Test Suite

The bundled [Rust Autobahn port](autobahn-testsuite-rs/README.md) runs all 517
WebSocket cases against the `sockudo-ws` echo server, including compression, with
the full message counts and no case exclusions. Requires Rust 1.88+ and Python 3
for process management.

```bash
make -C autobahn test
```

The command builds both binaries, waits for the server, runs eight cases at a
time, and stops the server on completion or failure. Reports and logs are saved
in `autobahn/reports/`; open `index.html` for case details. A failing case or close
handshake makes the command fail. Concurrent runs are for conformance checking;
their timings should not be treated as isolated latency measurements.

GitHub Actions runs the same command on pull requests and pushes to `master` or
`main`, and requires it to pass before publishing a release to crates.io. The
`autobahn-reports` artifact retains reports and logs for 14 days, including failed
runs. The Autobahn workflow can also be started manually.

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
cargo run --example http2_echo --features tokio-runtime,http2

# HTTP/3 WebSocket server
cargo run --example http3_echo --features tokio-runtime,http3
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
│   ├── compio.rs         # Native Compio HTTP/1.1, HTTP/2, and HTTP/3 runtime support
│   ├── http2/            # HTTP/2 WebSocket (RFC 8441)
│   │   ├── mod.rs
│   │   └── stream.rs     # Http2Stream wrapper
│   ├── http3/            # HTTP/3 WebSocket (RFC 9220)
│   │   ├── mod.rs
│   │   └── stream.rs     # Http3Stream wrapper
│   └── io_uring/         # Linux io_uring transport
│       ├── mod.rs
│       ├── stream.rs     # UringStream wrapper
│       └── buffer.rs     # Owned buffer pool (not kernel-registered)
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
├── tests/
│   └── e2e_runtime_transports.rs # Tokio and Compio HTTP/2 + HTTP/3 loopback tests
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
