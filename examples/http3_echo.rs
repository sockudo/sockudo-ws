//! HTTP/3 WebSocket Echo Server Example
//!
//! This example demonstrates running a WebSocket server over HTTP/3/QUIC
//! using the Extended CONNECT protocol (RFC 9220).
//!
//! # Running
//!
//! ```bash
//! cargo run --example http3_echo --features http3
//! ```
//!
//! Note: QUIC requires TLS certificates. Generate self-signed certs for testing:
//! ```bash
//! openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes
//! ```

use std::fs;

use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sockudo_ws::{Config, Http3, Message, WebSocketServer};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("HTTP/3 WebSocket Echo Server");
    println!("============================");
    println!();
    println!("This example shows HTTP/3 WebSocket over QUIC (RFC 9220).");
    println!();

    // Load TLS certificates
    // For testing, generate with:
    // openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes
    let (certs, key) = load_certs_and_key()?;

    // Build rustls server config
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    // Enable ALPN for HTTP/3
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    let ws_config = Config::builder()
        .max_payload_length(64 * 1024)
        .http3_idle_timeout(30_000)
        .build();

    let server =
        WebSocketServer::<Http3>::bind("127.0.0.1:4433".parse()?, tls_config, ws_config).await?;

    println!("Listening on: {}", server.local_addr()?);
    println!();
    println!("Benefits of HTTP/3 WebSocket:");
    println!("  - No head-of-line blocking");
    println!("  - 0-RTT connection resumption");
    println!("  - Better mobile performance");
    println!("  - Multiple WebSocket streams per connection");
    println!();

    server
        .serve(|mut ws, req| async move {
            println!(
                "HTTP/3 WebSocket connection to: {} (protocol: {:?})",
                req.path, req.protocol
            );

            // Echo loop - exact same API as HTTP/1.1 and HTTP/2!
            while let Some(msg) = ws.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        println!(
                            "Received text: {}",
                            std::str::from_utf8(&text).unwrap_or("<invalid utf8>")
                        );
                        if ws.send(Message::Text(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        println!("Received binary: {} bytes", data.len());
                        if ws.send(Message::Binary(data)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(reason)) => {
                        println!("Received close: {:?}", reason);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        break;
                    }
                }
            }

            println!("WebSocket connection closed");
        })
        .await?;

    Ok(())
}

fn load_certs_and_key()
-> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Box<dyn std::error::Error>> {
    // Try to load from files, or generate self-signed for demo
    let cert_path = "cert.pem";
    let key_path = "key.pem";

    if std::path::Path::new(cert_path).exists() && std::path::Path::new(key_path).exists() {
        let cert_pem = fs::read(cert_path)?;
        let key_pem = fs::read(key_path)?;

        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
            .filter_map(|c| c.ok())
            .collect();

        let key =
            rustls_pemfile::private_key(&mut key_pem.as_slice())?.ok_or("No private key found")?;

        Ok((certs, key))
    } else {
        // Generate self-signed certificate for demo
        println!("No certificates found. Generate with:");
        println!(
            "  openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -days 365 -nodes"
        );
        println!();

        // Use rcgen to generate self-signed cert
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())?;

        Ok((vec![cert_der], key_der))
    }
}
