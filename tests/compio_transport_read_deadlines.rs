#![cfg(feature = "compio-runtime")]
#[cfg(feature = "http3")]
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

#[cfg(feature = "http2")]
mod compio_http2_e2e {
    use compio::net::{TcpListener, TcpStream};
    use sockudo_ws::compio::{connect_http2, runtime, serve_http2};
    use sockudo_ws::{Config, Error};
    use std::time::Duration;
    #[compio::test]
    async fn idle_timeout_interrupts_http2_read() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = runtime::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_http2(stream, Config::default(), |ws, _req| async move {
                compio::time::sleep(Duration::from_secs(4)).await;
                drop(ws);
            })
            .await
            .unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let config = Config::builder().auto_ping(false).idle_timeout(1).build();
        let mut ws = connect_http2(
            stream,
            &format!("https://localhost:{}/idle", addr.port()),
            None,
            config,
        )
        .await
        .unwrap();

        let result = compio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("HTTP/2 read did not yield to the idle deadline");
        assert!(
            matches!(result, Some(Err(Error::IdleTimeout))),
            "unexpected read result: {result:?}"
        );

        drop(ws);
        server_task.await.unwrap();
    }
}
#[cfg(feature = "http3")]
mod compio_http3_e2e {
    use sockudo_ws::compio::{CompioHttp3Server, connect_http3, runtime};
    use sockudo_ws::{Config, Error};
    use std::time::Duration;
    async fn server_endpoint(server_tls: rustls::ServerConfig) -> compio::quic::Endpoint {
        compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
            .with_alpn_protocols(&["h3"])
            .bind("127.0.0.1:0")
            .await
            .unwrap()
    }

    #[compio::test]
    async fn idle_timeout_interrupts_http3_read() {
        let (server_tls, client_tls) = crate::h3_support::tls_configs();
        let endpoint = server_endpoint(server_tls).await;
        let addr = endpoint.local_addr().unwrap();
        let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());

        let server_task = runtime::spawn(async move {
            server
                .serve(|ws, _req| async move {
                    compio::time::sleep(Duration::from_secs(4)).await;
                    drop(ws);
                })
                .await
                .unwrap();
        });

        let config = Config::builder().auto_ping(false).idle_timeout(1).build();
        let mut ws = connect_http3(addr, "localhost", "/idle", None, client_tls, config)
            .await
            .unwrap();

        let result = compio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("HTTP/3 read did not yield to the idle deadline");
        assert!(
            matches!(result, Some(Err(Error::IdleTimeout))),
            "unexpected read result: {result:?}"
        );

        drop(ws);
        endpoint.close(compio::quic::VarInt::from_u32(0x100), b"done");
        server_task.await.unwrap();
    }
}
