//! Axum WebSocket Echo Server using sockudo-ws
//!
//! This example shows how to integrate sockudo-ws with Axum by handling
//! the HTTP upgrade manually and then using sockudo-ws for WebSocket frames.
//!
//! Run with: cargo run --example axum_echo

use std::net::SocketAddr;

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{Response, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;

use sockudo_ws::handshake::generate_accept_key;
use sockudo_ws::{Config, Message, WebSocketStream};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route(
            "/",
            get(|| async { "sockudo-ws Echo Server - connect to /ws" }),
        )
        .route("/ws", get(ws_handler));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Axum + sockudo-ws server listening on http://{}", addr);
    println!("Connect to ws://{}/ws", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(req: Request) -> impl IntoResponse {
    // Validate WebSocket upgrade request
    let key = match req.headers().get("sec-websocket-key") {
        Some(k) => k.to_str().unwrap_or("").to_string(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };

    // Generate accept key
    let accept_key = generate_accept_key(&key);

    // Spawn handler for after upgrade
    tokio::spawn(async move {
        // Get the upgraded connection
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                handle_socket(io).await;
            }
            Err(e) => eprintln!("Upgrade error: {}", e),
        }
    });

    // Return 101 Switching Protocols
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(Body::empty())
        .unwrap()
}

async fn handle_socket<S>(stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // Create sockudo-ws WebSocketStream with our config
    let config = Config::builder()
        .max_payload_length(16 * 1024)
        .idle_timeout(60)
        .build();

    let mut ws = WebSocketStream::server(stream, config);

    // Simple echo loop using ws.send()
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                if ws.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Binary(data)) => {
                if ws.send(Message::Binary(data)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {} // Ping/Pong handled automatically
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }
    }
}
