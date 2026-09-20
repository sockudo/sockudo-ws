//! Standalone WebSocket server for testing and benchmarking
//!
//! This server implements a simple echo protocol for Autobahn testing.

use std::net::SocketAddr;

use bytes::BytesMut;
use socket2::{Domain, Protocol as SockProtocol, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use sockudo_ws::Config;
use sockudo_ws::error::Result;
use sockudo_ws::handshake::{build_response, generate_accept_key, parse_request};
use sockudo_ws::protocol::{Message, Protocol, Role};

#[cfg(feature = "permessage-deflate")]
use sockudo_ws::deflate::{DeflateConfig, parse_deflate_offer};
#[cfg(feature = "permessage-deflate")]
use sockudo_ws::protocol::CompressedProtocol;

const DEBUG: bool = false;

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = "127.0.0.1:9001".parse().unwrap();

    // Create socket with SO_REUSEPORT for kernel load balancing
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(SockProtocol::TCP))
        .expect("failed to create socket");

    socket.set_reuse_address(true).expect("set_reuse_address");
    #[cfg(unix)]
    socket.set_reuse_port(true).expect("set_reuse_port");
    socket.set_nonblocking(true).expect("set_nonblocking");
    socket.bind(&addr.into()).expect("bind");
    socket.listen(1024).expect("listen");

    let listener = TcpListener::from_std(socket.into()).expect("from_std");

    println!("WebSocket server listening on ws://{}", addr);
    println!("Ready for Autobahn test suite");

    loop {
        let (stream, _peer) = listener.accept().await?;

        // TCP_NODELAY - disable Nagle's algorithm for low latency
        stream.set_nodelay(true).ok();

        tokio::spawn(async move {
            let _ = handle_connection(stream).await;
        });
    }
}

async fn handle_connection(mut stream: TcpStream) -> Result<()> {
    if DEBUG {
        println!("New connection accepted");
    }

    // Read HTTP upgrade request
    let mut buf = BytesMut::with_capacity(4096);
    #[cfg(feature = "permessage-deflate")]
    let mut deflate_config: Option<DeflateConfig> = None;

    loop {
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            if DEBUG {
                println!("Connection closed before handshake");
            }
            return Ok(());
        }

        if DEBUG {
            println!("Read {} bytes for handshake", n);
        }

        if let Some((req, consumed)) = parse_request(&buf)? {
            if DEBUG {
                println!(
                    "Parsed handshake request: path={}, extensions={:?}",
                    req.path, req.extensions
                );
            }
            // Generate accept key
            let accept_key = generate_accept_key(req.key);

            // Check for compression extension
            #[cfg(feature = "permessage-deflate")]
            let extensions_response = if let Some(ext) = req.extensions {
                if DEBUG {
                    println!("Client requested extensions: {}", ext);
                }
                // Parse each extension offer
                let mut found = false;
                for offer in ext.split(',') {
                    if DEBUG {
                        println!("Parsing offer: {}", offer.trim());
                    }
                    if let Some(params) = parse_deflate_offer(offer.trim()) {
                        if DEBUG {
                            println!("Found deflate offer with {} params", params.len());
                        }
                        // Accept the deflate offer with our preferred config
                        if let Ok(config) = DeflateConfig::from_params(&params) {
                            if DEBUG {
                                println!("Accepting compression with config: {:?}", config);
                            }
                            deflate_config = Some(config);
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    let resp = deflate_config.as_ref().map(|c| c.to_response_header());
                    if DEBUG {
                        println!("Sending extension response: {:?}", resp);
                    }
                    resp
                } else {
                    if DEBUG {
                        println!("No compression negotiated");
                    }
                    None
                }
            } else {
                if DEBUG {
                    println!("No extensions requested by client");
                }
                None
            };

            #[cfg(not(feature = "permessage-deflate"))]
            let extensions_response: Option<String> = None;

            // Build and send response
            let response = build_response(&accept_key, None, extensions_response.as_deref());
            stream.write_all(&response).await?;
            stream.flush().await?;

            if DEBUG {
                println!(
                    "WebSocket handshake completed, compression: {}",
                    deflate_config.is_some()
                );
            }

            // Keep any leftover data
            let leftover = buf.split_off(consumed);
            buf = leftover;
            break;
        }
    }

    // Handle WebSocket frames
    let config = Config::default();

    #[cfg(feature = "permessage-deflate")]
    {
        if let Some(deflate_cfg) = deflate_config {
            // Use compressed protocol
            return handle_connection_compressed(stream, buf, config, deflate_cfg).await;
        }
    }

    // Non-compressed path
    handle_connection_plain(stream, buf, config).await
}

async fn handle_connection_plain(
    mut stream: TcpStream,
    mut buf: BytesMut,
    config: Config,
) -> Result<()> {
    let mut protocol = Protocol::new(Role::Server, config.max_frame_size, config.max_message_size);
    let mut write_buf = BytesMut::with_capacity(128 * 1024);
    let mut messages = Vec::with_capacity(8);

    loop {
        // Process frames one at a time, flushing responses between frames
        // This ensures we send responses for valid frames before failing on invalid ones
        let process_result = protocol.process_into(&mut buf, &mut messages);

        // Handle any successfully parsed messages first
        for msg in messages.drain(..) {
            match msg {
                Message::Text(text) => {
                    if DEBUG {
                        println!("Echoing text message: {} bytes", text.len());
                    }
                    protocol.encode_message(&Message::Text(text), &mut write_buf)?;
                    // Flush after each message to ensure it's sent before potential errors
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                        write_buf.clear();
                    }
                }
                Message::Binary(data) => {
                    if DEBUG {
                        println!("Echoing binary message: {} bytes", data.len());
                    }
                    protocol.encode_message(&Message::Binary(data), &mut write_buf)?;
                    // Flush after each message
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                        write_buf.clear();
                    }
                }
                Message::Ping(data) => {
                    protocol.encode_pong(&data, &mut write_buf);
                }
                Message::Pong(_) => {}
                Message::Close(reason) => {
                    protocol.encode_message(&Message::Close(reason), &mut write_buf)?;
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                    }
                    stream.shutdown().await?;
                    return Ok(());
                }
            }
        }

        // Now check if processing had an error
        process_result?;

        if !write_buf.is_empty() {
            stream.write_all(&write_buf).await?;
            stream.flush().await?;
            write_buf.clear();
        }

        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
    }
}

