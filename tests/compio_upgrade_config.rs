#![cfg(all(feature = "compio-runtime", feature = "http3"))]
#[cfg(feature = "http3")]
mod h3_support {
    use std::sync::Once;

    static INSTALL_CRYPTO: Once = Once::new();

    pub fn tls_configs() -> (rustls::ServerConfig, rustls::ClientConfig) {
        INSTALL_CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();

        let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();

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

mod negotiation {
    use sockudo_ws::compio::{CompioHttp3Server, connect_http3, runtime};
    use sockudo_ws::{Config, Message};
    async fn server_endpoint(server_tls: rustls::ServerConfig) -> compio::quic::Endpoint {
        compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
            .with_alpn_protocols(&["h3"])
            .bind("127.0.0.1:0")
            .await
            .unwrap()
    }

    #[compio::test]
    async fn websocket_over_http3_quic_e2e() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls).await;
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default())
            .protocols(["superchat", "chat"])
            .unwrap();

        let server_task = runtime::spawn(async move {
            server
                .serve(|mut ws, req| async move {
                    assert_eq!(req.path, "/compio-h3");
                    assert_eq!(req.selected_subprotocol.as_deref(), Some("superchat"));
                    let msg = ws.next().await.unwrap().unwrap();
                    assert!(matches!(&msg, Message::Text(text) if text == "compio-h3"));
                    ws.send(msg).await.unwrap();
                })
                .await
                .unwrap();
        });

        let mut ws = connect_http3(
            addr,
            "localhost",
            "/compio-h3",
            Some("chat, superchat"),
            client_tls,
            Config::default(),
        )
        .await
        .unwrap();

        ws.send_text("compio-h3").await.unwrap();
        let echoed = ws.next().await.unwrap().unwrap();
        assert!(matches!(echoed, Message::Text(text) if text == "compio-h3"));

        drop(ws);
        endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }
}
