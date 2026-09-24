#![cfg(all(feature = "compio-runtime", feature = "http3"))]

use std::io;
use std::sync::Once;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use sockudo_ws::compio::{CompioHttp3Server, connect_http3_multiplexed};
use sockudo_ws::{Config, Error, Message};

static INSTALL_CRYPTO: Once = Once::new();

fn tls_configs() -> (rustls::ServerConfig, rustls::ClientConfig) {
    INSTALL_CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });

    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

    let server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .unwrap();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let client = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    (server, client)
}

fn is_cancelled_write(error: &Error) -> bool {
    matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::ConnectionAborted)
}

#[compio::test]
async fn cancelled_client_write_makes_the_http3_stream_terminal() {
    const PAYLOAD_SIZE: usize = 4 * 1024 * 1024;

    let (server_tls, client_tls) = tls_configs();
    let endpoint = compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
        .with_alpn_protocols(&["h3"])
        .bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());
    let (observed_tx, mut observed_rx) = futures_channel::mpsc::unbounded();

    let server_task = compio::runtime::spawn(async move {
        server
            .serve(move |mut ws, request| {
                let mut observed_tx = observed_tx.clone();
                async move {
                    if request.path == "/cancelled-write" {
                        compio::time::sleep(Duration::from_millis(100)).await;
                        let observed =
                            compio::time::timeout(Duration::from_secs(5), ws.next()).await;
                        observed_tx
                            .send(matches!(observed, Ok(Some(Err(_)))))
                            .await
                            .unwrap();
                    } else {
                        assert_eq!(request.path, "/replacement");
                        let message = ws.next().await.unwrap().unwrap();
                        ws.send(message).await.unwrap();
                    }
                }
            })
            .await
            .unwrap();
    });

    let mut connection = connect_http3_multiplexed(
        addr,
        "localhost",
        client_tls,
        Config::builder()
            .max_backpressure(PAYLOAD_SIZE + 14)
            .build(),
    )
    .await
    .unwrap();
    let mut client = connection
        .open_websocket("/cancelled-write", None)
        .await
        .unwrap();

    let cancelled = compio::time::timeout(
        Duration::from_millis(10),
        client.send(Message::binary(vec![0x5a; PAYLOAD_SIZE])),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "large write completed before cancellation"
    );

    let error = compio::time::timeout(Duration::from_secs(5), client.send_text("must-not-send"))
        .await
        .expect("terminal write check timed out")
        .expect_err("cancelled HTTP/3 stream accepted another write");
    assert!(is_cancelled_write(&error));
    assert!(client.is_closed());
    assert!(
        observed_rx
            .next()
            .await
            .expect("server stopped before reporting the reset")
    );

    let mut replacement = connection
        .open_websocket("/replacement", None)
        .await
        .expect("cancelled stream closed the HTTP/3 connection");
    replacement.send_text("still-open").await.unwrap();
    let echoed = replacement.next().await.unwrap().unwrap();
    assert!(matches!(echoed, Message::Text(text) if text == "still-open"));

    connection.close();
    endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
    server_task.await.unwrap();
}

#[compio::test]
async fn cancelled_server_write_makes_the_http3_stream_terminal() {
    const PAYLOAD_SIZE: usize = 4 * 1024 * 1024;

    let (server_tls, client_tls) = tls_configs();
    let endpoint = compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
        .with_alpn_protocols(&["h3"])
        .bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let server = CompioHttp3Server::from_endpoint(
        endpoint.clone(),
        Config::builder()
            .max_backpressure(PAYLOAD_SIZE + 14)
            .build(),
    );
    let (terminal_tx, mut terminal_rx) = futures_channel::mpsc::unbounded();

    let server_task = compio::runtime::spawn(async move {
        server
            .serve(move |mut ws, request| {
                let mut terminal_tx = terminal_tx.clone();
                async move {
                    if request.path == "/cancelled-server-write" {
                        let cancelled = compio::time::timeout(
                            Duration::from_millis(10),
                            ws.send(Message::binary(vec![0xa5; PAYLOAD_SIZE])),
                        )
                        .await;
                        assert!(
                            cancelled.is_err(),
                            "large write completed before cancellation"
                        );
                        let error = ws
                            .send_text("must-not-send")
                            .await
                            .expect_err("cancelled HTTP/3 stream accepted another write");
                        terminal_tx.send(is_cancelled_write(&error)).await.unwrap();
                    } else {
                        assert_eq!(request.path, "/replacement");
                        let message = ws.next().await.unwrap().unwrap();
                        ws.send(message).await.unwrap();
                    }
                }
            })
            .await
            .unwrap();
    });

    let mut connection =
        connect_http3_multiplexed(addr, "localhost", client_tls, Config::default())
            .await
            .unwrap();
    let mut client = connection
        .open_websocket("/cancelled-server-write", None)
        .await
        .unwrap();
    compio::time::sleep(Duration::from_millis(100)).await;

    let observed = compio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("peer reset timed out");
    assert!(matches!(observed, Some(Err(_))));
    assert!(
        terminal_rx
            .next()
            .await
            .expect("server stopped before reporting terminal state")
    );

    let mut replacement = connection
        .open_websocket("/replacement", None)
        .await
        .expect("cancelled stream closed the HTTP/3 connection");
    replacement.send_text("still-open").await.unwrap();
    let echoed = replacement.next().await.unwrap().unwrap();
    assert!(matches!(echoed, Message::Text(text) if text == "still-open"));

    connection.close();
    endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
    server_task.await.unwrap();
}