#[cfg(feature = "permessage-deflate")]
async fn handle_connection_compressed(
    mut stream: TcpStream,
    mut buf: BytesMut,
    config: Config,
    deflate_config: DeflateConfig,
) -> Result<()> {
    let mut protocol = CompressedProtocol::server(
        config.max_frame_size,
        config.max_message_size,
        deflate_config,
    );
    let mut write_buf = BytesMut::with_capacity(128 * 1024);
    let mut messages = Vec::with_capacity(8);

    loop {
        if DEBUG {
            println!("Processing buffer with {} bytes", buf.len());
        }

        // Process frames, capturing result to check after handling valid messages
        let process_result = protocol.process_into(&mut buf, &mut messages);

        if DEBUG {
            println!("Got {} messages", messages.len());
        }

        for msg in messages.drain(..) {
            match msg {
                Message::Text(text) => {
                    if DEBUG {
                        println!("Echoing compressed text message: {} bytes", text.len());
                    }
                    protocol.encode_message(&Message::Text(text), &mut write_buf)?;
                    // Flush after each message to ensure it's sent before potential errors
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                        write_buf.clear();
                    }
                }
                Message::Binary(data) => {
                    if DEBUG {
                        println!("Echoing compressed binary message: {} bytes", data.len());
                    }
                    protocol.encode_message(&Message::Binary(data), &mut write_buf)?;
                    // Flush after each message
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                        write_buf.clear();
                    }
                }
                Message::Ping(data) => {
                    if DEBUG {
                        println!("Received ping, sending pong");
                    }
                    protocol.encode_pong(&data, &mut write_buf);
                }
                Message::Pong(_) => {
                    if DEBUG {
                        println!("Received pong");
                    }
                }
                Message::Close(reason) => {
                    if DEBUG {
                        println!("Received close frame, reason: {:?}", reason);
                    }
                    // Close frames are not compressed
                    protocol.encode_pong(&[], &mut write_buf); // dummy to get inner
                    write_buf.clear();
                    // Encode close manually without compression
                    use bytes::Bytes;
                    use sockudo_ws::frame::{OpCode, encode_frame};
                    let payload = if let Some(ref r) = reason {
                        let mut p = BytesMut::with_capacity(2 + r.reason.len());
                        p.extend_from_slice(&r.code.to_be_bytes());
                        p.extend_from_slice(r.reason.as_bytes());
                        p.freeze()
                    } else {
                        Bytes::new()
                    };
                    encode_frame(&mut write_buf, OpCode::Close, &payload, true, None);
                    if DEBUG {
                        println!("Sending close frame response");
                    }
                    if !write_buf.is_empty() {
                        stream.write_all(&write_buf).await?;
                        stream.flush().await?;
                    }
                    if DEBUG {
                        println!("Shutting down connection");
                    }
                    stream.shutdown().await?;
                    return Ok(());
                }
            }
        }

        // Now check if processing had an error
        process_result?;

        if !write_buf.is_empty() {
            if DEBUG {
                println!("Flushing {} bytes to client", write_buf.len());
            }
            stream.write_all(&write_buf).await?;
            stream.flush().await?;
            write_buf.clear();
        }

        if DEBUG {
            println!("Waiting for more data...");
        }
        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            if DEBUG {
                println!("Connection closed by client");
            }
            return Ok(());
        }
        if DEBUG {
            println!("Read {} more bytes", n);
        }
    }
}
