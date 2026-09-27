#[cfg(all(feature = "tokio-runtime", feature = "http3"))]
mod h3_support {
    use std::sync::Once;

    static INSTALL_CRYPTO: Once = Once::new();

    pub fn tls_configs() -> (rustls::ServerConfig, rustls::ClientConfig) {
        INSTALL_CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();

        let mut server_tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .unwrap();
        server_tls.alpn_protocols = vec![b"h3".to_vec()];

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut client_tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];

        (server_tls, client_tls)
    }
}

#[cfg(all(feature = "tokio-runtime", feature = "http3"))]
mod writes {
    use futures_util::{SinkExt, StreamExt};
    use sockudo_ws::{Config, Http3, Message, WebSocketClient, WebSocketServer};
    use std::{sync::Arc, time::Duration};
    fn server_endpoint(server_tls: rustls::ServerConfig) -> quinn::Endpoint {
        let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap();
        let server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn client_write_survives_http3_flow_control_backpressure() {
        const PAYLOAD_SIZE: usize = 4 * 1024 * 1024;

        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls);
        let addr = endpoint.local_addr().unwrap();
        let server = WebSocketServer::<Http3>::from_endpoint(endpoint.clone(), Config::default());
        let (received_tx, mut received_rx) = tokio::sync::mpsc::unbounded_channel();

        let server_task = tokio::spawn(async move {
            server
                .serve(move |mut ws, _| {
                    let received_tx = received_tx.clone();
                    async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let message = ws.next().await.unwrap().unwrap();
                        assert!(
                            matches!(message, Message::Binary(data) if data.len() == PAYLOAD_SIZE)
                        );
                        received_tx.send(()).unwrap();
                    }
                })
                .await
                .unwrap();
        });

        // The frame must reach HTTP/3 flow control, including its wire header.
        let client = WebSocketClient::<Http3>::new(
            Config::builder()
                .max_backpressure(PAYLOAD_SIZE + 14)
                .build(),
        );
        let mut ws = client
            .connect(addr, "localhost", "/client-backpressure", client_tls)
            .await
            .unwrap();

        tokio::time::timeout(
            Duration::from_secs(5),
            ws.send(Message::binary(vec![0x5a; PAYLOAD_SIZE])),
        )
        .await
        .expect("client write timed out")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), received_rx.recv())
            .await
            .expect("server read timed out")
            .expect("server stopped before receiving the message");

        drop(ws);
        endpoint.close(quinn::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn server_write_survives_http3_flow_control_backpressure() {
        const PAYLOAD_SIZE: usize = 4 * 1024 * 1024;

        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls);
        let addr = endpoint.local_addr().unwrap();
        // The frame must reach HTTP/3 flow control, including its wire header.
        let server = WebSocketServer::<Http3>::from_endpoint(
            endpoint.clone(),
            Config::builder()
                .max_backpressure(PAYLOAD_SIZE + 14)
                .build(),
        );
        let (sent_tx, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();

        let server_task = tokio::spawn(async move {
            server
                .serve(move |mut ws, _| {
                    let sent_tx = sent_tx.clone();
                    async move {
                        let result = ws
                            .send(Message::binary(vec![0xa5; PAYLOAD_SIZE]))
                            .await
                            .map_err(|error| error.to_string());
                        sent_tx.send(result).unwrap();
                    }
                })
                .await
                .unwrap();
        });

        let client = WebSocketClient::<Http3>::new(Config::default());
        let mut ws = client
            .connect(addr, "localhost", "/server-backpressure", client_tls)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("client read timed out")
            .expect("server stopped before sending the message")
            .unwrap();
        assert!(matches!(message, Message::Binary(data) if data.len() == PAYLOAD_SIZE));
        tokio::time::timeout(Duration::from_secs(5), sent_rx.recv())
            .await
            .expect("server write timed out")
            .expect("server stopped before reporting the write result")
            .unwrap();

        drop(ws);
        endpoint.close(quinn::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }
}
