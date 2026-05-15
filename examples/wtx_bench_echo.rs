//! Echo server shaped like the `wtx-bench` WebSocket adapter.
//!
//! It uses raw frame processing so text echoes do not pay an application-level
//! UTF-8 validation cost, and writes header + payload with vectored I/O so
//! large payloads are not copied into a combined output buffer.

use std::io::IoSlice;
use std::net::SocketAddr;

use bytes::{Buf, BytesMut};
use sockudo_ws::frame::encode_frame_header;
use sockudo_ws::handshake::{build_response, generate_accept_key, parse_request};
use sockudo_ws::protocol::Protocol;
use sockudo_ws::{Config, Error, OpCode, RawMessage, Role};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> sockudo_ws::Result<()> {
    let addr: SocketAddr = "0.0.0.0:9000".parse().unwrap();
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        stream.set_nodelay(true).ok();

        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream).await {
                eprintln!("connection error: {err}");
            }
        });
    }
}

async fn handle_connection(mut stream: TcpStream) -> sockudo_ws::Result<()> {
    let mut read_buf = BytesMut::with_capacity(sockudo_ws::RECV_BUFFER_SIZE);

    loop {
        let n = stream.read_buf(&mut read_buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        if let Some((req, consumed)) = parse_request(&read_buf)? {
            let accept_key = generate_accept_key(req.key);
            let response = build_response(&accept_key, None, None);
            stream.write_all(&response).await?;
            read_buf.advance(consumed);
            break;
        }
    }

    let config = Config::default();
    let mut protocol = Protocol::new(Role::Server, config.max_frame_size, config.max_message_size);
    let mut messages = Vec::new();
    let mut header_buf = BytesMut::with_capacity(sockudo_ws::MAX_FRAME_HEADER_SIZE);

    loop {
        protocol.process_raw_into(&mut read_buf, &mut messages)?;

        if messages.is_empty() {
            let n = stream.read_buf(&mut read_buf).await?;
            if n == 0 {
                return Ok(());
            }
            protocol.process_raw_into(&mut read_buf, &mut messages)?;
        }

        for msg in messages.drain(..) {
            match msg {
                RawMessage::Text(payload) => {
                    write_unmasked_frame(
                        &mut stream,
                        &mut header_buf,
                        OpCode::Text,
                        payload.as_ref(),
                    )
                    .await?;
                }
                RawMessage::Binary(payload) => {
                    write_unmasked_frame(
                        &mut stream,
                        &mut header_buf,
                        OpCode::Binary,
                        payload.as_ref(),
                    )
                    .await?;
                }
                RawMessage::Ping(payload) => {
                    write_unmasked_frame(
                        &mut stream,
                        &mut header_buf,
                        OpCode::Pong,
                        payload.as_ref(),
                    )
                    .await?;
                }
                RawMessage::Close(_) => {
                    write_unmasked_frame(&mut stream, &mut header_buf, OpCode::Close, &[]).await?;
                    return Ok(());
                }
                RawMessage::Pong(_) => {}
            }
        }
    }
}

async fn write_unmasked_frame(
    stream: &mut TcpStream,
    header_buf: &mut BytesMut,
    opcode: OpCode,
    payload: &[u8],
) -> sockudo_ws::Result<()> {
    header_buf.clear();
    encode_frame_header(header_buf, opcode, payload.len(), true, None);

    let mut header_written = 0;
    let mut payload_written = 0;

    while header_written < header_buf.len() || payload_written < payload.len() {
        let header = &header_buf[header_written..];
        let body = &payload[payload_written..];
        let slices = [IoSlice::new(header), IoSlice::new(body)];
        let slices = if header.is_empty() {
            &slices[1..]
        } else if body.is_empty() {
            &slices[..1]
        } else {
            &slices[..]
        };

        let written = stream.write_vectored(slices).await?;
        if written == 0 {
            return Err(Error::ConnectionClosed);
        }

        let header_remaining = header_buf.len() - header_written;
        if written < header_remaining {
            header_written += written;
        } else {
            header_written = header_buf.len();
            payload_written += written - header_remaining;
        }
    }

    Ok(())
}
